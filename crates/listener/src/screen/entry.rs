use super::*;

/// Gate outcome for one observation round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Quorum reached: open with this distinct-buyer count as velocity.
    Confirm {
        /// Distinct non-dev buyers observed.
        buyers: u32,
    },
    /// Never open: dev dump, timeout, or (via sweep) stale entry.
    Abort {
        /// Human-readable cause for logs.
        reason: String,
    },
    /// Keep watching.
    Wait,
}

/// Pending watchlist entry.
#[derive(Debug)]
struct PendingGate {
    /// Watched insider wallets: the launcher plus, on v2 launches, the
    /// distinct creator (see [`EntryGate::add_cohort`]). Buys from any of
    /// these never count toward quorum; sells/unknowns from any abort.
    /// Bounded ([`MAX_COHORT_WALLETS`]) so entries stay small.
    devs: Vec<Pubkey>,
    deadline_secs: u64,
    buyers: HashSet<Pubkey>,
    /// First-submit time: caps relaunch refreshes so spam cannot extend
    /// the window forever (see [`EntryGate::submit`]).
    created_secs: u64,
}

/// Time-windowed buyer-quorum state machine. Pure except for the clock
/// the caller passes in — fully unit-testable.
///
/// Deadlines are caller wall-clock (`unix_now` at the call sites): an NTP
/// backward jump can only expire entries early (fail-closed abort, never
/// a false confirm); forward jumps stretch the window, capped at
/// `created + 3×window` per entry plus the map cap below.
#[derive(Debug, Default)]
pub struct EntryGate {
    window_secs: u64,
    min_buyers: u32,
    pending: HashMap<String, PendingGate>,
}

/// Watchlist cap: mint spam must not grow memory without bound when
/// timeout sweeps stall. The entry with the nearest deadline is evicted
/// (loudly) to make room.
pub const MAX_PENDING_ENTRIES: usize = 10_000;

/// Cohort cap per watchlist entry: launcher + creator fit with margin;
/// more wallets cannot join one entry (each is a distinct insider to
/// watch, not a quorum voter).
pub const MAX_COHORT_WALLETS: usize = 4;

impl EntryGate {
    /// New gate: entries get `window_secs` to gather `min_buyers`.
    pub fn new(window_secs: u64, min_buyers: u32) -> Self {
        Self {
            window_secs,
            min_buyers: min_buyers.max(1),
            pending: HashMap::new(),
        }
    }

    /// Watchlist size (for metrics).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// True when a mint currently has a pending entry.
    pub fn contains(&self, mint: &str) -> bool {
        self.pending.contains_key(mint)
    }

    /// Submit a fresh signal (idempotent per mint). Returns false when
    /// the dev address is unparsable (fail-closed: caller must skip).
    ///
    /// Relaunch refreshes the window but buyers carry over (demand already
    /// observed stays valid). The refresh is capped at `created + 3×window`
    /// so a spammed relaunch cannot extend the watchlist entry forever.
    /// A relaunch resets the insider cohort to the new dev alone — use
    /// [`EntryGate::add_cohort`] after each submit for v2 creators.
    pub fn submit(&mut self, mint: &str, dev: &str, now_secs: u64) -> bool {
        let Ok(dev) = Pubkey::from_str(dev) else {
            return false;
        };
        let deadline_secs = now_secs.saturating_add(self.window_secs);
        match self.pending.get_mut(mint) {
            Some(entry) => {
                entry.devs = vec![dev];
                let cap = entry.created_secs.saturating_add(self.window_secs.saturating_mul(3));
                entry.deadline_secs = deadline_secs.min(cap);
            }
            None => {
                if self.pending.len() >= MAX_PENDING_ENTRIES
                    && let Some(oldest) = self
                        .pending
                        .iter()
                        .min_by_key(|(_, entry)| entry.deadline_secs)
                        .map(|(mint, _)| mint.clone())
                {
                    warn!(mint = %oldest, "watchlist full; evicting nearest-deadline entry");
                    self.pending.remove(&oldest);
                }
                self.pending.insert(
                    mint.to_string(),
                    PendingGate {
                        devs: vec![dev],
                        deadline_secs,
                        buyers: HashSet::new(),
                        created_secs: now_secs,
                    },
                );
            }
        }
        true
    }

    /// Watch one more insider wallet on an existing entry (v2 creator
    /// cohort). Returns false when the wallet is unparsable, the entry
    /// is missing, already listed, or the cohort is full — fail-closed:
    /// the caller must drop the signal rather than watch half the
    /// insiders. Never resets deadlines or buyers.
    pub fn add_cohort(&mut self, mint: &str, wallet: &str) -> bool {
        let Ok(wallet) = Pubkey::from_str(wallet) else {
            return false;
        };
        let Some(entry) = self.pending.get_mut(mint) else {
            return false;
        };
        if entry.devs.contains(&wallet) || entry.devs.len() >= MAX_COHORT_WALLETS {
            return false;
        }
        entry.devs.push(wallet);
        true
    }

    /// Abandon a watchlist entry without an outcome (fail-closed unwind
    /// when post-submit setup cannot complete). True when an entry was
    /// removed.
    pub fn remove(&mut self, mint: &str) -> bool {
        self.pending.remove(mint).is_some()
    }

