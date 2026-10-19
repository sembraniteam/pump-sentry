//! Entry gate: watchlist, quorum polling, confirmed opens.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use alert::Alerter;
use auditor::RugCheckError;
use curve::{CurveError, PriceTick};
use listener::{
    BundleVerdict, EntryGate, GateOutcome, MicroSignals, PUMP_PROGRAM_ID, ScreenError, TapePoint,
    micro_signals, tape_volume_base_units,
};
use risk_engine::{
    Action, DecisionSource, RiskDecision, SCALP_TP_MIN, StrategyConfig, StrategyMode, TokenMetrics,
    evaluate_metrics_fallback, resolve_strategy,
};
use simulator::Side;
use tracing::{debug, info, warn};

use crate::journal::JournalLine;
use crate::pipeline::{AlertSink, Auditor, CurveRead, LiveSink, Pipeline, ReserveFeed, Screener};
use crate::settings::now_secs;

/// One curve read: `(mint, signal, full-or-truncated swaps or error)`.
type Observation = (
    String,
    PendingSignal,
    Result<CurveRead, ScreenError>,
);

/// Max concurrent RPC per gate phase (slow rounds halve it).
const GATE_FANOUT: usize = 16;
/// Fanout floor.
const GATE_FANOUT_MIN: usize = 1;
/// Slow round in secs halves fanout.
const SLOW_GATE_ROUND_SECS: u64 = 10;


/// Dev history to 0.0–1.0 trust. Scores below the 0.40 fallback
/// cutoff PASS there: the floor must not mask the prior-launch
/// penalty, or serial launchers saturate exactly at the cutoff and
/// stay buy-eligible (see `fallback::RUGCHECK_SCORE_CAP` note for the
/// layered-cap convention).
pub fn dev_reputation_score(
    age_secs: Option<u64>,
    prior_launches: usize,
    min_age_secs: u64,
) -> f64 {
    let age_factor = match (age_secs, min_age_secs) {
        (Some(age), min) if min > 0 => (age as f64 / (min as f64 * 4.0)).min(1.0),
        (Some(_), _) => 1.0,
        (None, _) => 0.0,
    };
    (0.6 + 0.4 * age_factor - 0.05 * prior_launches as f64).clamp(0.0, 1.0)
}

/// Entry price and sizing liquidity from a reserve read, plus paper-only flag.
/// `None` means skip the signal.
pub fn resolve_entry_with_flag(
    tick: Result<PriceTick, CurveError>,
    quote_override: Option<f64>,
    fallback_liquidity: f64,
) -> Option<(f64, f64, bool)> {
    match tick {
        Ok(tick)
            if tick.price_usd.is_finite()
                && tick.price_usd > 0.0
                && tick.liquidity_usd.is_finite()
                && tick.liquidity_usd >= 0.0 =>
        {
            Some((tick.price_usd, tick.liquidity_usd, false))
        }
        Ok(bad) => {
            // Bad print: drop signal, never price garbage.
            warn!(mint = %bad.mint, price = bad.price_usd, liq = bad.liquidity_usd,
                "ignoring degenerate curve tick; dropping signal");
            None
        }
        Err(e) => match quote_override {
            Some(quote) if quote.is_finite() && quote > 0.0 => {
                warn!(error = %ids::strip_error_url(&e.to_string()), "curve read failed; pricing via QUOTE_USD override");
                let liquidity = if fallback_liquidity.is_finite() && fallback_liquidity >= 0.0 {
                    fallback_liquidity
                } else {
                    0.0
                };
                Some((quote, liquidity, true))
            }
            _ => {
                warn!(error = %ids::strip_error_url(&e.to_string()), "curve read failed and no valid override; dropping signal");
                None
            }
        },
    }
}

/// Stop-loss floor for a tier: 2x fee + 4pt margin, negated,
/// clamped to [-100, 0) so absurd tiers can't print an invalid SL.
///
/// Units: `per_side_total_bps` is the PER-SIDE total (venue tier plus
/// extra commission); the function doubles it internally to a
/// round-trip percent (`2 * bps / 100`) plus a 4-point margin.
/// Callers must pass the summed per-side total, never an
/// already-doubled round-trip figure.
pub(crate) fn fee_aware_sl_floor(sl_percent: f64, per_side_total_bps: f64) -> f64 {
    debug_assert!(sl_percent.is_finite(), "sl_percent must be finite");
    debug_assert!(per_side_total_bps.is_finite(), "per-side bps must be finite");
    if !per_side_total_bps.is_finite() {
        warn!(sl = sl_percent, bps = per_side_total_bps,
            "non-finite fee tier; holding the requested stop");
        return sl_percent;
    }
    let floored = sl_percent.min(-(2.0 * per_side_total_bps / 100.0 + 4.0));
    let clamped = floored.clamp(-100.0, 0.0);
    if clamped != floored {
        warn!(sl = sl_percent, bps = per_side_total_bps, "absurd fee tier clamped the stop; check the schedule");
    }
    clamped
}

/// Watchlist size that warns ops (entries die by deadline; the hard
/// cap below only sheds under flood).
const WATCHLIST_WARN_SIZE: usize = 64;

/// Watchlist hard cap: intake sheds the oldest entry past this so a
/// launch flood can't grow memory without bound. Shed entries drop
/// from both the gate and the meta map together (half-watched is
/// worse than none). Fanout stays capped separately (`GATE_FANOUT`).
const WATCHLIST_HARD_CAP: usize = 512;

// TODO(settings, outside gate ownership): alert `0`=off defaults live
// in config/settings. Every fallback/diverged/giveup path here must
// keep its `warn!` so ops still sees it when alerts are silenced.
// Do not gate logs on alert presence: logs are the fallback channel.

/// Launch identity for watchlist intake: everything `on_launch` needs
/// in one value, mirroring `OpenRequest` below. Eight flat params
/// tripped `too_many_arguments`; the struct is the honest fix.
#[derive(Debug, Clone, Copy)]
pub struct LaunchRequest<'a> {
    pub mint: &'a str,
    pub curve: &'a str,
    pub assoc_curve: &'a str,
    pub dev: &'a str,
    pub creator: Option<&'a str>,
    pub name: &'a str,
    pub symbol: &'a str,
    pub slot: u64,
}

