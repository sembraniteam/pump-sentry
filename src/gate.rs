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
    Action, DecisionSource, ExitRegime, RiskDecision, SCALP_TP_MIN, StrategyConfig, StrategyMode,
    TokenMetrics, evaluate_metrics_fallback, resolve_regime, resolve_strategy,
};
use simulator::Side;
use tracing::{debug, info, warn};

use crate::journal::JournalLine;
use crate::pipeline::{AlertSink, Auditor, CurveRead, LiveSink, Pipeline, ReserveFeed, Screener};
use crate::settings::now_secs;

/// One curve read: `(mint, signal, full-or-truncated swaps or error)`.
type Observation = (String, PendingSignal, Result<CurveRead, ScreenError>);

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
    debug_assert!(
        per_side_total_bps.is_finite(),
        "per-side bps must be finite"
    );
    if !per_side_total_bps.is_finite() {
        warn!(
            sl = sl_percent,
            bps = per_side_total_bps,
            "non-finite fee tier; holding the requested stop"
        );
        return sl_percent;
    }
    let floored = sl_percent.min(-(2.0 * per_side_total_bps / 100.0 + 4.0));
    let clamped = floored.clamp(-100.0, 0.0);
    if clamped != floored {
        warn!(
            sl = sl_percent,
            bps = per_side_total_bps,
            "absurd fee tier clamped the stop; check the schedule"
        );
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
#[derive(Debug, Clone)]
struct TrackedOpen<'a> {
    mint: &'a str,
    curve: &'a str,
    assoc_curve: &'a str,
    entry_quote: f64,
    liquidity_usd: f64,
    profile: &'a risk_engine::RiskProfile,
    /// Profile provenance for the entry card (`model` | `heuristic`).
    risk: &'static str,
    /// Risk-engine model the decision rode (Some whenever an engine is
    /// configured, model or fallback alike — a fallback open is
    /// attributed to the model whose failure produced it; `None` =
    /// no engine). Journal-only: per-model performance is aggregated
    /// offline, never a gate input.
    model: Option<String>,
    allow_live: bool,
    creator: Option<&'a str>,
    /// Venue tier with the tick. `None` is standard; synthetic forces `None`.
    venue_fee_bps: Option<f64>,
    /// SOL/USD stamped at confirm. Rechecked at use; stale fails closed.
    sol: Option<(f64, u64)>,
    trail_percent: f64,
    /// Exact curve reserves off the confirm tick: the paper fill and the
    /// live sizing quote the chain's own math. `None` on synthetic
    /// (override-priced) opens and mocks.
    reserves: Option<simulator::CurveReserves>,
    /// True when the confirm tick read complete (chain truth). Entries
    /// are pre-migration only; the choke refuses these (see below).
    complete: bool,
    /// Confirm-tape microstructure snapshot for the journal.
    micro: MicroSignals,
    /// Effective exit regime (LLM pick or forced baseline) for the journal.
    regime: ExitRegime,
    /// Ex-ante round-trip cost preview at the confirm mark (see
    /// `FlipPreview`). `None` = unpriceable — the cap and the journal
    /// treat unknown as absent, never as a cost.
    flip: Option<simulator::FlipPreview>,
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
    /// microstructure velocity. Written back to the map on every
    /// observed waiting round (AUD-003: a local-only write here left
    /// `velocity_delta` null forever); the confirm reads the stored
    /// point, never a re-stamped audit time. A defer rewatch inserts
    /// the confirmed round's local point as the fresh window's start.
    pub prev_tape: Option<TapePoint>,
    /// Confirm-level risk defers taken under the `defer` policy
    /// (inference failed and the decision was postponed). Counts on
    /// the signal; exhaustion lands the entry on the heuristic.
    pub defers: u32,
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
        Self {
            day: now_secs / 86_400,
            day_net: 0.0,
            halt_alerted_day: 0,
        }
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

/// Assemble the risk-model input: confirm pricing plus the
/// structured tape. Pure free function (unit-pinned): the analyst
/// sees the same numbers the logs show. The hard gates around the
/// call stay independent — the model can only tighten (PASS/temper)
/// through the shared profile, never bypass a refusal.
#[allow(clippy::too_many_arguments)]
fn confirm_metrics(
    liquidity_usd: f64,
    buyers: u32,
    trust: f64,
    score: u32,
    venue_fee: Option<f64>,
    creation_slot_buys: Option<u32>,
    cluster_pct: Option<f64>,
    volume_usd: Option<f64>,
    micro: MicroSignals,
    top_holder_pct: Option<f64>,
    flip_loss_pct: Option<f64>,
) -> TokenMetrics {
    TokenMetrics {
        liquidity_usd,
        buy_velocity_5s: buyers,
        dev_reputation: trust,
        rugcheck_score: score,
        venue_fee_bps: venue_fee,
        creation_slot_buys,
        cluster_pct,
        // Unmeasured: no reserve field carries curve progress; the
        // prompt assumes its spread prior when absent — the measured
        // neighbor `entry_flip_loss_pct` supersedes it when priced.
        curve_progress: None,
        volume_usd,
        buy_ratio: micro.buy_ratio,
        unique_traders: micro.unique_traders,
        top_wallet_share_pct: micro.top_wallet_share_pct,
        creator_net_base: micro.creator_net_base,
        velocity_delta: micro.velocity_delta,
        top_holder_pct,
        entry_flip_loss_pct: flip_loss_pct,
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
                warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "audit unresolved (indexing/transport); watchlisting for retry within gate window");
                (None, now)
            }
            Err(e) => {
                warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "audit error; skipping (fail-closed)");
                return;
            }
        };

        // 2. Dev history, fail closed. Admission only: the score is
        // NOT stored — confirm re-screens both wallets fresh (worst
        // wins there), so nothing here can go stale across the wait.
        if self.screen_wallet(mint, dev, "dev", false).await.is_none() {
            return;
        }

        // 2b. v2 creator: screened too, worst wins at confirm.
        let distinct_creator = req.creator.filter(|c| *c != dev);
        if let Some(creator) = distinct_creator
            && self
                .screen_wallet(mint, creator, "creator", false)
                .await
                .is_none()
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
                defers: 0,
            },
        );
        match audit_score {
            Some(score) => info!(%mint, score, "watchlisted; awaiting buyer quorum"),
            None => {
                info!(%mint, "watchlisted without audit score; awaiting buyer quorum + audit retry")
            }
        }
    }

    /// Screen one insider to a trust score. `None` skips (fail-closed).
    /// `fresh` bypasses any dev-verdict cache (confirm-time re-screen).
    async fn screen_wallet(
        &self,
        mint: &str,
        wallet: &str,
        role: &str,
        fresh: bool,
    ) -> Option<f64> {
        let verdict = if fresh {
            self.screener.screen_dev_fresh(wallet).await
        } else {
            self.screener.screen_dev(wallet).await
        };
        match verdict {
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
            warn!(
                watchlist = mints.len(),
                "watchlist above watermark; RPC burn follows launch rate"
            );
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
                            debug!(%mint, error = %ids::strip_error_url(&e.to_string()), "audit still unresolved; keeping watch");
                            signal.last_audit_secs = now;
                            audit_changed = true;
                        }
                        Err(e) => {
                            warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "audit resolved non-retryable; dropping watch");
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
                                }
                            }
                            GateOutcome::Abort { reason } => {
                                state.meta.remove(&mint);
                                GateStep::Aborted(reason)
                            }
                            GateOutcome::Wait => {
                                // AUD-003: persist this round's tape point
                                // so the NEXT round's velocity compares
                                // against it. Field-level write only (no
                                // insert): a concurrently-advanced
                                // watermark is not clobbered and a
                                // removed/relaunched entry is never
                                // resurrected here.
                                if let Some(entry) = state.meta.get_mut(&mint) {
                                    entry.prev_tape = signal.prev_tape;
                                }
                                GateStep::Waiting
                            }
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
                    warn!(
                        from = state.fanout,
                        to = cut,
                        "slow gate round; halving fanout"
                    );
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
        let (tick_result, venue_fee) =
            match self.reserves.fetch_tick_with_fee(mint, &signal.curve).await {
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
        if !priced_via_override && (!liquidity_usd.is_finite() || liquidity_usd <= 0.0) {
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
        // Holder concentration: the ten largest holders own this % of
        // supply. Read once per confirm for BOTH consumers: the armed
        // cap refuses over-cap (or unreadable-while-armed) fail-closed,
        // and the risk model always receives the reading as context
        // (`None` when unreadable and unarmed — unknown, never zero).
        // Exits and held bags are untouched either way.
        let holder_cap = self.settings.max_top_holder_pct;
        let armed = holder_cap > 0.0 && holder_cap.is_finite();
        let top_holder_pct = match self.screener.top_holder_pct(mint).await {
            Ok(pct) => {
                if armed && pct > holder_cap {
                    warn!(%mint, top10_pct = format!("{pct:.1}"), cap = holder_cap, "holder concentration over cap; dropping signal");
                    return;
                }
                info!(%mint, top10_pct = format!("{pct:.1}"), "holder concentration read");
                Some(pct)
            }
            Err(e) => {
                if armed {
                    warn!(%mint, error = %ids::redact_urls_in_text(&e.to_string()), "holder concentration unreadable while cap armed; dropping signal (fail-closed)");
                    return;
                }
                debug!(%mint, error = %ids::redact_urls_in_text(&e.to_string()), "holder concentration unreadable; model reads unknown");
                None
            }
        };

        // Confirm-time insider re-screen: dev history goes stale
        // between watchlist intake and quorum (serial launchers, fresh
        // rugs). Reuses nothing from intake — worst trust wins again —
        // and fails closed on any screen failure. Worst trust also
        // means fresh history: confirm screens bypass the dev-verdict
        // cache (AUD-004), so a launch this wallet made after intake
        // cannot ride a stale pass.
        let dev_trust = match self.screen_wallet(mint, &signal.dev, "dev", true).await {
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
            match self.screen_wallet(mint, creator, "creator", true).await {
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
        let stale_snapshot =
            now_secs().saturating_sub(signal.last_audit_secs) > self.settings.gate_window_secs;
        let score = if stale_snapshot {
            match self.auditor.evaluate_token(mint).await {
                Ok(report) => report.score,
                Err(e) => {
                    warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "confirm-time re-audit failed; dropping (fail-closed)");
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
        // Ex-ante round-trip cost at the CURRENT book: what one
        // normal ticket costs to open and immediately sell back
        // (entry impact, exit impact, venue fees inside). Computed
        // once here, before the model — the decision gets the
        // measured number as context (`entry_flip_loss_pct`) and the
        // entry choke enforces `MAX_ENTRY_FLIP_LOSS_PCT` on it;
        // unknown (unpriceable) stays `None` and every consumer
        // allows. The fill mirrors the book's tiering: prior
        // registrations ride through, the tick's own tier overrides.
        let flip = {
            let portfolio = self.portfolio.lock().await;
            let fill = match venue_fee {
                Some(bps) => portfolio.fill_for_mint(mint).with_venue_fee_bps(bps),
                None => portfolio.fill_for_mint(mint),
            };
            drop(portfolio);
            let mark = simulator::Mark {
                price: entry_quote,
                liquidity_usd,
                curve: exec.map(|e| crate::live::curve_reserves(&e)),
            };
            fill.flip_preview(&mark, self.settings.notional_usd)
        };
        if let Some(preview) = flip {
            info!(%mint, flip_loss_pct = format!("{:.2}", preview.flip_loss_pct),
                booked_usd = format!("{:.4}", preview.booked_usd),
                realizable_usd = format!("{:.4}", preview.realizable_usd),
                qty_tokens = preview.qty_tokens,
                "entry flip cost preview at confirm (open + immediate sell-back)");
        }
        let metrics = confirm_metrics(
            liquidity_usd,
            buyers,
            trust,
            score,
            venue_fee,
            req.creation_slot_buys,
            req.cluster_pct,
            if req.tape_base_units > 0 {
                let v = req.tape_base_units as f64 / 1_000_000.0 * entry_quote;
                v.is_finite().then_some(v)
            } else {
                None
            },
            req.micro,
            top_holder_pct,
            flip.map(|p| p.flip_loss_pct),
        );
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
        // Explicit fallback policy (`inference_fallback`): on an engine
        // failure the operator chooses validated heuristic bands
        // (`heuristic`), skips the entry (`pass`), or defers (`defer`):
        // rewatch the signal and retry the model on a later confirm,
        // bounded by `INFERENCE_DEFER_MAX_N` (each defer starts a fresh
        // watch window, so the defer count is the hard stop; the
        // deadline aborts still-idle defers); exhausted defers land on
        // the heuristic. Engine-
        // absent operation is a deliberate mode, not a failure: it
        // never defers (nothing would recover) and the `pass` policy
        // still skips it. Single choke: both arrivals are `Fallback`.
        let mut profile = decision.profile;
        if decision.source == DecisionSource::Fallback {
            // Single choke: engine-absent and engine-failed both arrive
            // here as `Fallback`. Engine-absent is deliberate mode, not
            // a failure: `pass` still skips it (operator choice), but
            // it NEVER defers — nothing would recover.
            let model_failed = self.engine.is_some();
            match self.settings.inference_fallback.as_str() {
                "pass" => {
                    info!(%mint, "inference fallback policy=pass: skipping entry on model failure");
                    profile = risk_engine::RiskProfile::pass();
                }
                "defer" if model_failed && signal.defers < self.settings.inference_defer_max_n => {
                    // Postpone, don't decide on broken inference: rewatch
                    // the signal (fresh window, buyers re-gather; the
                    // entry gate self-evicts at its deadline and its
                    // submit cap bounds total lifetime) and let a later
                    // confirm decide on a model.
                    let mut rewatch = (*signal).clone();
                    rewatch.defers += 1;
                    let now = now_secs();
                    {
                        let mut state = self.gate_state.lock().await;
                        let _ = state.gate.submit(mint, &rewatch.dev, now);
                        if let Some(creator) = rewatch.creator.as_deref() {
                            let _ = state.gate.add_cohort(mint, creator);
                        }
                        state.meta.insert(mint.to_string(), rewatch);
                    }
                    info!(
                        %mint, defers = signal.defers,
                        max = self.settings.inference_defer_max_n,
                        "inference fallback policy=defer: rewatched; the model decides on a later confirm"
                    );
                    return;
                }
                "defer" if model_failed => {
                    info!(
                        %mint, defers = signal.defers,
                        max = self.settings.inference_defer_max_n,
                        "defer budget exhausted: validated heuristic bands decide (the default)"
                    );
                }
                _ => {}
            }
        }
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
        // Regime snap BEFORE the strategy reshape: the model picks
        // from the closed set and tested numbers replace its free-form
        // TP/SL; the forced baseline (evaluation control / LLM kill
        // path) overrides the pick. Operator shaping still applies
        // after, so `scalp`/`runner` intent always wins.
        let forced = self.settings.force_exit_regime.parse().ok();
        let (profile, regime) = resolve_regime(&profile, forced);
        // Rewrite BUY per strategy.
        let strategy = StrategyConfig {
            mode: self.strategy,
            runner_moonbag_percent: self.settings.runner_moonbag_percent,
        };
        let profile = resolve_strategy(&profile, &strategy);
        info!(%mint, mode = ?strategy.mode, regime = regime.as_str(), tp = profile.tp_percent,
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
            model: self.engine.as_ref().map(|e| e.model().to_string()),
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
            regime,
            flip,
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
            let (tripped, fire) = daily
                .halt_state(self.settings.max_daily_loss_usd, now_secs())
                .await;
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
            // Heat stays mint-based (`held_base_mints`): positions are
            // counted per mint, so a duplicate bag must not consume
            // slots. `live_unpaired_bag_count` is for the exposure
            // dollar cap only (AUD-014).
            let live_extra = self
                .exec
                .held_base_mints()
                .await
                .into_iter()
                .filter(|m| !paper_mints.contains(m))
                .count();
            if portfolio.position_ids().len() + live_extra >= self.settings.max_open_positions {
                info!(%mint, max = self.settings.max_open_positions, live_extra, "heat limit (incl. live-held mints): skipping");
                return;
            }
            // Total exposure in USD counts the same two sides: paper
            // cost basis, plus one ticket per unpaired live bag (each
            // bag is its own ticket; paired bags ride the paper basis).
            // The prospective ticket is included.
            let live_bags = self.exec.live_unpaired_bag_count(&paper_mints).await;
            let cap = self.settings.max_exposure_usd;
            if cap.is_finite() && cap > 0.0 {
                let deployed = paper_locked + live_bags as f64 * self.settings.notional_usd;
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
            // AUD-007: the typed fee guard only — an invalid raw
            // `fee_bps` (>=100%, negative, NaN) contributes NO widening.
            // It already fails closed in paper settlement; an unguarded
            // raw value here could clamp every live stop to the -100%
            // floor, silently removing real stop-losses.
            let typed_fee_bps = self.settings.fee().map(|f| f.get() as f64).unwrap_or(0.0);
            let per_side_total_bps = bps + typed_fee_bps;
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
        // Instant-flip cost cap (`0` = off): refuse the open when the
        // SAME preview that informed the decision, at the tier the
        // book will charge, says opening and selling straight back
        // loses more than this share of the ticket — costs would eat
        // any edge before the trade exists. Computed at the confirm
        // mark, before the model (see `confirm_open`) and enforced
        // HERE, at the entry choke: the model proposes, this number
        // cannot be argued past. Unknown (`None` / unpriceable / a
        // size the book booked differently) always allows: absence of
        // evidence is not evidence of cost. Exits never block.
        let flip_cap = self.settings.max_entry_flip_loss_pct;
        if flip_cap.is_finite() && flip_cap > 0.0 {
            match req.flip {
                Some(preview) if preview.flip_loss_pct > flip_cap => {
                    info!(%mint, flip_loss_pct = format!("{:.2}", preview.flip_loss_pct),
                        cap_pct = flip_cap, booked_usd = format!("{:.4}", preview.booked_usd),
                        realizable_usd = format!("{:.4}", preview.realizable_usd),
                        "entry flip cap: opening and selling back at current depth loses more than the cap; skipping open");
                    return;
                }
                Some(preview) => {
                    info!(%mint, flip_loss_pct = format!("{:.2}", preview.flip_loss_pct),
                        cap_pct = flip_cap, "entry flip cost inside cap");
                }
                None => {
                    debug!(%mint, cap_pct = flip_cap,
                        "entry flip cost unpriceable; cap allows (unknown is not a verdict)");
                }
            }
        }
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
                            model: req.model.clone(),
                            venue_fee_bps: portfolio.venue_fee_for(mint),
                            sol_feed: self.sol_feed.price(),
                            mark,
                            ticket_usd: self.settings.notional_usd,
                            position,
                            micro: Some(req.micro),
                            regime: Some(req.regime.as_str().to_string()),
                            flip_loss_pct: req.flip.map(|p| p.flip_loss_pct),
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
                    self.daily.lock().await.record(-open_leg_usd, now).await;
                }
                self.emit(card);
                // No feed means no live leg: exits would be manual.
                if let Err(e) = self.reserves.watch(mint, curve).await {
                    warn!(%mint, error = %ids::redact_urls_in_text(&e.to_string()), "ticker watch failed; paper-only, live mirror skipped");
                    if allow_live {
                        self.exec
                            .mark_diverged(
                                mint,
                                "live buy skipped: ticker watch failed".to_string(),
                            )
                            .await;
                        self.emit(format!(
                            "ticker watch failed on {mint}; paper open WITHOUT live cover"
                        ));
                    } else {
                        // AUD-002: paper-only open with no live leg at
                        // all (bundle suspect / override pricing):
                        // never let its simulated PnL reach the live
                        // breaker; clear any stale coverage stamp.
                        self.exec
                            .mark_diverged(
                                mint,
                                "paper-only open: live dispatch skipped (bundle suspect or override pricing)".to_string(),
                            )
                            .await;
                    }
                } else if allow_live {
                    self.live_buy(mint, curve, assoc_curve, mark, sol, creator)
                        .await;
                } else {
                    // AUD-002: same as the failed-watch arm — the
                    // paper open has no live leg by design.
                    self.exec
                        .mark_diverged(
                            mint,
                            "paper-only open: live dispatch skipped (bundle suspect or override pricing)".to_string(),
                        )
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
    fn confirm_metrics_carries_the_structured_tape() {
        let micro = MicroSignals {
            buys: 6,
            sells: 2,
            buy_ratio: Some(0.75),
            buy_volume_base: 6_000_000,
            sell_volume_base: 1_000_000,
            unique_traders: 5,
            top_wallet_share_pct: Some(40.0),
            creator_net_base: -500_000,
            velocity_delta: Some(0.3),
        };
        let m = confirm_metrics(
            30_000.0,
            6,
            0.9,
            120,
            Some(125.0),
            Some(3),
            Some(9.0),
            Some(1_250.0),
            micro,
            Some(22.0),
            Some(6.5),
        );
        assert_eq!(m.liquidity_usd, 30_000.0);
        assert_eq!(m.buy_velocity_5s, 6);
        assert_eq!(m.volume_usd, Some(1_250.0));
        assert_eq!(m.buy_ratio, Some(0.75));
        assert_eq!(m.unique_traders, 5);
        assert_eq!(m.top_wallet_share_pct, Some(40.0));
        assert_eq!(m.creator_net_base, -500_000);
        assert_eq!(m.velocity_delta, Some(0.3));
        assert_eq!(m.top_holder_pct, Some(22.0));
        assert_eq!(m.entry_flip_loss_pct, Some(6.5));
        // Unknowns stay unknown (null), never zero-as-data.
        let empty = MicroSignals {
            buys: 0,
            sells: 0,
            buy_ratio: None,
            buy_volume_base: 0,
            sell_volume_base: 0,
            unique_traders: 0,
            top_wallet_share_pct: None,
            creator_net_base: 0,
            velocity_delta: None,
        };
        let m = confirm_metrics(
            30_000.0, 5, 0.9, 120, None, None, None, None, empty, None, None,
        );
        assert_eq!(m.buy_ratio, None);
        assert_eq!(m.top_holder_pct, None);
        assert_eq!(m.velocity_delta, None);
        assert_eq!(m.entry_flip_loss_pct, None);
    }

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
            defers: 0,
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