    /// Fold one observation round into the entry. Removes the entry on
    /// terminal outcomes. Unknown mints report [`GateOutcome::Wait`].
    ///
    /// Best-effort dev-dump detection: a `Sell` (any `SwapSide::Sell` —
    /// legacy/v2/v3, all layout-verified) authored by any watched
    /// insider wallet (launcher cohort) aborts here. Dumps routed through
    /// transfers or funded sybils do not abort on their own — cluster
    /// forensics in [`BundleDetector`] covers coordinated funding instead.
    pub fn observe(&mut self, mint: &str, swaps: &[SwapRecord], now_secs: u64) -> GateOutcome {
        let Some(entry) = self.pending.get_mut(mint) else {
            return GateOutcome::Wait;
        };
        for swap in swaps {
            match swap.side {
                SwapSide::Buy => {
                    if !entry.devs.contains(&swap.wallet) {
                        entry.buyers.insert(swap.wallet);
                    }
                }
                SwapSide::Sell => {
                    if entry.devs.contains(&swap.wallet) {
                        let reason = format!("dev dump by {} in {}", swap.wallet, swap.signature);
                        self.pending.remove(mint);
                        return GateOutcome::Abort { reason };
                    }
                }
                SwapSide::Unknown => {
                    // Fail closed on the residual blindness: an unmapped
                    // pump ix naming a watched insider smells like a dump
                    // through a layout we cannot parse yet. Stranger
                    // unknowns never count toward quorum and never abort.
                    if entry.devs.contains(&swap.wallet) {
                        let reason = format!(
                            "possible dev dump via unknown pump ix by {} in {}",
                            swap.wallet, swap.signature
                        );
                        self.pending.remove(mint);
                        return GateOutcome::Abort { reason };
                    }
                }
            }
        }
        let buyers = entry.buyers.len();
        if buyers >= self.min_buyers as usize {
            self.pending.remove(mint);
            return GateOutcome::Confirm {
                buyers: buyers as u32,
            };
        }
        if now_secs >= entry.deadline_secs {
            let reason = format!("timeout: only {buyers} distinct buyers in window");
            self.pending.remove(mint);
            return GateOutcome::Abort { reason };
        }
        GateOutcome::Wait
    }

    /// Abort every entry past its deadline. Returns `(mint, outcome)` pairs.
    pub fn sweep_timeouts(&mut self, now_secs: u64) -> Vec<(String, GateOutcome)> {
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, entry)| now_secs >= entry.deadline_secs)
            .map(|(mint, _)| mint.clone())
            .collect();
        stale
            .into_iter()
            .map(|mint| {
                let buyers = self
                    .pending
                    .remove(&mint)
                    .map(|e| e.buyers.len())
                    .unwrap_or(0);
                (
                    mint,
                    GateOutcome::Abort {
                        reason: format!("timeout: only {buyers} distinct buyers in window"),
                    },
                )
            })
            .collect()
    }
}

/// Developer history policy.
///
/// Coverage limitation (by design, RPC-bounded): wallet age is measured
/// over the full `sig_limit` signature window, but the prior-launch
/// count is sampled from only the newest `sample_txs` transactions.
/// A serial launcher whose launches sit deeper than `sample_txs` in an
/// otherwise old wallet passes the count — the age gate, audit score,
/// quorum, and bundle forensics downstream still apply. Raising
/// `sample_txs` widens coverage at direct RPC cost.
#[derive(Debug, Clone)]
pub struct DevScreenConfig {
    /// Minimum wallet age in seconds (default 3600: reject fresh wallets).
    pub min_age_secs: u64,
    /// Maximum tolerated prior Pump.fun launches (default 1).
    pub max_prior_launches: usize,
    /// Recent transactions sampled for the launch count (default 20).
    pub sample_txs: usize,
    /// Signature window fetched per wallet (default 100).
    pub sig_limit: usize,
}

impl Default for DevScreenConfig {
    fn default() -> Self {
        Self {
            min_age_secs: 3600,
            max_prior_launches: 1,
            sample_txs: 20,
            sig_limit: 100,
        }
    }
}

/// Screening verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevVerdict {
    /// True when the wallet may be trusted with a position.
    pub pass: bool,
    /// Wallet age in seconds (`None` = no history at all).
    pub age_secs: Option<u64>,
    /// Pump.fun launches authored in the sampled window.
    pub prior_launches: usize,
    /// Human-readable cause for logs.
    pub reason: String,
}

/// Pure policy over measured history. `age_secs=None` always fails
/// (fail-closed: no history, no trust).
pub fn judge_developer(
    age_secs: Option<u64>,
    prior_launches: usize,
    cfg: &DevScreenConfig,
) -> DevVerdict {
    let Some(age) = age_secs else {
        return DevVerdict {
            pass: false,
            age_secs: None,
            prior_launches,
            reason: "no on-chain history (fresh wallet)".to_string(),
        };
    };
    if age < cfg.min_age_secs {
        return DevVerdict {
            pass: false,
            age_secs: Some(age),
            prior_launches,
            reason: format!("wallet age {age}s below minimum {}", cfg.min_age_secs),
        };
    }
    if prior_launches > cfg.max_prior_launches {
        return DevVerdict {
            pass: false,
            age_secs: Some(age),
            prior_launches,
            reason: format!(
                "serial launcher: {prior_launches} prior launches (max {})",
                cfg.max_prior_launches
            ),
        };
    }
    DevVerdict {
        pass: true,
        age_secs: Some(age),
        prior_launches,
        reason: "history acceptable".to_string(),
    }
}

/// Poll interval floor for gate rounds (not a tight loop: RPC budget).
pub const GATE_POLL_INTERVAL: Duration = Duration::from_millis(2000);