/// Confirm-open request: gate signal plus quorum.
#[derive(Debug, Clone, Copy)]
struct OpenRequest<'a> {
    mint: &'a str,
    signal: &'a PendingSignal,
    buyers: u32,
    /// Confirming round's tape volume in base units (6-decimal basis;
    /// spot-priced to USD at the confirm quote).
    tape_base_units: u64,
    /// Confirm-tape microstructure snapshot for the journal.
    micro: MicroSignals,
    allow_live: bool,
    creation_slot_buys: Option<u32>,
    cluster_pct: Option<f64>,
}

/// Priced confirm ready for paper plus live.
#[derive(Debug, Clone, Copy)]
struct TrackedOpen<'a> {
    mint: &'a str,
    curve: &'a str,
    assoc_curve: &'a str,
    entry_quote: f64,
    liquidity_usd: f64,
    profile: &'a risk_engine::RiskProfile,
    /// Profile provenance for the entry card (`model` | `heuristic`).
    risk: &'static str,
    allow_live: bool,
    creator: Option<&'a str>,
    /// Venue tier with the tick. `None` is standard; synthetic forces `None`.
    venue_fee_bps: Option<f64>,
    /// SOL/USD stamped at confirm. Rechecked at use; stale fails closed.
    sol: Option<(f64, u64)>,    trail_percent: f64,
    /// Exact curve reserves off the confirm tick: the paper fill and the
    /// live sizing quote the chain's own math. `None` on synthetic
    /// (override-priced) opens and mocks.
    reserves: Option<simulator::CurveReserves>,
    /// True when the confirm tick read complete (chain truth). Entries
    /// are pre-migration only; the choke refuses these (see below).
    complete: bool,
    /// Confirm-tape microstructure snapshot for the journal.
    micro: MicroSignals,
}

