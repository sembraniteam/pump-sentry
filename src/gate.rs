//! Entry gate service: watchlist signals, buyer-quorum polling, and
//! confirmed opens (paper + live dispatch kickoff).
//!
//! Moved verbatim from `pipeline.rs` (same `impl Pipeline` block, new
//! home). Behavior unchanged.

use std::collections::HashMap;

use alert::Alerter;
use auditor::RugCheckError;
use curve::{CurveError, PriceTick};
use listener::{
    BundleVerdict, EntryGate, GateOutcome, ScreenError, SwapRecord,
};
use risk_engine::{
    Action, SCALP_TP_MIN, StrategyConfig, StrategyMode, TokenMetrics, evaluate_heuristic_fallback,
    resolve_strategy,
};
use simulator::Side;
use tracing::{debug, info, warn};

use crate::pipeline::{AlertSink, Auditor, LiveSink, Pipeline, ReserveFeed, Screener};
use crate::settings::now_secs;

/// One curve observation: `(mint, signal, swaps+watermark+truncated or error)`.
type Observation = (
    String,
    PendingSignal,
    Result<(Vec<SwapRecord>, Option<String>, bool), ScreenError>,
);

/// Concurrent RPC fanout ceiling per gate phase: a full `join_all` over a
/// thousand-entry watchlist opens a thousand concurrent RPCs (rate-limit
/// mass timeouts); 16 at a time keeps rounds moving without hammering.
/// The live width adapts per round (see `GateState::fanout`): slow rounds
/// halve it toward [`GATE_FANOUT_MIN`], fast rounds recover toward this
/// ceiling one step at a time.
const GATE_FANOUT: usize = 16;
/// Floor for adaptive gate fanout: serial progress beats a wedged node.
const GATE_FANOUT_MIN: usize = 1;
/// Round slower than this (seconds, phases 1+3 wall time) halves the
/// gate fanout for the next round (rate-limit/backoff signal).
const SLOW_GATE_ROUND_SECS: u64 = 10;


/// Map screened dev history to a 0.4–1.0 trust score for the risk
/// input. Screening already rejected young/prolific wallets; survivors
/// ramp from 0.7 at exactly `min_age` to 1.0 at 4× `min_age`, minus 0.05
/// per prior launch. The 0.4 floor equals the heuristic PASS cutoff, so
/// screening and scoring cannot disagree on the worst devs. A disabled
/// age gate (`min_age_secs == 0`) scores full trust (nothing to ramp on).
/// Unknown age scores 0.6 (below any screened survivor — unreachable in
/// production since screening rejects ageless wallets outright).
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
    (0.6 + 0.4 * age_factor - 0.05 * prior_launches as f64).clamp(0.4, 1.0)
}

/// Resolve an entry price and sizing liquidity from a reserve read, plus
/// the paper-only flag (true when the price came from the manual
/// `QUOTE_USD` override — the live mirror must stay out).
///
/// Live tick wins; on failure the manual override applies; `None` means
/// "cannot price — skip the signal". Single source: callers must take
/// the flag from here, never re-derive it from `tick_result.is_err()`
/// (a future caller doing that would let a static price leak live).
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
            // A feed print of NaN/zero/negative is worse than none: it
            // would open paper (and mirror live) at a garbage price.
            warn!(mint = %bad.mint, price = bad.price_usd, liq = bad.liquidity_usd,
                "ignoring degenerate curve tick; dropping signal");
            None
        }
        Err(e) => match quote_override {
            Some(quote) if quote.is_finite() && quote > 0.0 => {
                warn!(error = %e, "curve read failed; pricing via QUOTE_USD override");
                let liquidity = if fallback_liquidity.is_finite() && fallback_liquidity >= 0.0 {
                    fallback_liquidity
                } else {
                    0.0
                };
                Some((quote, liquidity, true))
            }
            _ => {
                warn!(error = %e, "curve read failed and no valid override; dropping signal");
                None
            }
        },
    }
}

/// Stop-loss floor for a venue tier: twice the per-side fee (round
/// trip) plus a 4-point impact margin, negated. A tighter stop on a
/// custom tier loses to venue costs before price action matters:
/// 125 bps → −6.5% (never binds: profiles stop at −8 or wider),
/// 345 bps → −10.9% (widens a −8 stop). Pure and unit-tested.
fn fee_aware_sl_floor(sl_percent: f64, venue_fee_bps: f64) -> f64 {
    sl_percent.min(-(2.0 * venue_fee_bps / 100.0 + 4.0))
}

