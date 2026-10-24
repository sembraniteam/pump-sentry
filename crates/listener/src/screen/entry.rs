use super::*;

/// Gate outcome for one round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Quorum reached. Count is distinct buyers.
    Confirm {
        /// Distinct non-dev buyers observed.
        buyers: u32,
    },
    /// Never open: dev dump, timeout, or stale entry.
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
    /// Watched insiders (launcher plus v2 creator). Bounded.
    /// Their buys never count. Their sells or unknowns abort.
    devs: Vec<Pubkey>,
    deadline_secs: u64,
    buyers: HashSet<Pubkey>,
    /// First-submit time. Caps relaunch refresh.
    created_secs: u64,
}

/// Buyer-quorum state machine. Pure except for the caller clock.
#[derive(Debug, Default)]
pub struct EntryGate {
    window_secs: u64,
    min_buyers: u32,
    pending: HashMap<String, PendingGate>,
}

/// Watchlist cap. Evicts nearest deadline when full.
pub const MAX_PENDING_ENTRIES: usize = 10_000;

/// One `/watchlist` line: a mint under buyer-quorum evaluation with
/// its progress. Owned data (no lifetime gaggle) so it crosses into
/// the bot's status payload untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryGateView {
    pub mint: String,
    /// Distinct non-dev buyers observed so far.
    pub buyers: u32,
    /// First submit (relaunch refresh keeps this).
    pub created_secs: u64,
    /// Quorum deadline.
    pub deadline_secs: u64,
}

/// Cohort cap per entry.
pub const MAX_COHORT_WALLETS: usize = 4;

impl EntryGate {
    /// New gate. Entries get `window_secs` to gather buyers.
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

    /// Operator-facing watchlist snapshot (`/watchlist`): one line per
    /// pending mint with its quorum progress. Pure read; no custody,
    /// no secrets (mints are public chain data).
    pub fn pending_view(&self) -> Vec<EntryGateView> {
        self.pending
            .iter()
            .map(|(mint, entry)| EntryGateView {
                mint: mint.clone(),
                buyers: entry.buyers.len() as u32,
                created_secs: entry.created_secs,
                deadline_secs: entry.deadline_secs,
            })
            .collect()
    }

    /// True when a mint currently has a pending entry.
    pub fn contains(&self, mint: &str) -> bool {
        self.pending.contains_key(mint)
    }

    /// Submit a signal (idempotent per mint). False on bad dev address.
    ///
    /// Relaunch refreshes the window but keeps buyers.
    /// Refresh caps at `created + 3xwindow`.
    pub fn submit(&mut self, mint: &str, dev: &str, now_secs: u64) -> bool {
        let Ok(dev) = Pubkey::from_str(dev) else {
            return false;
        };
        let deadline_secs = now_secs.saturating_add(self.window_secs);
        match self.pending.get_mut(mint) {
            Some(entry) => {
                // Preserve the cohort (v2 creator): relaunch refreshes the
                // window, never drops a watched insider.
                if !entry.devs.contains(&dev) {
                    entry.devs.insert(0, dev);
                    entry.devs.truncate(MAX_COHORT_WALLETS);
                }
                let cap = entry
                    .created_secs
                    .saturating_add(self.window_secs.saturating_mul(3));
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

    /// Watch one more insider (v2 creator). False when bad or full.
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

    /// Drop an entry. True when removed.
    pub fn remove(&mut self, mint: &str) -> bool {
        self.pending.remove(mint).is_some()
    }

    /// Fold one round into the entry. Removes entry when terminal.
    /// Unknown mints report Wait.
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
                    // Unmapped ix from a watched wallet may be a dump.
                    // Strangers never abort.
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
        // Deadline first: a quorum that arrives after the window is
        // stale signal, not a confirm — entries get `window_secs`,
        // never a poll round more.
        if now_secs >= entry.deadline_secs {
            let reason = format!("timeout: only {buyers} distinct buyers in window");
            self.pending.remove(mint);
            return GateOutcome::Abort { reason };
        }
        if buyers >= self.min_buyers as usize {
            self.pending.remove(mint);
            return GateOutcome::Confirm {
                buyers: buyers as u32,
            };
        }
        GateOutcome::Wait
    }

    /// Abort every entry past deadline.
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

/// Dev history policy.
/// Age covers the walk. Launches count across the whole walked
/// history (AUD-010); `sample_txs` no longer truncates the count.
/// `sig_limit` sizes one signature page of the backward walk.
#[derive(Debug, Clone)]
pub struct DevScreenConfig {
    /// Min wallet age in seconds.
    pub min_age_secs: u64,
    /// Max past launches allowed.
    pub max_prior_launches: usize,
    /// Txs sampled for the launch count.
    ///
    /// Obsolete for the count (AUD-010 removed the slice: a truncated
    /// history fails closed, so the count walks the whole history);
    /// kept parsed for config compatibility. `sig_limit` now bounds
    /// one page of the walk and is kept at or above this value.
    pub sample_txs: usize,
    /// Signature window fetched per wallet.
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
    /// True when the wallet can hold a position.
    pub pass: bool,
    /// Wallet age (`None` means no history).
    pub age_secs: Option<u64>,
    /// Launches seen in the sampled window.
    pub prior_launches: usize,
    /// The launch count is a LOWER BOUND: the walk stopped at the
    /// page budget with older history unread (AUD-010). Truncated
    /// verdicts never pass — this is evidence for the operator log.
    pub history_truncated: bool,
    /// Human-readable cause for logs.
    pub reason: String,
}

/// Policy over measured history. No history fails.
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
            history_truncated: false,
            reason: "no on-chain history (fresh wallet)".to_string(),
        };
    };
    if age < cfg.min_age_secs {
        return DevVerdict {
            pass: false,
            age_secs: Some(age),
            prior_launches,
            history_truncated: false,
            reason: format!("wallet age {age}s below minimum {}", cfg.min_age_secs),
        };
    }
    if prior_launches > cfg.max_prior_launches {
        return DevVerdict {
            pass: false,
            age_secs: Some(age),
            prior_launches,
            history_truncated: false,
            reason: format!(
                "serial launcher: {prior_launches} prior launches (max {}, excluding this launch)",
                cfg.max_prior_launches
            ),
        };
    }
    DevVerdict {
        pass: true,
        age_secs: Some(age),
        prior_launches,
        history_truncated: false,
        reason: "history acceptable".to_string(),
    }
}

/// Gate poll floor (saves RPC budget).
pub const GATE_POLL_INTERVAL: Duration = Duration::from_millis(2000);