/// Log-safe token text: on-chain name/symbol are attacker-controlled,
/// so strip control chars (log injection) and cap length. Pure.
fn log_token_text(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Watchlist entry context.
#[derive(Debug, Clone)]
pub struct PendingSignal {
    pub curve: String,
    /// Creator, if the launch had one. `None` is paper-only (fails live).
    pub creator: Option<String>,
    pub assoc_curve: String,
    /// Score at signal time. Quorum needs one (fail-closed).
    pub audit_score: Option<u32>,
    /// Last audit try (throttles re-polls).
    pub last_audit_secs: u64,
    pub creation_slot: u64,
    /// Watermark for incremental reads.
    pub last_sig: Option<String>,
    /// Launcher address for confirm-time re-screen. Intake screens for
    /// admission only; confirm re-screens this (plus creator) and takes
    /// the worst trust there — no stored score ever crosses the wait.
    pub dev: String,
    /// Watch-entry intake time. The hard cap sheds the oldest intake
    /// first; audit retries never re-stamp it.
    pub intake_secs: u64,
    /// Previous round's tape point (observe time + buy records) for
    /// microstructure velocity. Updated every observed round; the
    /// confirm reads it, never a re-stamped audit time.
    pub prev_tape: Option<TapePoint>,
}

/// Gate plus signal context under one lock.
pub struct GateState {
    pub gate: EntryGate,
    pub meta: HashMap<String, PendingSignal>,
    /// Poll exclusion: `Some(started_at)` while a round owns the slot;
    /// stale claims may be taken over. One field — never "0 means free".
    pub poll: Option<u64>,
    /// Fanout width this round.
    pub fanout: usize,
}

/// Stale claim age.
pub const STALE_POLL_SECS: u64 = 30;

/// Min spacing between audit retries per mint.
pub const AUDIT_RETRY_SECS: u64 = 3;

/// Day-loss breaker: halts entries past the cap. Exits still work.
#[derive(Debug, Clone, Copy)]
pub struct DailyBreaker {
    /// UTC day bucket (`unix_secs / 86400`) of the accumulated loss.
    pub day: u64,
    /// Realized net PnL accumulated within `day` (negative = losing).
    pub day_net: f64,
    /// Day bucket the halt card already fired for (once-per-day alert).
    pub halt_alerted_day: u64,
}

impl DailyBreaker {
    /// Fresh breaker (today, flat).
    pub fn new(now_secs: u64) -> Self {
        Self { day: now_secs / 86_400, day_net: 0.0, halt_alerted_day: 0 }
    }

    /// Fold one closed trade in (rolls the day first).
    pub fn record(&mut self, net_pnl: f64, now_secs: u64) {
        let day = now_secs / 86_400;
        if day != self.day {
            self.day = day;
            self.day_net = 0.0;
        }
        if net_pnl.is_finite() {
            self.day_net += net_pnl;
        }
    }

    /// True when entries must halt. Rolls stale buckets on read.
    pub fn tripped(&mut self, cap_usd: f64, now_secs: u64) -> bool {
        let day = now_secs / 86_400;
        if day != self.day {
            self.day = day;
            self.day_net = 0.0;
        }
        cap_usd.is_finite() && cap_usd > 0.0 && self.day_net <= -cap_usd
    }

    /// Halt state plus one-shot card: `(tripped, fire_card)`. The card
    /// fires once per UTC day; silence afterwards is the point — one
    /// card, then the heartbeat carries the state until rollover.
    pub fn halt_state(&mut self, cap_usd: f64, now_secs: u64) -> (bool, bool) {
        if !self.tripped(cap_usd, now_secs) {
            return (false, false);
        }
        if self.halt_alerted_day == self.day {
            return (true, false);
        }
        self.halt_alerted_day = self.day;
        (true, true)
    }
}

impl GateState {
    /// Empty watchlist, poll slot free.
    pub fn new(gate: EntryGate) -> Self {
        Self {
            gate,
            meta: HashMap::new(),
            poll: None,
            fanout: GATE_FANOUT,
        }
    }
}

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {

    /// Watchlisted mint count.
    pub async fn pending_count(&self) -> usize {
        self.gate_state.lock().await.gate.pending_count()
    }

    /// Claim the poll slot; false when running. Stale claims are taken over.
    pub async fn try_begin_poll(&self) -> bool {
        let mut state = self.gate_state.lock().await;
        let now = now_secs();
        if let Some(started) = state.poll {
            if now.saturating_sub(started) < STALE_POLL_SECS {
                return false;
            }
            warn!(
                "stale gate-poll claim (age {:?}s); taking over",
                now.saturating_sub(started)
            );
        }
        state.poll = Some(now);
        true
    }

    /// Release the poll slot.
    pub async fn end_poll(&self) {
        let mut state = self.gate_state.lock().await;
        state.poll = None;
    }

    /// Audit, screen, watchlist. Never opens here; skips fail closed.
    pub async fn on_launch(&self, req: &LaunchRequest<'_>) {
        let (mint, dev, slot) = (req.mint, req.dev, req.slot);
        info!(mint = %mint, name = %log_token_text(req.name, 32),
            symbol = %log_token_text(req.symbol, 10), program = %PUMP_PROGRAM_ID,
            slot, "new launch");
        // 1. Audit, fail closed. Retryable watchlists scoreless for retry.
        let now = now_secs();
        let (audit_score, last_audit_secs) = match self.auditor.evaluate_token(mint).await {
            Ok(report) => (Some(report.score), now),
            Err(RugCheckError::SecurityRejection(reason)) => {
                info!(%mint, "audit reject: {reason}");
                return;
            }
            Err(e) if e.is_retryable() => {
                warn!(%mint, error = %e, "audit unresolved (indexing/transport); watchlisting for retry within gate window");
                (None, now)
            }
            Err(e) => {
                warn!(%mint, error = %e, "audit error; skipping (fail-closed)");
                return;
            }
        };

        // 2. Dev history, fail closed. Admission only: the score is
        // NOT stored — confirm re-screens both wallets fresh (worst
        // wins there), so nothing here can go stale across the wait.
        if self.screen_wallet(mint, dev, "dev").await.is_none() {
            return;
        }

        // 2b. v2 creator: screened too, worst wins at confirm.
        let distinct_creator = req.creator.filter(|c| *c != dev);
        if let Some(creator) = distinct_creator
            && self.screen_wallet(mint, creator, "creator").await.is_none()
        {
            return;
        }

        // 3. Watchlist; confirms happen in poll_gates.
        let mut state = self.gate_state.lock().await;
        // Hard cap: shed the oldest intake past the cap (refreshes of a
        // known mint never shed). Gate and meta drop together.
        if !state.meta.contains_key(mint)
            && state.meta.len() >= WATCHLIST_HARD_CAP
            && let Some(oldest) = oldest_watch_entry(&state.meta)
        {
            state.gate.remove(&oldest);
            state.meta.remove(&oldest);
            warn!(mint = %oldest, cap = WATCHLIST_HARD_CAP, "watchlist at hard cap; shedding oldest intake");
        }
        if !state.gate.submit(mint, dev, now_secs()) {
            warn!(%mint, "bad dev address; skipping");
            return;
        }
        if let Some(creator) = distinct_creator
            && !state.gate.add_cohort(mint, creator)
        {
            // Half-watched is worse than none: unwind.
            state.gate.remove(mint);
            warn!(%mint, "creator cohort untrackable; skipping (fail-closed)");
            return;
        }
        state.meta.insert(
            mint.to_string(),
            PendingSignal {
                curve: req.curve.to_string(),
                assoc_curve: req.assoc_curve.to_string(),
                audit_score,
                last_audit_secs,
                creation_slot: slot,
                last_sig: None,
                creator: req.creator.map(|c| c.to_string()),
                dev: dev.to_string(),
                intake_secs: now,
                prev_tape: None,
            },
        );
        match audit_score {
            Some(score) => info!(%mint, score, "watchlisted; awaiting buyer quorum"),
            None => info!(%mint, "watchlisted without audit score; awaiting buyer quorum + audit retry"),
        }
    }

    /// Screen one insider to a trust score. `None` skips (fail-closed).
    async fn screen_wallet(&self, mint: &str, wallet: &str, role: &str) -> Option<f64> {
        match self.screener.screen_dev(wallet).await {
            Ok(verdict) if verdict.pass => {
                info!(%mint, %role, age = ?verdict.age_secs, priors = verdict.prior_launches, "insider screen pass");
                Some(dev_reputation_score(
                    verdict.age_secs,
                    verdict.prior_launches,
                    self.settings.dev_min_age_secs,
                ))
            }
            Ok(verdict) => {
                info!(%mint, %role, reason = %verdict.reason, "insider screen reject");
                None
            }
            Err(e) => {
                warn!(%mint, %role, error = %ids::redact_urls_in_text(&e.to_string()), "insider screen error; skipping (fail-closed)");
                None
            }
        }
    }

    /// Drive every watchlisted mint one round: confirms open, aborts drop.
    pub async fn poll_gates(&self) {
        let round_start = std::time::Instant::now();
        let (mints, fanout): (Vec<(String, PendingSignal)>, usize) = {
            let state = self.gate_state.lock().await;
            let mints = state
                .meta
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            (mints, state.fanout.clamp(GATE_FANOUT_MIN, GATE_FANOUT))
        };
        // Watermark (ops signal only): the hard cap is enforced at
        // intake (`WATCHLIST_HARD_CAP`); entries otherwise die by
        // deadline. Ops tunes RPC headroom here; fanout stays capped.
        if mints.len() > WATCHLIST_WARN_SIZE {
            warn!(watchlist = mints.len(), "watchlist above watermark; RPC burn follows launch rate");
        }
        // Phase 1: concurrent reads.
        use futures_util::stream::{self, StreamExt};
        let observations: Vec<Observation> =
            stream::iter(mints.into_iter().map(|(mint, signal)| async move {
                let observed = self
                    .screener
                    .observe_curve(&mint, &signal.curve, signal.last_sig.as_deref())
                    .await;
                (mint, signal, observed)
            }))
            .buffered(fanout.max(1))
            .collect()
            .await;
        // Phase 2: gate machine.
        enum GateStep {
            // Boxed: the signal owns five strings; an inline variant
            // would bloat every step of this transient enum.
            Confirmed {
                buyers: u32,
                tape: u64,
                signal: Box<PendingSignal>,
                micro: MicroSignals,
            },
            Aborted(String),
            Waiting,
            Stale,
        }
        let mut confirmed: Vec<(String, Box<PendingSignal>, u32, u64, MicroSignals)> = Vec::new();
        for (mint, mut signal, observed) in observations {
            let (swaps, newest) = match observed {
                Ok(CurveRead::Full { swaps, watermark }) => (swaps, watermark),
                Ok(CurveRead::Truncated) => {
                    // Partial view: never confirm.
                    debug!(%mint, "curve observation truncated; holding watch without quorum");
                    continue;
                }
                Err(e) => {
                    warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "curve observation failed; keeping watch");
                    continue;
                }
            };
            // `None` score is still indexing. Throttled retry; deadline aborts.
            let audit_at_snapshot = signal.audit_score;
            let mut audit_changed = false;
            if signal.audit_score.is_none() {
                let now = now_secs();
                if now.saturating_sub(signal.last_audit_secs) >= AUDIT_RETRY_SECS {
                    match self.auditor.evaluate_token(&mint).await {
                        Ok(report) => {
                            info!(%mint, score = report.score, "audit resolved on retry");
                            signal.audit_score = Some(report.score);
                            signal.last_audit_secs = now;
                            audit_changed = true;
                        }
                        Err(e) if e.is_retryable() => {
                            debug!(%mint, error = %e, "audit still unresolved; keeping watch");
                            signal.last_audit_secs = now;
                            audit_changed = true;
                        }
                        Err(e) => {
                            warn!(%mint, error = %e, "audit resolved non-retryable; dropping watch");
                            let mut state = self.gate_state.lock().await;
                            state.gate.remove(&mint);
                            state.meta.remove(&mint);
                            continue;
                        }
                    }
                }
            }
            // One lock: skip relaunches; terminal removes keep newborns.
            let step = {
                let mut state = self.gate_state.lock().await;
                let fresh = state.meta.get(&mint);
                // Drift means relaunch: never clobber or confirm.
                let stale = fresh.is_some_and(|current| {
                    current.curve != signal.curve
                        || current.assoc_curve != signal.assoc_curve
                        || current.creator != signal.creator
                        || current.dev != signal.dev
                        || current.audit_score != audit_at_snapshot
                        || current.creation_slot != signal.creation_slot
                });
                if stale {
                    GateStep::Stale
                } else {
                    // Snapshot watermark for the compare-and-swap below.
                    let snap_sig = signal.last_sig.clone();
                    let has_new = newest.is_some();
                    if let Some(sig) = newest {
                        signal.last_sig = Some(sig);
                    }
                    if audit_changed || has_new {
                        // Keep a concurrently-advanced watermark.
                        if has_new
                            && let Some(current) = state.meta.get(&mint)
                            && current.last_sig != snap_sig
                            && let Some(fresh) = current.last_sig.clone()
                        {
                            signal.last_sig = Some(fresh);
                        }
                        state.meta.insert(mint.clone(), signal.clone());
                    }
                    if signal.audit_score.is_none() {
                        GateStep::Waiting
                    } else {
                        // Microstructure tape: this round's activity
                        // against the stored previous point (velocity
                        // needs history of one). The stored point is
                        // read before this round overwrites it below.
                        let now = now_secs();
                        let stored_prev = state.meta.get(&mint).and_then(|m| m.prev_tape);
                        let round_buys = swaps
                            .iter()
                            .filter(|s| s.side == listener::SwapSide::Buy)
                            .count() as u32;
                        let micro = micro_signals(
                            &swaps,
                            &signal.dev,
                            signal.creator.as_deref(),
                            stored_prev,
                            now,
                        );
                        signal.prev_tape = Some(TapePoint {
                            ts: now,
                            buys: round_buys,
                        });
                        match state.gate.observe(&mint, &swaps, now) {
                            GateOutcome::Confirm { buyers } => {
                                state.meta.remove(&mint);
                                // Tape volume rides the confirm: the same
                                // swaps that made quorum, in base units.
                                let tape = tape_volume_base_units(&swaps);
                                GateStep::Confirmed {
                                    buyers,
                                    tape,
                                    signal: Box::new(signal.clone()),
                                    micro,
                                }                            }
                            GateOutcome::Abort { reason } => {
                                state.meta.remove(&mint);
                                GateStep::Aborted(reason)
                            }
                            GateOutcome::Wait => GateStep::Waiting,
                        }
                    }
                }
            };
            match step {
                GateStep::Confirmed {
                    buyers,
                    tape,
                    signal: sig,
                    micro,
                } => confirmed.push((mint, sig, buyers, tape, micro)),
                GateStep::Aborted(reason) => {
                    info!(%mint, %reason, "gate abort");
                }
                GateStep::Waiting | GateStep::Stale => {}
            }
        }
        // Phase 3: forensics plus opens.
        if !confirmed.is_empty() {
            use futures_util::stream::{self, StreamExt};
            stream::iter(confirmed.into_iter().map(|(mint, signal, buyers, tape, micro)| async move {
                // Reject drops; suspect is paper-only.
                match self
                    .screener
                    .screen_bundle(&signal.curve, signal.creation_slot)
                    .await
                {
                    Ok(analysis)
                        if analysis.verdict == BundleVerdict::RejectBundledMonopoly =>
                    {
                        info!(%mint, buys = analysis.creation_slot_buys, supply = format!("{:.1}%", analysis.total_cluster_supply_pct), "bundle REJECT");
                    }
                    Ok(analysis) if analysis.verdict == BundleVerdict::SuspectLowVolume => {
                        info!(%mint, "bundle suspect: paper-only, live dispatch skipped");
                        self.confirm_open(&OpenRequest { mint: &mint, signal: &signal, buyers, tape_base_units: tape, micro, allow_live: false, creation_slot_buys: Some(analysis.creation_slot_buys as u32), cluster_pct: Some(analysis.total_cluster_supply_pct) }).await;
                    }
                    Ok(analysis) => {
                        self.confirm_open(&OpenRequest { mint: &mint, signal: &signal, buyers, tape_base_units: tape, micro, allow_live: true, creation_slot_buys: Some(analysis.creation_slot_buys as u32), cluster_pct: Some(analysis.total_cluster_supply_pct) }).await;
                    }
                    Err(e) => {
                        warn!(%mint, error = %ids::redact_urls_in_text(&e.to_string()), "bundle screen error; dropping (fail-closed)");
                    }
                }
            }))
            .buffered(fanout.max(1))
            .collect::<Vec<()>>()
            .await;
        }
        // AIMD fanout: slow halves, fast recovers one.
        {
            let mut state = self.gate_state.lock().await;
            if round_start.elapsed().as_secs() >= SLOW_GATE_ROUND_SECS {
                let cut = (state.fanout / 2).max(GATE_FANOUT_MIN);
                if cut != state.fanout {
                    warn!(from = state.fanout, to = cut, "slow gate round; halving fanout");
                    state.fanout = cut;
                }
            } else if state.fanout < GATE_FANOUT {
                state.fanout += 1;
            }
        }
        let now = now_secs();
        // Sweep timeouts under one short lock.
        let timeouts = { self.gate_state.lock().await.gate.sweep_timeouts(now) };
        for (mint, outcome) in timeouts {
            // Drop meta only when no fresh entry replaced it.
            let mut state = self.gate_state.lock().await;
            if !state.gate.contains(&mint) {
                state.meta.remove(&mint);
            }
            drop(state);
            if let GateOutcome::Abort { reason } = outcome {
                info!(%mint, %reason, "gate abort");
            }
        }
        // Timer-driven idle-follow sweep: runs even when no pool ticks
        // arrive, so dead follows can't leak to restart.
        let stale = self.sweep_idle().await;
        if !stale.is_empty() {
            info!(count = stale.len(), "swept idle post-migration follows");
        }
    }

    /// Open a confirmed signal: price, check risk, open paper plus live.
    async fn confirm_open(&self, req: &OpenRequest<'_>) {
        let mint = req.mint;
        let signal = req.signal;
        let buyers = req.buyers;
        // Price and tier from one chain state.
        let (tick_result, venue_fee) = match self
            .reserves
            .fetch_tick_with_fee(mint, &signal.curve)
            .await
        {
            Ok((tick, fee)) => (Ok(tick), fee),
            Err(e) => (Err(e), None),
        };
        // Exact reserves off the same read (copied out before the tick
        // is consumed): paper fills and live sizing both quote them.
        // The complete flag rides along separately: the choke refuses
        // migrated entries (see `open_tracked`).
        let exec = tick_result.as_ref().ok().and_then(|t| t.exec);
        let complete = tick_result.as_ref().is_ok_and(|t| t.complete);
        // Fallback is paper-only; the flag is the source.
        let Some((entry_quote, liquidity_usd, priced_via_override)) = resolve_entry_with_flag(
            tick_result,
            self.settings.quote_usd,
            self.settings.liquidity_usd,
        ) else {
            warn!(%mint, "no live price and QUOTE_USD unset; dropping signal");
            return;
        };

        // Live ticks must carry positive liquidity: zero liquidity
        // prices fee-only (synthetic-grade). Fail closed.
        if !priced_via_override
            && (!liquidity_usd.is_finite() || liquidity_usd <= 0.0)
        {
            warn!(%mint, liq = liquidity_usd, "non-positive curve liquidity; dropping signal (fail-closed)");
            return;
        }
        // Venue fee must be readable AND sane on live ticks: an
        // unreadable tier would book the standard tier while live pays
        // the custom one (fail-cheap), and NaN/negative would poison
        // fee math downstream. Synthetic (override-priced) registers
        // no tier, so it stays `None` by construction below.
        if !priced_via_override && venue_fee.is_none_or(|f| !f.is_finite() || f < 0.0) {
            warn!(%mint, "venue fee unreadable; dropping (fail-closed)");
            return;
        }
        const _: () = assert!(crate::live::TOKEN_DECIMALS == 6);
        match crate::live::six_decimals(&self.reserves, mint).await {
            Ok(_) => {}
            Err(crate::live::DecimalsRefusal::NonSix(other)) => {
                warn!(%mint, decimals = other, "non-6-decimal mint; sizing assumes 6 — dropping signal");
                return;
            }
            Err(crate::live::DecimalsRefusal::Unreadable(error)) => {
                warn!(%mint, %error, "mint decimals unreadable; dropping signal (fail-closed)");
                return;
            }
        }
        // Holder concentration, only when armed (`0` = off, costing no
        // reads): the ten largest holders own this % of supply. Over
        // the cap — or unreadable while armed — drops the signal
        // (fail-closed); exits and held bags are untouched.
        let holder_cap = self.settings.max_top_holder_pct;
        if holder_cap > 0.0 && holder_cap.is_finite() {
            match self.screener.top_holder_pct(mint).await {
                Ok(pct) if pct <= holder_cap => {
                    info!(%mint, top10_pct = format!("{pct:.1}"), "holder concentration within cap");
                }
                Ok(pct) => {
                    warn!(%mint, top10_pct = format!("{pct:.1}"), cap = holder_cap, "holder concentration over cap; dropping signal");
                    return;
                }
                Err(e) => {
                    warn!(%mint, error = %ids::redact_urls_in_text(&e.to_string()), "holder concentration unreadable while cap armed; dropping signal (fail-closed)");
                    return;
                }
            }
        }

        // Confirm-time insider re-screen: dev history goes stale
        // between watchlist intake and quorum (serial launchers, fresh
        // rugs). Reuses nothing from intake — worst trust wins again —
        // and fails closed on any screen failure.
        let dev_trust = match self.screen_wallet(mint, &signal.dev, "dev").await {
            Some(trust) => trust,
            None => {
                warn!(%mint, "confirm-time dev re-screen failed; dropping (fail-closed)");
                return;
            }
        };
        let mut trust = dev_trust;
        if let Some(creator) = signal.creator.as_deref()
            && creator != signal.dev.as_str()
        {
            match self.screen_wallet(mint, creator, "creator").await {
                Some(creator_trust) => trust = trust.min(creator_trust),
                None => {
                    warn!(%mint, "confirm-time creator re-screen failed; dropping (fail-closed)");
                    return;
                }
            }
        }

        // Confirm-time re-audit: LP pulls / authority flips between
        // launch and quorum are invisible to the snapshot. Snapshots
        // younger than one gate window skip the extra round-trip.
        // Serial path only (a speculative overlap was deleted as YAGNI:
        // unproven latency gain, abort races, wasted inference spend).
        let stale_snapshot = now_secs().saturating_sub(signal.last_audit_secs)
            > self.settings.gate_window_secs;
        let score = if stale_snapshot {
            match self.auditor.evaluate_token(mint).await {
                Ok(report) => report.score,
                Err(e) => {
                    warn!(%mint, error = %e, "confirm-time re-audit failed; dropping (fail-closed)");
                    return;
                }
            }
        } else {
            let Some(score) = signal.audit_score else {
                // Fail closed (unreachable via poll_gates).
                warn!(%mint, "confirm reached without an audit score; dropping (fail-closed)");
                return;
            };
            score
        };
        let metrics = TokenMetrics {
            liquidity_usd,
            buy_velocity_5s: buyers,
            dev_reputation: trust,
            rugcheck_score: score,
            venue_fee_bps: venue_fee,
            creation_slot_buys: req.creation_slot_buys,
            cluster_pct: req.cluster_pct,
            // Unmeasured: no reserve field carries curve progress; the
            // prompt assumes its spread prior when absent.
            curve_progress: None,
            // Confirming tape spot-priced at the confirm quote (the
            // 6-decimal sizing basis is proven above): `None` on an
            // empty tape, never a zero that reads as "no demand".
            volume_usd: if req.tape_base_units > 0 {
                let v = req.tape_base_units as f64 / 1_000_000.0 * entry_quote;
                v.is_finite().then_some(v)
            } else {
                None
            },
        };
        // Model call timed: entry latency is sniper budget (raise
        // INFERENCE_TIMEOUT_MS only knowing it costs entry price).
        let started = std::time::Instant::now();
        let decision = match &self.engine {
            Some(engine) => engine.evaluate(&metrics).await,
            None => RiskDecision {
                profile: evaluate_metrics_fallback(&metrics),
                source: DecisionSource::Fallback,
            },
        };
        let profile = decision.profile;
        // Always log the decision (source + latency): heuristic-only
        // operation (`engine: None`) must be as visible as model calls.
        // Logs stay on when alerts are silenced (see the settings TODO
        // above): the warn below is the fallback channel, not the card.
        match &self.engine {
            Some(engine) => {
                info!(%mint, source = ?decision.source, latency_ms = started.elapsed().as_millis(),
                    fallbacks = engine.fallback_count(), timeouts = engine.timeout_count(),
                    invalid = engine.invalid_count(), volume_usd = ?metrics.volume_usd,
                    "risk decided at confirm");
            }
            None => {
                warn!(%mint, source = ?decision.source, latency_ms = started.elapsed().as_millis(),
                    volume_usd = ?metrics.volume_usd,
                    "risk heuristic fallback (no engine)");
            }
        }
        // Confirm-tape microstructure: what the round's flow looked
        // like (logged for BUY and PASS alike — a passed tape is still
        // evidence about the signal).
        {
            let m = req.micro;
            info!(%mint, buys = m.buys, sells = m.sells, buy_ratio = ?m.buy_ratio,
                unique_traders = m.unique_traders, top_wallet_share_pct = ?m.top_wallet_share_pct,
                creator_net_base = m.creator_net_base, velocity_delta = ?m.velocity_delta,
                "confirm tape microstructure");
        }
        if profile.action != Action::Buy {
            info!(%mint, buyers, score, "risk PASS at confirm");
            return;
        }
        // Rewrite BUY per strategy.
        let strategy = StrategyConfig {
            mode: self.strategy,
            runner_moonbag_percent: self.settings.runner_moonbag_percent,
        };
        let profile = resolve_strategy(&profile, &strategy);
        info!(%mint, mode = ?strategy.mode, tp = profile.tp_percent,
            moonbag = profile.moonbag_percent, trailing = profile.trailing_stop,
            "strategy resolved");
        // Low scalp TP is honored, never widened.
        if strategy.mode == StrategyMode::ScalpQuick && profile.tp_percent < SCALP_TP_MIN {
            warn!(%mint, tp = profile.tp_percent,
                "scalp TP below +25% band; honoring the model's early exit");
        }
        // Runners use the wide trail; others use the shared setting.
        let trail_percent = match strategy.mode {
            StrategyMode::Runner => self.settings.runner_trail_percent,
            _ => self.settings.trail_percent,
        };
        // One SOL reading for both legs.
        let sol = self.sol_feed.stamped_price();
        // Override opens are synthetic paper.
        if priced_via_override {
            self.exec.mark_synthetic(mint).await;
        }
        self.open_tracked(&TrackedOpen {
            mint,
            curve: &signal.curve,
            assoc_curve: &signal.assoc_curve,
            entry_quote,
            liquidity_usd,
            profile: &profile,
            risk: match decision.source {
                DecisionSource::Model => "model",
                DecisionSource::Fallback => "heuristic",
            },
            allow_live: req.allow_live && !priced_via_override,
            creator: signal.creator.as_deref(),
            // Synthetic registers no tier.
            venue_fee_bps: if priced_via_override { None } else { venue_fee },
            sol,
            trail_percent,
            // Synthetic (override-priced) opens carry no chain state.
            reserves: if priced_via_override {
                None
            } else {
                exec.map(|e| crate::live::curve_reserves(&e))
            },
            complete,
            micro: req.micro,
        })
        .await;
    }

    /// Open paper at confirm price, watch exits, mirror live if allowed.
    async fn open_tracked(&self, req: &TrackedOpen<'_>) {
        let mint = req.mint;
        let curve = req.curve;
        let assoc_curve = req.assoc_curve;
        let entry_quote = req.entry_quote;
        let liquidity_usd = req.liquidity_usd;
        let profile = req.profile;
        let allow_live = req.allow_live;
        let trail_percent = req.trail_percent;
        let creator = req.creator;
        let sol = req.sol;
        let reserves = req.reserves;
        // Kill switch first: an operator stop preempts every other
        // check and touches nothing but new risk. Exits, scale-outs,
        // and `sell <mint>` are owned elsewhere and keep running.
        if self.entries_paused.load(Ordering::Relaxed) {
            info!(%mint, "kill switch: entries paused; skipping open");
            return;
        }
        // Data degrade: a critical source (RPC pool / SOL feed /
        // listener) is unhealthy — refusing to decide on it is the
        // whole point. Same choke point as the kill switch; exits are
        // untouched and the supervisor clears it on recovery.
        if self.data_paused.load(Ordering::Relaxed) {
            warn!(%mint, "data degraded: entries paused; skipping open");
            return;
        }
        // Migrated/completed curves never open: entries are
        // pre-migration only (exits route via pool legs). Without this,
        // paper would open what live must refuse at preflight
        // (`state.complete`), stranding paper-without-live. Same choke
        // point as the kill switch; the migrated set covers ticks that
        // don't carry the flag.
        if req.complete || self.migrated.lock().await.contains_key(req.mint) {
            warn!(%mint, "curve complete/migrated; entries are pre-migration only — skipping open");
            return;
        }
        // WAL degraded: a custody write failed, so the claim of record
        // is broken. No new risk until an operator restart (same choke
        // point as the kill switch; exits are untouched).
        if self.exec.live.as_ref().is_some_and(|t| t.wal_degraded()) {
            warn!(%mint, "live WAL write failed; refusing new entries until restart");
            return;
        }
        // Breaker next: past cap, no new risk. One card per day;
        // the heartbeat carries the halted state after that.
        let (tripped, fire, day_net) = {
            let mut daily = self.daily.lock().await;
            let (tripped, fire) =
                daily.halt_state(self.settings.max_daily_loss_usd, now_secs());
            (tripped, fire, daily.day_net())
        };
        if tripped {
            warn!(%mint, cap = self.settings.max_daily_loss_usd, day_net, "daily loss breaker tripped; skipping open");
            if fire {
                self.emit(Alerter::format_halt(
                    &self.card_ctx(),
                    self.settings.max_daily_loss_usd,
                    day_net,
                ));
            }
            return;
        }
        // Execution-intelligence gate: this ticket shape's settled
        // entry gaps say what live really paid over paper. Proven bleed
        // (a cell with enough settled samples) worse than the cap
        // refuses the open — costs would eat the edge before the trade
        // starts. Unknown (thin cell) always allows: absence of evidence
        // is not evidence of bleed. Same choke point as the kill switch
        // (entries only; exits, scale-outs, `sell <mint>` untouched),
        // and the window sheds stale evidence on its own. Buys are
        // structurally curve (no pool-buy path), so the venue is fixed.
        let bleed_cap = self.settings.max_expected_entry_gap_usd;
        if bleed_cap.is_finite()
            && bleed_cap > 0.0
            && let Some(exp) = self
                .exec
                .expected_entry_gap_usd("curve", self.settings.notional_usd, liquidity_usd)
                .await
            && exp < -bleed_cap
        {
            info!(%mint, expected_gap_usd = format!("{exp:+.4}"), cap_usd = bleed_cap,
                "entry bleed cap: this ticket shape bled worse than cap; skipping open");
            return;
        }
        let mut portfolio = self.portfolio.lock().await;
        // Heat + per-mint cooldown.
        if portfolio.position_ids().len() >= self.settings.max_open_positions {
            info!(%mint, max = self.settings.max_open_positions, "heat limit: skipping");
            return;
        }
        // Live exposure counts too: paper cash recycles on close while
        // live SOL stays locked (tracked/parked/pending), so paper-only
        // heat would keep opening into a stuck, diverged book. Mints
        // already paired with paper count once.
        {
            let mut paper_mints = std::collections::HashSet::new();
            let mut paper_locked = 0.0;
            for id in portfolio.position_ids() {
                if let Some(p) = portfolio.position(id) {
                    paper_mints.insert(p.mint.clone());
                    // Basis actually invested (floored size at fill).
                    paper_locked += p.entry_notional;
                }
            }
            let live_extra = self
                .exec
                .held_base_mints()
                .await
                .into_iter()
                .filter(|m| !paper_mints.contains(m))
                .count();
            if portfolio.position_ids().len() + live_extra
                >= self.settings.max_open_positions
            {
                info!(%mint, max = self.settings.max_open_positions, live_extra, "heat limit (incl. live-held mints): skipping");
                return;
            }
            // Total exposure in USD counts the same two sides: paper
            // cost basis, plus one ticket per unpaired live bag (a bag
            // carries no stored USD basis; one ticket is the estimate
            // heat already uses). The prospective ticket is included.
            let cap = self.settings.max_exposure_usd;
            if cap.is_finite() && cap > 0.0 {
                let deployed = paper_locked + live_extra as f64 * self.settings.notional_usd;
                let prospective = deployed + self.settings.notional_usd;
                if prospective > cap {
                    info!(%mint, deployed, cap, ticket = self.settings.notional_usd,
                        "exposure cap: skipping open");
                    return;
                }
            }
        }
        if portfolio
            .position_ids()
            .iter()
            .any(|id| portfolio.position(*id).is_some_and(|p| p.mint == mint))
        {
            info!(%mint, "already holding this mint; skipping");
            return;
        }
        let now = now_secs();
        if self.settings.mint_cooldown_secs > 0
            && portfolio.history().iter().any(|t| {
                t.mint == mint && now.saturating_sub(t.closed_at) < self.settings.mint_cooldown_secs
            })
        {
            info!(%mint, "mint in cooldown; skipping");
            return;
        }
        // Entry-gap calibration for this ticket shape, for opens that
        // survived every gate above (the bleed-cap refusal already ran
        // at the choke point): proven bleed is logged, unknown stays
        // silent. Buys are structurally curve, so the venue is fixed.
        if let Some(exp) = self
            .exec
            .expected_entry_gap_usd("curve", self.settings.notional_usd, liquidity_usd)
            .await
        {
            info!(%mint, ticket_usd = self.settings.notional_usd, liquidity_usd,
                expected_gap_usd = format!("{exp:+.4}"),
                "entry gap calibration: this ticket shape bled live before");
        }
        // Tier read with the tick. Custom tiers reprice paper like live.
        if let Some(bps) = req.venue_fee_bps {
            portfolio.set_venue_fee_for_mint(mint, bps);
            if (bps - crate::live::VENUE_FEE_BPS as f64).abs() > f64::EPSILON {
                info!(%mint, bps, "custom venue fee tier registered for paper ledger");
            }
        }
        // Widen stops past round-trip costs on custom tiers (venue +
        // commission): a stop inside total costs is a guaranteed loss.
        // Both inputs are per-side; `fee_aware_sl_floor` doubles to
        // round-trip internally.
        let mut profile = *profile;
        if let Some(bps) = req.venue_fee_bps {
            let per_side_total_bps = bps + self.settings.fee_bps;
            let widened = fee_aware_sl_floor(profile.sl_percent, per_side_total_bps);
            if widened < profile.sl_percent {
                info!(%mint, sl = profile.sl_percent, widened, bps,
                    "custom-tier stop widened past round-trip costs");
                profile.sl_percent = widened;
            }
        }
        // Paper-only bleed: live legs debit actuals, so without them
        // the ledger would never see dispatch costs. Skipped when
        // priceless (legs keep their last values).
        if self.exec.live.is_none()
            && let Some((sol_usd, _)) = sol
        {
            let (open_leg, close_leg) = crate::live::paper_dispatch_legs_usd(
                self.settings.tip_lamports,
                crate::live::priority_fee_lamports(
                    self.settings.compute_unit_limit,
                    self.settings.priority_fee_micro_lamports,
                ),
                sol_usd,
            );
            portfolio.set_paper_dispatch_legs(open_leg, close_leg);
        }
        // Mark at open: the tick spot plus exact reserves when the tick
        // carried them (state- and size-aware fill); proxy basis else.
        let mark = simulator::Mark {
            price: entry_quote,
            liquidity_usd,
            curve: reserves,
        };
        match portfolio.open_position_params(&simulator::OpenPositionParams {
            mint,
            side: Side::Long,
            mark,
            notional_usd: self.settings.notional_usd,
            tp_percent: profile.tp_percent,
            sl_percent: profile.sl_percent,
            trailing: profile.trailing_stop,
            trail_percent,
            timeout_seconds: profile.timeout_seconds,
            moonbag_percent: profile.moonbag_percent,
            now_secs: now,
        }) {
            Ok(id) => {
                // Booked execution price (impact + fee inside), never
                // the raw mark: logs and cards report what the book paid.
                let booked = portfolio
                    .position(id)
                    .map(|p| p.entry_price)
                    .unwrap_or(entry_quote);
                // What this open debited (same legs `open_position`
                // used): the breaker books it now because the close
                // headline only nets the close leg.
                let open_leg_usd = portfolio.paper_dispatch_legs().0;
                let snap = portfolio.snapshot(&HashMap::new());
                info!(%mint, id, entry = format!("{booked:.10}"),
                    mark = format!("{entry_quote:.10}"),
                    tp = profile.tp_percent, sl = profile.sl_percent,
                    equity = format!("{:.2}", snap.equity), "paper position opened");
                // Liquidity-aware exit preview for the size just booked:
                // what a full exit nets now (SOL and USD) and under
                // drained books. Log-only sizing context — no cap
                // exists, so nothing refuses here; measurement first.
                if let Some(qty) = portfolio.position(id).map(|p| p.quantity)
                    && let Some(proceeds) =
                        portfolio
                            .fill_for_mint(mint)
                            .exit_proceeds(Side::Long, &mark, qty)
                {
                    let usd = |v: f64| {
                        if v.is_finite() {
                            format!("{v:.2}")
                        } else {
                            "n/a".to_string()
                        }
                    };
                    let in_sol = match sol {
                        Some((sol_usd, _)) if sol_usd.is_finite() && sol_usd > 0.0 => {
                            format!("{:.4}", proceeds.realizable_usd / sol_usd)
                        }
                        _ => "n/a".to_string(),
                    };
                    info!(%mint, realizable_usd = usd(proceeds.realizable_usd),
                        realizable_sol = in_sol,
                        discount_bps = format!("{:.0}", proceeds.discount_bps),
                        half_liq_usd = usd(proceeds.stressed[0].1),
                        quarter_liq_usd = usd(proceeds.stressed[1].1),
                        "exit proceeds at current and drained depth");
                }
                // Decision record: the paper open plus the risk engine
                // provenance that shaped its profile.
                if let Some(j) = self.journal() {
                    let position = portfolio.position(id).cloned();
                    if let Some(position) = position {
                        j.append(&JournalLine::Open {
                            ts: now,
                            mint: mint.to_string(),
                            risk: req.risk.to_string(),
                            venue_fee_bps: portfolio.venue_fee_for(mint),
                            sol_feed: self.sol_feed.price(),
                            mark,
                            ticket_usd: self.settings.notional_usd,
                            position,
                            micro: Some(req.micro),
                        });
                    }
                }
                // Emit after unlock: never alert under lock.
                let card = Alerter::format_entry(
                    &self.card_ctx(),
                    mint,
                    booked,
                    profile.tp_percent,
                    profile.sl_percent,
                    self.settings.notional_usd,
                    req.risk,
                );
                drop(portfolio);
                // Paper-only open-leg bleed feeds the breaker at open
                // (live mode records the actual buy tip at buy instead;
                // synthetic never feeds the breaker).
                if self.exec.live.is_none()
                    && open_leg_usd.is_finite()
                    && open_leg_usd > 0.0
                    && !self.exec.is_synthetic(mint).await
                {
                    self.daily.lock().await.record(-open_leg_usd, now);
                }
                self.emit(card);
                // No feed means no live leg: exits would be manual.
                if let Err(e) = self.reserves.watch(mint, curve).await {
                    warn!(%mint, error = %ids::redact_urls_in_text(&e.to_string()), "ticker watch failed; paper-only, live mirror skipped");
                    if allow_live {
                        self.exec
                            .mark_diverged(mint, "live buy skipped: ticker watch failed".to_string())
                            .await;
                        self.emit(format!("ticker watch failed on {mint}; paper open WITHOUT live cover"));
                    }
                } else if allow_live {
                    self.live_buy(mint, curve, assoc_curve, mark, sol, creator)
                        .await;
                }
            }
            Err(e) => warn!(%mint, error = %e, "paper open failed"),
        }
    }
}