/// Confirm-open request: one gate-confirmed signal plus its quorum.
/// Bundles the four `confirm_open` inputs so new context cannot grow the
/// arity again.
#[derive(Debug, Clone, Copy)]
struct OpenRequest<'a> {
    mint: &'a str,
    signal: &'a PendingSignal,
    buyers: u32,
    allow_live: bool,
}

/// Tracked-open request: priced confirm ready for paper + live dispatch.
/// Bundles the seven `open_tracked` inputs for the same reason.
#[derive(Debug, Clone, Copy)]
struct TrackedOpen<'a> {
    mint: &'a str,
    curve: &'a str,
    assoc_curve: &'a str,
    entry_quote: f64,
    liquidity_usd: f64,
    profile: &'a risk_engine::RiskProfile,
    allow_live: bool,
    creator: Option<&'a str>,
    /// Venue tier read with the pricing tick (same chain state, one
    /// fewer RPC than a separate fee call). `None` keeps the standard
    /// tier — and is forced `None` for synthetic (override-priced)
    /// opens, whose paper never feeds the breaker anyway.
    venue_fee_bps: Option<f64>,
    /// SOL/USD stamped at confirm time (shared by paper + live sizing).
    /// Captured here — not re-read at dispatch — so the amount and the
    /// guard price one market, not two reads seconds apart. Freshness
    /// is re-checked at USE time (`age_ok`); a reading aged past the
    /// feed window by dispatch fails closed like a stale feed.
    sol: Option<(f64, u64)>,
    /// Trailing giveback resolved per strategy (runner uses its own wide
    /// trail; everything else uses the shared setting).
    trail_percent: f64,
}

/// Watchlist entry context alongside [`EntryGate`] decisions.
#[derive(Debug, Clone)]
pub struct PendingSignal {
    /// Bonding-curve address (base58) for swap observation.
    pub curve: String,
    /// Token creator (base58), when the launch carried one: required
    /// for v3 live dispatch (cross-checked against curve state).
    /// `None` (legacy v1 launch) fails live closed — paper only.
    pub creator: Option<String>,
    /// Curve ATA address (base58) for live exits.
    pub assoc_curve: String,
    /// RugCheck score captured at signal time (`None` while the audit
    /// is still pending retry — RugCheck 400s fresh mints during
    /// indexing; see `AUDIT_RETRY_SECS`). Quorum can never confirm
    /// without a score (fail-closed at confirm, deadline aborts).
    pub audit_score: Option<u32>,
    /// Last audit attempt (`now_secs`): throttles re-polls.
    pub last_audit_secs: u64,
    /// Creation slot for the bundle rule.
    pub creation_slot: u64,
    /// Signature watermark for incremental curve reads.
    pub last_sig: Option<String>,
    /// Continuous dev-trust score (0.4–1.0) from the launch-time screen,
    /// carried to confirm time for the risk input. On v2 launches with a
    /// distinct creator this is the WORST of both insider screens.
    pub dev_trust: f64,
}

/// Entry gate plus signal context under one lock (no lock ordering).
pub struct GateState {
    /// Buyer-quorum state machine.
    pub gate: EntryGate,
    /// Per-mint context for pending entries.
    pub meta: HashMap<String, PendingSignal>,
    /// Gate-poll mutual exclusion across overlapping rounds. Set while a
    /// round runs, with the claim timestamp: a claim older than
    /// [`STALE_POLL_SECS`] is treated as leaked (owning task cancelled
    /// before [`Pipeline::end_poll`]) and may be taken over, so the gate
    /// self-heals instead of stalling forever.
    pub poll_busy: bool,
    /// `now_secs` of the current `poll_busy` claim (0 when free).
    pub poll_started_secs: u64,
    /// Live fanout width for this round's phase 1+3 (adaptive between
    /// [`GATE_FANOUT_MIN`] and [`GATE_FANOUT`]; see `poll_gates`).
    pub fanout: usize,
}

/// Age after which a gate-poll claim is considered leaked and eligible
/// for takeover. Normal rounds finish in seconds (phase budgets top out
/// near 15s); 30s of silence means the owning task died mid-round, so
/// the gate self-heals instead of stalling confirms for two minutes.
pub const STALE_POLL_SECS: u64 = 30;

/// Minimum spacing between audit retries for one watchlisted mint.
/// RugCheck 400s on seconds-old mints (indexing latency) resolve in
/// seconds — faster polling would spam a struggling third party.
pub const AUDIT_RETRY_SECS: u64 = 3;

/// Day-loss circuit breaker: halts *new entries* once the UTC-day
/// realized net passes the configured loss cap. Exits always keep
/// working (a breaker must never trap open risk). Pure time math.
#[derive(Debug, Clone, Copy)]
pub struct DailyBreaker {
    /// UTC day bucket (`unix_secs / 86400`) of the accumulated loss.
    pub day: u64,
    /// Realized net PnL accumulated within `day` (negative = losing).
    pub day_net: f64,
}

impl DailyBreaker {
    /// Fresh breaker (today, flat).
    pub fn new(now_secs: u64) -> Self {
        Self { day: now_secs / 86_400, day_net: 0.0 }
    }

    /// Fold one closed trade into the day bucket (rolls the day first).
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

    /// True when new entries must halt: positive cap configured and the
    /// day net is at or beyond it. A stale bucket never trips (rolls on
    /// read, same as on record).
    pub fn tripped(&mut self, cap_usd: f64, now_secs: u64) -> bool {
        let day = now_secs / 86_400;
        if day != self.day {
            self.day = day;
            self.day_net = 0.0;
        }
        cap_usd.is_finite() && cap_usd > 0.0 && self.day_net <= -cap_usd
    }
}