/// Oldest watch entry by intake time (hard-cap eviction key). `None`
/// on an empty map. Deterministic: never a HashMap-key accident.
fn oldest_watch_entry(meta: &HashMap<String, PendingSignal>) -> Option<String> {
    meta.iter()
        .min_by_key(|(_, signal)| signal.intake_secs)
        .map(|(mint, _)| mint.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_cap_sheds_the_oldest_intake() {
        let signal = |intake_secs: u64| PendingSignal {
            curve: "C".to_string(),
            creator: None,
            assoc_curve: "A".to_string(),
            audit_score: Some(1),
            last_audit_secs: intake_secs + 500,
            creation_slot: 1,
            last_sig: None,
            dev: "D".to_string(),
            intake_secs,
            prev_tape: None,
        };
        let mut meta = HashMap::new();
        meta.insert("newest".to_string(), signal(300));
        meta.insert("oldest".to_string(), signal(100));
        meta.insert("middle".to_string(), signal(200));
        assert_eq!(oldest_watch_entry(&meta), Some("oldest".to_string()));
        // Audit retries re-stamp `last_audit_secs`, never the intake key.
        meta.get_mut("middle").expect("m").last_audit_secs = 9_999;
        assert_eq!(oldest_watch_entry(&meta), Some("oldest".to_string()));
        assert_eq!(oldest_watch_entry(&HashMap::new()), None);
    }

    #[test]
    fn fee_aware_sl_floor_leaves_standard_stops() {
        assert_eq!(fee_aware_sl_floor(-8.0, 125.0), -8.0);
        assert_eq!(fee_aware_sl_floor(-10.0, 125.0), -10.0);
    }

    #[test]
    fn fee_aware_sl_floor_widens_custom_tiers() {
        assert_eq!(fee_aware_sl_floor(-8.0, 345.0), -10.9);
        assert_eq!(fee_aware_sl_floor(-15.0, 345.0), -15.0);
    }

    #[test]
    fn fee_aware_sl_floor_clamps_to_full_loss() {
        assert_eq!(fee_aware_sl_floor(-8.0, 10_000.0), -100.0);
        assert_eq!(fee_aware_sl_floor(-8.0, 100_000.0), -100.0);
    }
}