impl GateState {
    /// Empty watchlist with the poll slot free.
    pub fn new(gate: EntryGate) -> Self {
        Self {
            gate,
            meta: HashMap::new(),
            poll_busy: false,
            poll_started_secs: 0,
            fanout: GATE_FANOUT,
        }
    }
}

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {

    /// Watchlisted mint count (for metrics/tests).
    pub async fn pending_count(&self) -> usize {
        self.gate_state.lock().await.gate.pending_count()
    }

    /// Claim the gate-poll slot; false when a round is already running.
    /// A claim older than [`STALE_POLL_SECS`] is treated as leaked (the
    /// owning task was cancelled before [`Pipeline::end_poll`]) and taken
    /// over with a loud log.
    pub async fn try_begin_poll(&self) -> bool {
        let mut state = self.gate_state.lock().await;
        let now = now_secs();
        if state.poll_busy && now.saturating_sub(state.poll_started_secs) < STALE_POLL_SECS {
            return false;
        }
        if state.poll_busy {
            warn!(
                "stale gate-poll claim (age {:?}s); taking over",
                now.saturating_sub(state.poll_started_secs)
            );
        }
        state.poll_busy = true;
        state.poll_started_secs = now;
        true
    }

    /// Release the gate-poll slot after a round.
    pub async fn end_poll(&self) {
        let mut state = self.gate_state.lock().await;
        state.poll_busy = false;
        state.poll_started_secs = 0;
    }

    /// Audit → dev screen → watchlist. Never opens here: real demand must
    /// show up in [`Pipeline::poll_gates`] first. Skips (with a log) on
    /// any fail-closed condition.
    pub async fn on_launch(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        dev: &str,
        creator: Option<&str>,
        slot: u64,
    ) {
        // Invariant: every address here arrives as a stringified, already
        // parsed `Pubkey` (production callers forward parser output, which
        // is typed). Stubs pass placeholders like "MINT" — downstream
        // screening parses defensively (`parse_address` at each RPC
        // boundary), so a bad string degrades to per-round warnings until
        // the gate sweep, never to a mispriced open.
        // 1. Audit — fail closed, with indexing-latency retry. A fresh
        // mint's 400 ("not indexed yet") must not burn the launch at
        // second 0: retryable failures watchlist with no score and
        // re-poll on the gate cadence until the deadline aborts.
        // Policy rejections and misconfiguration still drop at once.
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

        // 2. Developer history — fail closed. Survivors fold into a
        // continuous trust score (see `dev_reputation_score`) rather than
        // a flat 1.0, so barely-old-enough wallets price worse than
        // long-aged ones.
        let dev_trust = match self.screen_wallet(mint, dev, "dev").await {
            Some(trust) => trust,
            None => return,
        };

        // 2b. v2 creator history — fail closed AND. `create_v2` launches
        // carry a creator distinct from the payer/launcher, with its own
        // rug history: screening only the payer lets serial creators
        // through. The signal carries the worst of both trusts, and the
        // gate watches both wallets for dumps.
        let mut trust = dev_trust;
        let distinct_creator = creator.filter(|c| *c != dev);
        if let Some(creator) = distinct_creator {
            match self.screen_wallet(mint, creator, "creator").await {
                Some(creator_trust) => trust = trust.min(creator_trust),
                None => return,
            }
        }

        // 3. Watchlist — demand confirmation happens in poll_gates.
        let mut state = self.gate_state.lock().await;
        if !state.gate.submit(mint, dev, now_secs()) {
            warn!(%mint, "bad dev address; skipping");
            return;
        }
        if let Some(creator) = distinct_creator
            && !state.gate.add_cohort(mint, creator)
        {
            // Half-watched insiders are worse than none: unwind instead
            // of tracking a launch whose creator dumps unseen.
            state.gate.remove(mint);
            warn!(%mint, "creator cohort untrackable; skipping (fail-closed)");
            return;
        }
        state.meta.insert(
            mint.to_string(),
            PendingSignal {
                curve: curve.to_string(),
                assoc_curve: assoc_curve.to_string(),
                audit_score,
                last_audit_secs,
                creation_slot: slot,
                last_sig: None,
                creator: creator.map(|c| c.to_string()),
                dev_trust: trust,
            },
        );
        match audit_score {
            Some(score) => info!(%mint, score, "watchlisted; awaiting buyer quorum"),
            None => info!(%mint, "watchlisted without audit score; awaiting buyer quorum + audit retry"),
        }
    }

    /// Screen one insider wallet (launcher or creator): audit-logged
    /// verdict mapped to a trust score. `None` means skip-the-signal
    /// (reject or transport error — both fail closed).
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
                warn!(%mint, %role, error = %e, "insider screen error; skipping (fail-closed)");
                None
            }
        }
    }

    /// Drive every watchlisted mint one observation round: confirm opens
    /// (with live-measured velocity), aborts drop, timeouts sweep.
    ///
    /// Three phases: (1) concurrent curve reads (no head-of-line block on
    /// slow RPC), (2) sequential gate state machine (deterministic, single
    /// critical section per mint), (3) concurrent bundle forensics +
    /// confirms (the slowest RPC path, never serial).
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
        // Phase 1: concurrent observations, bounded so a large
        // watchlist cannot open one RPC per mint at once. Width is the
        // adaptive live value, not the ceiling.
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
        // Phase 2: gate state machine (fast, sequential for determinism).
        enum GateStep {
            Confirmed(u32, PendingSignal),
            Aborted(String),
            Waiting,
            Stale,
        }
        let mut confirmed: Vec<(String, PendingSignal, u32)> = Vec::new();
        for (mint, mut signal, observed) in observations {
            let (swaps, newest, truncated) = match observed {
                Ok(observed) => observed,
                Err(e) => {
                    warn!(%mint, error = %e, "curve observation failed; keeping watch");
                    continue;
                }
            };
            if truncated {
                // Partial view: never confirm on it and never advance the
                // watermark — the next round re-reads the same window once
                // velocity cools. Sustained truncation hits the deadline
                // (abort), the fail-closed answer to a gap.
                debug!(%mint, "curve observation truncated; holding watch without quorum");
                continue;
            }
            // Audit retry (P1.2): a `None` score means RugCheck was still
            // indexing at launch. Re-poll throttled to `AUDIT_RETRY_SECS`;
            // the gate deadline aborts unresolvable mints (fail-closed
            // after the window, never at second 0). The watermark below
            // is deliberately NOT advanced while the score is missing:
            // post-resolve quorum must observe the whole window,
            // including insider dumps during the wait.
            // Snapshot the score BEFORE retrying: the staleness check
            // below must compare meta against the pre-retry snapshot, or
            // our own retry would read as a concurrent relaunch.
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
            // Single critical section: skip the round when a concurrent
            // relaunch replaced the snapshot (never clobber fresh signal
            // context), then observe and remove atomically on terminal
            // outcomes so newborn entries cannot be deleted.
            let step = {
                let mut state = self.gate_state.lock().await;
                let fresh = state.meta.get(&mint);
                // Any identity drift (curve, ATA, creator, score, slot)
                // means a concurrent relaunch replaced the snapshot: never
                // clobber fresh signal context or confirm on stale reads.
                // The score compares against its SNAPSHOT value (a retry
                // above may have filled it in — that is progress, and
                // must not read as a relaunch).
                let stale = fresh.is_some_and(|current| {
                    current.curve != signal.curve
                        || current.assoc_curve != signal.assoc_curve
                        || current.creator != signal.creator
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
                        // Never regress a concurrently-advanced watermark:
                        // overlapping rounds snapshot the same entry, and
                        // the older round must not overwrite the newer
                        // round's `last_sig` (it would re-read the window
                        // and stall quorum; buyer counts stay safe via set
                        // semantics either way). Audit progress merges
                        // onto whichever watermark is current.
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
                        match state.gate.observe(&mint, &swaps, now_secs()) {
                            GateOutcome::Confirm { buyers } => {
                                state.meta.remove(&mint);
                                GateStep::Confirmed(buyers, signal.clone())
                            }
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
                GateStep::Confirmed(buyers, sig) => confirmed.push((mint, sig, buyers)),
                GateStep::Aborted(reason) => {
                    info!(%mint, %reason, "gate abort");
                }
                GateStep::Waiting | GateStep::Stale => {}
            }
        }
        // Phase 3: bundle forensics + opens, bounded like phase 1 (the
        // slowest RPC path must never fan out without limit either).
        if !confirmed.is_empty() {
            use futures_util::stream::{self, StreamExt};
            stream::iter(confirmed.into_iter().map(|(mint, signal, buyers)| async move {
                // Bundle forensics before money moves: reject drops,
                // suspect trades paper-only, safe runs the full flow.
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
                        self.confirm_open(&OpenRequest { mint: &mint, signal: &signal, buyers, allow_live: false }).await;
                    }
                    Ok(_) => {
                        self.confirm_open(&OpenRequest { mint: &mint, signal: &signal, buyers, allow_live: true }).await;
                    }
                    Err(e) => {
                        warn!(%mint, error = %e, "bundle screen error; dropping (fail-closed)");
                    }
                }
            }))
            .buffered(fanout.max(1))
            .collect::<Vec<()>>()
            .await;
        }
        // Adapt the fanout for the next round: a slow round (phases 1+3
        // wall time past the threshold) means the node is throttling us,
        // so halve toward serial; a fast round recovers one step toward
        // the ceiling. Additive-increase / multiplicative-decrease keeps
        // throughput high on healthy nodes without hammering sick ones.
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
        // Collect under one short critical section: iterating while
        // re-locking per mint depends on temporary-guard drops and would
        // self-deadlock the moment a refactor holds the guard across.
        let timeouts = { self.gate_state.lock().await.gate.sweep_timeouts(now) };
        for (mint, outcome) in timeouts {
            // Only drop meta when no fresh entry replaced it mid-sweep.
            let mut state = self.gate_state.lock().await;
            if !state.gate.contains(&mint) {
                state.meta.remove(&mint);
            }
            drop(state);
            if let GateOutcome::Abort { reason } = outcome {
                info!(%mint, %reason, "gate abort");
            }
        }
    }

    /// Open a confirmed signal: fresh pricing, risk with measured buyer
    /// velocity and screened developer trust, then paper + live dispatch.
    async fn confirm_open(&self, req: &OpenRequest<'_>) {
        let mint = req.mint;
        let signal = req.signal;
        let buyers = req.buyers;
        // Pricing + venue tier from one chain state (single account read
        // for both — a tick and a tier from two different slots could
        // price one market and fee another).
        let (tick_result, venue_fee) = match self
            .reserves
            .fetch_tick_with_fee(mint, &signal.curve)
            .await
        {
            Ok((tick, fee)) => (Ok(tick), fee),
            Err(e) => (Err(e), None),
        };
        // A manual QUOTE_USD fallback is for paper/tests only: never let
        // real funds ride on an operator-set price when the feed is down.
        // The flag comes from the resolver itself (single source), so a
        // future caller cannot misderive it from `tick_result.is_err()`.
        let Some((entry_quote, liquidity_usd, priced_via_override)) = resolve_entry_with_flag(
            tick_result,
            self.settings.quote_usd,
            self.settings.liquidity_usd,
        ) else {
            warn!(%mint, "no live price and QUOTE_USD unset; dropping signal");
            return;
        };

        // Mint-decimals gate: every sizing path below scales by 10^6
        // (Pump.fun standard). A non-6 mint would misprice entries and
        // live orders by 10^Δ — drop it loudly instead of guessing.
        // Unreadable decimals fail closed for the same reason.
        // Locked to the live sizing scale at compile time: changing one
        // without the other diverges paper from dispatch silently.
        const _: () = assert!(crate::live::TOKEN_DECIMALS == 6);
        match self.reserves.token_decimals(mint).await {
            Ok(6) => {}
            Ok(other) => {
                warn!(%mint, decimals = other, "non-6-decimal mint; sizing assumes 6 — dropping signal");
                return;
            }
            Err(e) => {
                warn!(%mint, error = %e, "mint decimals unreadable; dropping signal (fail-closed)");
                return;
            }
        }

        // Developer passed a real on-chain history screen (trust carried
        // on the signal from launch time).
        let Some(score) = signal.audit_score else {
            // Unreachable via poll_gates (quorum never confirms without
            // a score) — fail closed rather than price risk on nothing.
            warn!(%mint, "confirm reached without an audit score; dropping (fail-closed)");
            return;
        };
        let metrics = TokenMetrics {
            liquidity_usd,
            buy_velocity_5s: buyers,
            dev_reputation: signal.dev_trust,
            rugcheck_score: score,
        };
        let profile = match &self.engine {
            Some(engine) => engine.evaluate(&metrics).await.profile,
            None => evaluate_heuristic_fallback(
                metrics.liquidity_usd,
                metrics.buy_velocity_5s,
                metrics.dev_reputation,
                metrics.rugcheck_score,
            ),
        };
        if profile.action != Action::Buy {
            info!(%mint, buyers, score, "risk PASS at confirm");
            return;
        }
        // Multi-strategy execution: rewrite the BUY profile per the
        // startup-parsed `self.strategy` (adaptive = verbatim, scalp = TP
        // band + full exits, runner = forced moonbag + trailing).
        let strategy = StrategyConfig {
            mode: self.strategy,
            runner_moonbag_percent: self.settings.runner_moonbag_percent,
        };
        let profile = resolve_strategy(&profile, &strategy);
        info!(%mint, mode = ?strategy.mode, tp = profile.tp_percent,
            moonbag = profile.moonbag_percent, trailing = profile.trailing_stop,
            "strategy resolved");
        // Scalp never widens a below-band TP silently: the model wanted
        // out early, and forcing it to wait turns winners into losers.
        if strategy.mode == StrategyMode::ScalpQuick && profile.tp_percent < SCALP_TP_MIN {
            warn!(%mint, tp = profile.tp_percent,
                "scalp TP below +25% band; honoring the model's early exit");
        }
        // Runners trail wide (survive pullbacks toward 5x–10x); the scalp
        // and adaptive paths share the standard trail setting.
        let trail_percent = match strategy.mode {
            StrategyMode::Runner => self.settings.runner_trail_percent,
            _ => self.settings.trail_percent,
        };
        // Single SOL reading for both legs (see `TrackedOpen::sol`).
        let sol = self.sol_feed.stamped_price();
        // Override-priced opens are synthetic paper (no market behind
        // the quote): mark the mint so its PnL never feeds the breaker.
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
            allow_live: req.allow_live && !priced_via_override,
            creator: signal.creator.as_deref(),
            // Synthetic paper has no market behind its quote: no tier
            // to register (its PnL never feeds the breaker anyway).
            venue_fee_bps: if priced_via_override { None } else { venue_fee },
            sol,
            trail_percent,
        })
        .await;
    }

    /// Open a paper position at a confirmed price, watch for exits, and
    /// mirror to live dispatch when enabled (and allowed: bundle
    /// suspects stay paper-only).
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
        // Circuit breaker first: past the day-loss cap, no new risk —
        // paper or live. Exits are unaffected (handled on ticks).
        if self.daily.lock().await.tripped(self.settings.max_daily_loss_usd, now_secs()) {
            warn!(%mint, cap = self.settings.max_daily_loss_usd, "daily loss breaker tripped; skipping open");
            return;
        }
        let mut portfolio = self.portfolio.lock().await;
        // Portfolio heat: cap concurrent exposure and enforce per-mint
        // cooldown so correlated launches cannot stack the book.
        if portfolio.position_ids().len() >= self.settings.max_open_positions {
            info!(%mint, max = self.settings.max_open_positions, "heat limit: skipping");
            return;
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
        // Per-mint venue fee (custom creator tiers), read with the pricing
        // tick upstream — no second fetch here. `None` keeps the standard
        // 125 bps tier; a custom tier re-prices this mint's paper fills
        // to match the live dispatch exactly.
        if let Some(bps) = req.venue_fee_bps {
            portfolio.set_venue_fee_for_mint(mint, bps);
            if (bps - crate::live::VENUE_FEE_BPS as f64).abs() > f64::EPSILON {
                info!(%mint, bps, "custom venue fee tier registered for paper ledger");
            }
        }
        // Fee-aware stop: a stop tighter than round-trip venue costs plus
        // margin gets eaten by fees, not alpha. Standard tiers never bind
        // (floor −6.5% sits inside −8/−10); custom tiers widen the stop
        // instead of booking structural losers. TP is untouched (far
        // above costs on every tier).
        let mut profile = *profile;
        if let Some(bps) = req.venue_fee_bps {
            let widened = fee_aware_sl_floor(profile.sl_percent, bps);
            if widened < profile.sl_percent {
                info!(%mint, sl = profile.sl_percent, widened, bps,
                    "custom-tier stop widened past round-trip costs");
                profile.sl_percent = widened;
            }
        }
        match portfolio.open_position_params(&simulator::OpenPositionParams {
            mint,
            side: Side::Long,
            quote: entry_quote,
            notional_usd: self.settings.notional_usd,
            liquidity_usd,
            tp_percent: profile.tp_percent,
            sl_percent: profile.sl_percent,
            trailing: profile.trailing_stop,
            trail_percent,
            timeout_seconds: profile.timeout_seconds,
            moonbag_percent: profile.moonbag_percent,
            now_secs: now,
        }) {
            Ok(id) => {
                let snap = portfolio.snapshot(&HashMap::new());
                info!(%mint, id, entry = format!("{entry_quote:.10}"),
                    tp = profile.tp_percent, sl = profile.sl_percent,
                    equity = format!("{:.2}", snap.equity), "paper position opened");
                // Emit after the guard drops (below): alert dispatch must
                // never run while the portfolio lock is held.
                let card = Alerter::format_entry(
                    mint,
                    entry_quote,
                    profile.tp_percent,
                    profile.sl_percent,
                    self.settings.notional_usd,
                );
                drop(portfolio);
                self.emit(card);
                // No feed, no live leg: without the exit ticker a real bag
                // would have no TP/SL automation, so the live mirror stays
                // out (paper still opens for manual review).
                if let Err(e) = self.reserves.watch(mint, curve).await {
                    warn!(%mint, error = %e, "ticker watch failed; paper-only, live mirror skipped");
                    if allow_live {
                        self.exec
                            .mark_diverged(mint, "live buy skipped: ticker watch failed".to_string())
                            .await;
                        self.emit(format!("ticker watch failed on {mint}; paper open WITHOUT live cover"));
                    }
                } else if allow_live {
                    self.live_buy(
                        mint,
                        curve,
                        assoc_curve,
                        entry_quote,
                        liquidity_usd,
                        sol,
                        creator,
                    )
                    .await;
                }
            }
            Err(e) => warn!(%mint, error = %e, "paper open failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_aware_sl_floor_leaves_standard_stops() {
        // 125 bps → floor −6.5%: profiles stop at −8 or wider already.
        assert_eq!(fee_aware_sl_floor(-8.0, 125.0), -8.0);
        assert_eq!(fee_aware_sl_floor(-10.0, 125.0), -10.0);
    }

    #[test]
    fn fee_aware_sl_floor_widens_custom_tiers() {
        // 345 bps → floor −10.9%: a −8 stop would lose to venue costs.
        assert_eq!(fee_aware_sl_floor(-8.0, 345.0), -10.9);
        // Already-wide stops are untouched (never tightened).
        assert_eq!(fee_aware_sl_floor(-15.0, 345.0), -15.0);
    }
}
