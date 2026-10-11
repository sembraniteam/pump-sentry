//! Event processing: launch handling and tick-driven exits.
//!
//! [`Pipeline`] is generic over [`Auditor`] and [`ReserveFeed`] so unit
//! tests inject stubs instead of network clients. Pure helpers
//! ([`resolve_entry`]) stay free functions for direct testing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alert::Alerter;
use auditor::{RugCheckClient, RugCheckError, RugCheckReport};
use curve::{CurveError, CurveTicker, PriceTick};
use listener::{
    BundleAnalysis, BundleVerdict, DevVerdict, EntryGate, GateOutcome, MigrationReport,
    RpcScreener, ScreenError, SwapRecord,
};
use migration::{PoolTick, VaultTicker};
use risk_engine::{Action, RiskEngine, TokenMetrics, evaluate_heuristic_fallback};
use simulator::{ExitReason, PaperPortfolio, Side, check_exit};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::live::{LiveBuyParams, LiveError, LivePosition, LiveTrader, PendingRecheck, RecheckOutcome, SELL_MAX_ATTEMPTS, SELL_RETRY_COOLDOWN};
use crate::settings::{Settings, now_secs};

/// Token audit source. The blanket impl covers the real client.
pub trait Auditor {
    /// Evaluate a mint; `Err` (rejection or transport) means skip.
    async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError>;
}

impl Auditor for RugCheckClient {
    async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        self.evaluate_token(mint).await
    }
}

/// Bonding-curve reserve source. The blanket impl covers the real ticker.
pub trait ReserveFeed {
    /// One-shot reserve read for a curve.
    async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError>;
    /// Watch a curve for exit polling.
    async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError>;
    /// Stop watching; true when an entry was removed.
    async fn unwatch(&self, mint: &str) -> bool;
}

impl ReserveFeed for CurveTicker {
    async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError> {
        self.fetch_tick(mint, curve).await
    }

    async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError> {
        self.watch(mint, curve).await
    }

    async fn unwatch(&self, mint: &str) -> bool {
        self.unwatch(mint).await
    }
}

/// Live dispatch sink. The blanket impl covers [`LiveTrader`]; tests
/// inject a stub. Only used when the binary runs with `--live`.
pub trait LiveSink {
    /// Buy per [`LiveBuyParams`] via Jito bundle.
    /// Returns the tracked position plus the bundle UUID.
    async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError>;

    /// Sell a tracked position at `price_usd`; returns the bundle UUID.
    async fn sell(
        &self,
        position: &LivePosition,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<String, LiveError>;

    /// True when at least one buy awaits confirmation (default: none).
    fn has_pending(&self) -> bool {
        false
    }

    /// Re-poll pending buys once (default: nothing pending).
    async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        Vec::new()
    }
}

impl LiveSink for LiveTrader {
    async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError> {
        self.buy(req).await
    }

    async fn sell(
        &self,
        position: &LivePosition,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<String, LiveError> {
        self.sell(position, price_usd, sol_usd).await
    }

    fn has_pending(&self) -> bool {
        self.has_pending()
    }

    async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        self.recheck_pending().await
    }
}

/// Alert sink: fire-and-forget trade cards. The blanket impl covers
/// [`Alerter`]; tests inject a recording stub.
pub trait AlertSink {
    /// Dispatch a preformatted card without blocking.
    fn alert(&self, message: String);
}

impl AlertSink for Alerter {
    fn alert(&self, message: String) {
        self.send(message);
    }
}

/// On-chain screening source: dev history plus incremental curve reads.
/// The blanket impl covers [`RpcScreener`]; tests inject a stub.
pub trait Screener {
    /// Developer history verdict; `Err` means fail-closed skip.
    async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError>;
    /// Swap records for a curve since `since_sig`, plus the new watermark.
    async fn observe_curve(
        &self,
        mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>), ScreenError>;
    /// Bundle forensics at confirm time; `Err` means fail-closed drop.
    async fn screen_bundle(
        &self,
        curve: &str,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ScreenError>;
    /// Migration hunt for a graduated curve; `None` when not found.
    async fn detect_migration(
        &self,
        curve: &str,
        mint: &str,
    ) -> Result<Option<MigrationReport>, ScreenError>;
}

impl Screener for RpcScreener {
    async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
        let dev = listener::parse_address(dev)?;
        self.dev_verdict(&dev, now_secs()).await
    }

    async fn observe_curve(
        &self,
        _mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>), ScreenError> {
        let curve = listener::parse_address(curve)?;
        self.curve_swaps(&curve, since_sig).await
    }

    async fn screen_bundle(
        &self,
        curve: &str,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ScreenError> {
        let curve = listener::parse_address(curve)?;
        self.bundle_verdict(&curve, creation_slot).await
    }

    async fn detect_migration(
        &self,
        curve: &str,
        mint: &str,
    ) -> Result<Option<MigrationReport>, ScreenError> {
        let curve = listener::parse_address(curve)?;
        let mint = listener::parse_address(mint)?;
        Ok(self
            .hunt_migration(&curve, &mint)
            .await?
            .map(|detected| detected.report()))
    }
}

/// One curve observation: `(mint, signal, swaps+watermark or error)`.
type Observation = (
    String,
    PendingSignal,
    Result<(Vec<SwapRecord>, Option<String>), ScreenError>,
);

/// Resolve an entry price and sizing liquidity from a reserve read.
///
/// Live tick wins; on failure the manual `QUOTE_USD` override applies;
/// `None` means "cannot price — skip the signal".
pub fn resolve_entry(
    tick: Result<PriceTick, CurveError>,
    quote_override: Option<f64>,
    fallback_liquidity: f64,
) -> Option<(f64, f64)> {
    match tick {
        Ok(tick) => Some((tick.price_usd, tick.liquidity_usd)),
        Err(_) => quote_override.map(|quote| (quote, fallback_liquidity)),
    }
}

/// Shared pipeline state for the event, gate, and ticker tasks.
///
/// Decomposed by responsibility: [`LiveExecution`] owns everything that
/// touches real funds (its generic `L` is the only live-typed parameter);
/// the rest is ingest, pricing, ledger, and observability. Splitting the
/// struct (rather than only documenting groups) keeps a paper-only build
/// from even naming live state.
///
/// The five generics (`A/R/L/S/T`) exist so unit tests inject stubs
/// instead of network clients; production uses the defaults.
pub struct Pipeline<
    A = RugCheckClient,
    R = CurveTicker,
    L = LiveTrader,
    S = RpcScreener,
    T = Alerter,
> {
    /// Runtime knobs.
    pub settings: Settings,
    /// Token audit source.
    pub auditor: A,
    /// LLM engine; `None` selects the heuristic path.
    pub engine: Option<RiskEngine>,
    /// Reserve reads and exit watch set.
    pub reserves: R,
    /// Virtual positions and cash.
    pub portfolio: Mutex<PaperPortfolio>,
    /// Real-funds execution state (dispatcher, fills, retries).
    pub exec: LiveExecution<L>,
    /// Mints already reported as migrated (report once).
    pub migrated: Mutex<HashSet<String>>,
    /// Post-migration pool watchlist (paper tracking only).
    pub followed: Mutex<HashMap<String, FollowState>>,
    /// Raydium vault ticker for followed pools.
    pub vaults: VaultTicker,
    /// On-chain screening source (dev history + curve reads).
    pub screener: S,
    /// Watchlist state plus per-mint signal context (single lock).
    pub gate_state: Mutex<GateState>,
    /// Day-loss circuit breaker (new entries halt past the cap; exits
    /// never block).
    pub daily: Mutex<DailyBreaker>,
    /// Trade cards; `None` silences alerting entirely.
    pub alerts: Option<T>,
}

/// Real-funds execution state, split out of [`Pipeline`] so paper-only
/// code paths never touch live bookkeeping. Owns the only live-typed
/// generic (`L`); everything else on `Pipeline` is funds-free.
pub struct LiveExecution<L = LiveTrader> {
    /// Live dispatcher; `None` keeps the run paper-only.
    pub live: Option<L>,
    /// Tracked live fills by mint.
    pub live_positions: Mutex<HashMap<String, LivePosition>>,
    /// Live-sell retry bookkeeping: `(attempts, last_attempt_secs)`.
    pub live_retry: Mutex<HashMap<String, (u32, u64)>>,
    /// Serializes live dispatches: probe sizing and tracking updates
    /// must not interleave across concurrent ticks.
    pub live_sell_lock: Mutex<()>,
}

impl<L> LiveExecution<L> {
    /// Empty execution state (no dispatcher: paper-only).
    pub fn paper() -> Self {
        Self {
            live: None,
            live_positions: Mutex::new(HashMap::new()),
            live_retry: Mutex::new(HashMap::new()),
            live_sell_lock: Mutex::new(()),
        }
    }

    /// Attach a dispatcher (`None` keeps the paper-only state).
    pub fn with_live(mut self, live: Option<L>) -> Self {
        self.live = live;
        self
    }
}

/// Post-migration spike multiple that fires the follow alert once.
pub const FOLLOW_SPIKE_MULT: f64 = 2.0;
/// Pool ticks older than this (seconds) are ignored: a stale baseline or
/// spike check is worse than none (thin/manipulated prints linger).
pub const POOL_TICK_STALE_SECS: u64 = 300;

/// Post-migration follow state per mint.
#[derive(Debug, Clone)]
pub struct FollowState {
    /// First pool-tick price (baseline).
    pub entry_price: f64,
    /// Whether the spike card already fired.
    pub alerted_spike: bool,
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
}

/// Watchlist entry context alongside [`EntryGate`] decisions.
#[derive(Debug, Clone)]
pub struct PendingSignal {
    /// Bonding-curve address (base58) for swap observation.
    pub curve: String,
    /// Token creator (base58), when the launch carried one: selects the
    /// extended mayhem swap shape for live orders. `None` keeps legacy.
    pub creator: Option<String>,
    /// Curve ATA address (base58) for live exits.
    pub assoc_curve: String,
    /// RugCheck score captured at signal time.
    pub score: u32,
    /// Creation slot for the bundle rule.
    pub creation_slot: u64,
    /// Signature watermark for incremental curve reads.
    pub last_sig: Option<String>,
}

/// Entry gate plus signal context under one lock (no lock ordering).
pub struct GateState {
    /// Buyer-quorum state machine.
    pub gate: EntryGate,
    /// Per-mint context for pending entries.
    pub meta: HashMap<String, PendingSignal>,
    /// Gate-poll mutual exclusion across overlapping rounds.
    pub poll_busy: std::sync::atomic::AtomicBool,
}

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
            poll_busy: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl<A, R, L, S, T> Pipeline<A, R, L, S, T> {
    /// Assemble shared state behind [`Arc`] for task sharing.
    #[allow(clippy::too_many_arguments)]
    pub fn shared(
        settings: Settings,
        auditor: A,
        engine: Option<RiskEngine>,
        reserves: R,
        portfolio: PaperPortfolio,
        live: Option<L>,
        screener: S,
        alerts: Option<T>,
        vaults: VaultTicker,
    ) -> Arc<Self> {
        let gate = EntryGate::new(settings.gate_window_secs, settings.gate_min_buyers);
        Arc::new(Self {
            settings,
            auditor,
            engine,
            reserves,
            portfolio: Mutex::new(portfolio),
            exec: LiveExecution::paper().with_live(live),
            migrated: Mutex::new(HashSet::new()),
            followed: Mutex::new(HashMap::new()),
            screener,
            gate_state: Mutex::new(GateState::new(gate)),
            daily: Mutex::new(DailyBreaker::new(now_secs())),
            alerts,
            vaults,
        })
    }

    /// Watchlisted mint count (for metrics/tests).
    pub async fn pending_count(&self) -> usize {
        self.gate_state.lock().await.gate.pending_count()
    }

    /// Claim the gate-poll slot; false when a round is already running.
    pub async fn try_begin_poll(&self) -> bool {
        self.gate_state
            .lock()
            .await
            .poll_busy
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Release the gate-poll slot after a round.
    pub async fn end_poll(&self) {
        self.gate_state
            .lock()
            .await
            .poll_busy
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {
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
        // 1. Audit — fail closed.
        let report = match self.auditor.evaluate_token(mint).await {
            Ok(report) => report,
            Err(RugCheckError::SecurityRejection(reason)) => {
                info!(%mint, "audit reject: {reason}");
                return;
            }
            Err(e) => {
                warn!(%mint, error = %e, "audit error; skipping (fail-closed)");
                return;
            }
        };

        // 2. Developer history — fail closed.
        match self.screener.screen_dev(dev).await {
            Ok(verdict) if verdict.pass => {
                info!(%mint, age = ?verdict.age_secs, priors = verdict.prior_launches, "dev screen pass");
            }
            Ok(verdict) => {
                info!(%mint, reason = %verdict.reason, "dev screen reject");
                return;
            }
            Err(e) => {
                warn!(%mint, error = %e, "dev screen error; skipping (fail-closed)");
                return;
            }
        }

        // 3. Watchlist — demand confirmation happens in poll_gates.
        let mut state = self.gate_state.lock().await;
        if !state.gate.submit(mint, dev, now_secs()) {
            warn!(%mint, "bad dev address; skipping");
            return;
        }
        state.meta.insert(
            mint.to_string(),
            PendingSignal {
                curve: curve.to_string(),
                assoc_curve: assoc_curve.to_string(),
                score: report.score,
                creation_slot: slot,
                last_sig: None,
                creator: creator.map(|c| c.to_string()),
            },
        );
        info!(%mint, score = report.score, "watchlisted; awaiting buyer quorum");
    }

    /// Drive every watchlisted mint one observation round: confirm opens
    /// (with live-measured velocity), aborts drop, timeouts sweep.
    ///
    /// Three phases: (1) concurrent curve reads (no head-of-line block on
    /// slow RPC), (2) sequential gate state machine (deterministic, single
    /// critical section per mint), (3) concurrent bundle forensics +
    /// confirms (the slowest RPC path, never serial).
    pub async fn poll_gates(&self) {
        let mints: Vec<(String, PendingSignal)> = {
            self.gate_state
                .lock()
                .await
                .meta
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        // Phase 1: concurrent observations.
        let observations: Vec<Observation> =
            futures_util::future::join_all(mints.into_iter().map(|(mint, signal)| async move {
                let observed = self
                    .screener
                    .observe_curve(&mint, &signal.curve, signal.last_sig.as_deref())
                    .await;
                (mint, signal, observed)
            }))
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
            let (swaps, newest) = match observed {
                Ok(observed) => observed,
                Err(e) => {
                    warn!(%mint, error = %e, "curve observation failed; keeping watch");
                    continue;
                }
            };
            // Single critical section: skip the round when a concurrent
            // relaunch replaced the snapshot (never clobber fresh signal
            // context), then observe and remove atomically on terminal
            // outcomes so newborn entries cannot be deleted.
            let step = {
                let mut state = self.gate_state.lock().await;
                let fresh = state.meta.get(&mint);
                let stale = fresh.is_some_and(|current| {
                    current.curve != signal.curve
                        || current.score != signal.score
                        || current.creation_slot != signal.creation_slot
                });
                if stale {
                    GateStep::Stale
                } else {
                    if let Some(sig) = newest {
                        signal.last_sig = Some(sig);
                        state.meta.insert(mint.clone(), signal.clone());
                    }
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
            };
            match step {
                GateStep::Confirmed(buyers, sig) => confirmed.push((mint, sig, buyers)),
                GateStep::Aborted(reason) => {
                    info!(%mint, %reason, "gate abort");
                }
                GateStep::Waiting | GateStep::Stale => {}
            }
        }
        // Phase 3: concurrent bundle forensics + opens.
        if !confirmed.is_empty() {
            futures_util::future::join_all(confirmed.into_iter().map(|(mint, signal, buyers)| async move {
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
            .await;
        }
        let now = now_secs();
        for (mint, outcome) in self.gate_state.lock().await.gate.sweep_timeouts(now) {
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
        let tick_result = self.reserves.fetch_tick(mint, &signal.curve).await;
        // A manual QUOTE_USD fallback is for paper/tests only: never let
        // real funds ride on an operator-set price when the feed is down.
        let priced_via_override = tick_result.is_err();
        if priced_via_override {
            warn!(%mint, "reserve read failed; trying QUOTE_USD override (paper-only)");
        }
        let Some((entry_quote, liquidity_usd)) = resolve_entry(
            tick_result,
            self.settings.quote_usd,
            self.settings.liquidity_usd,
        ) else {
            warn!(%mint, "no live price and QUOTE_USD unset; dropping signal");
            return;
        };

        // Developer passed a real on-chain history screen: encode as full trust.
        let metrics = TokenMetrics {
            liquidity_usd,
            buy_velocity_5s: buyers,
            dev_reputation: 1.0,
            rugcheck_score: signal.score,
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
            info!(%mint, buyers, score = signal.score, "risk PASS at confirm");
            return;
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
        let creator = req.creator;
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
        match portfolio.open_position_params(&simulator::OpenPositionParams {
            mint,
            side: Side::Long,
            quote: entry_quote,
            notional_usd: self.settings.notional_usd,
            liquidity_usd,
            tp_percent: profile.tp_percent,
            sl_percent: profile.sl_percent,
            trailing: profile.trailing_stop,
            trail_percent: self.settings.trail_percent,
            timeout_seconds: profile.timeout_seconds,
            moonbag_percent: profile.moonbag_percent,
            now_secs: now_secs(),
        }) {
            Ok(id) => {
                let snap = portfolio.snapshot(&HashMap::new());
                info!(%mint, id, entry = format!("{entry_quote:.10}"),
                    tp = profile.tp_percent, sl = profile.sl_percent,
                    equity = format!("{:.2}", snap.equity), "paper position opened");
                self.emit(Alerter::format_entry(
                    mint,
                    entry_quote,
                    profile.tp_percent,
                    profile.sl_percent,
                    self.settings.notional_usd,
                ));
                drop(portfolio);
                if let Err(e) = self.reserves.watch(mint, curve).await {
                    warn!(%mint, error = %e, "ticker watch failed; exits need manual review");
                }
                if allow_live {
                    self.live_buy(mint, curve, assoc_curve, entry_quote, creator).await;
                }
            }
            Err(e) => warn!(%mint, error = %e, "paper open failed"),
        }
    }

    /// Mirror a paper open with a real Jito-routed buy (live mode only).
    /// Paper state is authoritative; live failures only log loudly.
    async fn live_buy(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        entry_quote: f64,
        creator: Option<&str>,
    ) {
        let Some(trader) = &self.exec.live else { return };
        match trader
            .buy(&LiveBuyParams {
                mint,
                curve,
                assoc_curve,
                notional_usd: self.settings.notional_usd,
                price_usd: entry_quote,
                sol_usd: self.settings.sol_usd,
                creator,
            })
            .await
        {
            Ok((position, bundle)) => {
                let amount = position.amount_tokens;
                self.exec.live_positions
                    .lock()
                    .await
                    .insert(mint.to_string(), position);
                warn!(%mint, "LIVE buy dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_buy(mint, &bundle, amount));
            }
            Err(e) => warn!(%mint, error = %e, "LIVE buy FAILED or unconfirmed; paper position still open (unconfirmed fills are rechecked automatically)"),
        }
    }

    /// Re-poll unconfirmed live buys: promote landed fills to tracked
    /// (with exit automation), drop on-chain failures, escalate give-ups
    /// to manual review. No-op when nothing is pending.
    async fn recheck_live_pending(&self) {
        let Some(trader) = &self.exec.live else { return };
        if !trader.has_pending() {
            return;
        }
        for recheck in trader.recheck_pending().await {
            let mint = recheck.position.mint.clone();
            match recheck.outcome {
                RecheckOutcome::Confirmed => {
                    let amount = recheck.position.amount_tokens;
                    self.exec.live_positions
                        .lock()
                        .await
                        .insert(mint.clone(), recheck.position);
                    warn!(mint = %mint, "LIVE buy confirmed late; now tracked with exits");
                    self.emit(Alerter::format_live_buy(&mint, &recheck.bundle_id, amount));
                }
                RecheckOutcome::FailedOnChain => {
                    warn!(mint = %mint, bundle = %recheck.bundle_id, "pending LIVE buy errored on-chain; no fill, dropping");
                }
                RecheckOutcome::StillUnknown => {
                    warn!(mint = %mint, bundle = %recheck.bundle_id, "pending LIVE buy still unconfirmed; keeping for next round");
                }
                RecheckOutcome::GiveUp => {
                    warn!(mint = %mint, bundle = %recheck.bundle_id, "pending LIVE buy unconfirmed after retries; MANUAL REVIEW REQUIRED");
                }
            }
        }
    }

    /// Mirror paper closes with proportional Jito-routed sells (live mode
    /// only). Sells `fraction_closed` of the tracked amount so moonbag
    /// scale-outs mirror exactly; dust write-offs drop tracking with a
    /// loud log. No-ops when nothing is tracked for the mint.
    async fn live_sell(&self, mint: &str, price_usd: f64, fraction_closed: f64) {
        let Some(trader) = &self.exec.live else { return };
        // One live dispatch at a time: sizing and tracking stay atomic
        // across concurrent ticks for any mint.
        let _serial = self.exec.live_sell_lock.lock().await;
        // Cooldown + attempt budget: never spam bundles at a dead market.
        // Decide under one short lock, then release before any await or
        // second lock (no nested acquisition, no lock held across await).
        enum RetryDecision {
            Proceed,
            GiveUp,
            Cooldown,
        }
        let decision = {
            let mut retry = self.exec.live_retry.lock().await;
            let entry = retry.entry(mint.to_string()).or_insert((0u32, 0u64));
            let now = now_secs();
            if now.saturating_sub(entry.1) < SELL_RETRY_COOLDOWN.as_secs() {
                RetryDecision::Cooldown
            } else {
                entry.0 += 1;
                entry.1 = now;
                if entry.0 > SELL_MAX_ATTEMPTS {
                    RetryDecision::GiveUp
                } else {
                    RetryDecision::Proceed
                }
            }
        };
        match decision {
            RetryDecision::Cooldown => return,
            RetryDecision::GiveUp => {
                self.exec.live_retry.lock().await.remove(mint);
                self.exec.live_positions.lock().await.remove(mint);
                warn!(%mint, "LIVE sell gave up after retries; MANUAL EXIT REQUIRED");
                return;
            }
            RetryDecision::Proceed => {}
        }
        let probe = {
            let guard = self.exec.live_positions.lock().await;
            let Some(tracked) = guard.get(mint) else {
                return;
            };
            let sell_amount =
                ((tracked.amount_tokens as f64) * fraction_closed.clamp(0.0, 1.0)) as u64;
            // Floor (never ceiling): the live leg must never oversell
            // custody — any dust shortfall self-heals via the no-paper
            // retry path, which re-sells the remainder at full fraction.
            if sell_amount == 0 {
                return;
            }
            LivePosition {
                mint: tracked.mint.clone(),
                curve: tracked.curve.clone(),
                assoc_curve: tracked.assoc_curve.clone(),
                amount_tokens: sell_amount,
                creator: tracked.creator.clone(),
            }
        };
        match trader.sell(&probe, price_usd, self.settings.sol_usd).await {
            Ok(bundle) => {
                // Confirmed on-chain by LiveTrader: mirror the fill locally.
                let fully_closed = {
                    let mut guard = self.exec.live_positions.lock().await;
                    if let Some(tracked) = guard.get_mut(mint) {
                        tracked.amount_tokens =
                            tracked.amount_tokens.saturating_sub(probe.amount_tokens);
                        if tracked.amount_tokens == 0 {
                            guard.remove(mint);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };
                // A fully closed mint must not poison the next position's
                // retry budget with this round's attempts.
                if fully_closed {
                    self.exec.live_retry.lock().await.remove(mint);
                }
                warn!(%mint, %bundle, "LIVE sell dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_sell(mint, &bundle));
            }
            Err(e) => warn!(%mint, error = %e, "LIVE sell unconfirmed; holding, will retry"),
        }
    }

    /// Fire a card when alerting is enabled; silent otherwise.
    fn emit(&self, message: String) {
        if let Some(alerts) = &self.alerts {
            alerts.alert(message);
        }
    }

    /// One price tick: evaluate exits for every open position on the mint,
    /// close on trigger, retry failed live exits, unwatch when nothing
    /// remains.
    pub async fn on_tick(&self, tick: &PriceTick) {
        let mut portfolio = self.portfolio.lock().await;
        let ids: Vec<u64> = portfolio.positions_for_mint(&tick.mint);
        let mut closed_fraction = 0.0f64;
        if ids.is_empty() {
            drop(portfolio);
            // No paper activity: retry a previously failed live exit, if any.
            if self.exec.live_positions.lock().await.contains_key(&tick.mint) {
                self.live_sell(&tick.mint, tick.price_usd, 1.0).await;
            }
            // Pending buys recheck regardless of paper state.
            self.recheck_live_pending().await;
            if !self.exec.live_positions.lock().await.contains_key(&tick.mint)
                && self.reserves.unwatch(&tick.mint).await
            {
                info!(mint = %tick.mint, "unwatched (nothing left)");
            }
            if tick.complete {
                self.report_migration_once(&tick.mint, &tick.curve).await;
            }
            return;
        } else {
            if tick.complete {
                warn!(mint = %tick.mint, "curve migrated; closing positions (spot stale)");
            }
            for id in ids {
                let reason = if tick.complete {
                    Some(ExitReason::Manual)
                } else {
                    match portfolio.position_mut(id) {
                        Some(position) => check_exit(position, tick.price_usd, tick.observed_at),
                        None => None,
                    }
                };
                let Some(reason) = reason else { continue };
                // TakeProfit with a moonbag plan scales out once, leaving the
                // runner; the runner is disarmed so the next TP exits fully.
                let moonbag = portfolio
                    .position(id)
                    .map(|p| p.moonbag_percent)
                    .unwrap_or(0.0);
                let exit_fraction = if reason == ExitReason::TakeProfit && moonbag > 0.0 {
                    (1.0 - moonbag / 100.0).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                match portfolio.close_partial_position(
                    id,
                    exit_fraction,
                    tick.price_usd,
                    tick.liquidity_usd,
                    reason,
                    tick.observed_at,
                ) {
                    Ok(trade) => {
                        // Feed the day-loss breaker (exits keep working even
                        // past the cap — only new entries halt).
                        self.daily.lock().await.record(trade.net_pnl, now_secs());
                        let is_moonbag_exit = trade.fraction_closed < 1.0;
                        if is_moonbag_exit && let Some(open) = portfolio.position_mut(id) {
                            open.moonbag_percent = 0.0;
                        }
                        let equity = portfolio.snapshot(&HashMap::new()).equity;
                        info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", trade.net_pnl),
                        pct = format!("{:+.2}%", trade.net_pnl_percent),
                        "paper position closed");
                        self.emit(Alerter::format_exit(
                            &tick.mint,
                            &format!("{:?}", trade.reason),
                            trade.net_pnl,
                            trade.net_pnl_percent,
                            equity,
                            is_moonbag_exit,
                        ));
                        closed_fraction = closed_fraction.max(trade.fraction_closed);
                    }
                    Err(e) => warn!(mint = %tick.mint, id, error = %e, "paper close failed"),
                }
            }
        }

        drop(portfolio);
        if closed_fraction > 0.0 {
            self.live_sell(&tick.mint, tick.price_usd, closed_fraction)
                .await;
        }
        // Pending buys recheck before the unwatch decision so a late
        // confirmation keeps its feed.
        self.recheck_live_pending().await;
        // Re-check under fresh locks: the gate task may have opened a new
        // position for this mint during the awaits above. Unwatch only on
        // a confirmed-empty state.
        let still_open = !self
            .portfolio
            .lock()
            .await
            .positions_for_mint(&tick.mint)
            .is_empty();
        let live_holding = self.exec.live_positions.lock().await.contains_key(&tick.mint);
        let gate_pending = self
            .gate_state
            .lock()
            .await
            .gate
            .contains(&tick.mint);
        if !still_open && !live_holding && !gate_pending && self.reserves.unwatch(&tick.mint).await
        {
            info!(mint = %tick.mint, "unwatched (nothing left)");
        }
        if tick.complete {
            self.report_migration_once(&tick.mint, &tick.curve).await;
        }
    }

    /// Hunt and announce the migration exactly once per mint. Runs after
    /// positions close: classification plus pool addresses for instant
    /// Raydium routing follow-ups.
    async fn report_migration_once(&self, mint: &str, curve: &str) {
        {
            let mut migrated = self.migrated.lock().await;
            if !migrated.insert(mint.to_string()) {
                return;
            }
        }
        match self.screener.detect_migration(curve, mint).await {
            Ok(Some(report)) => {
                info!(mint = %mint, sig = %report.signature, dest = ?report.destination, amm = ?report.amm_id, "migration detected");
                self.emit(Alerter::format_migration(
                    mint,
                    &format!("{:?}", report.destination),
                    &report.signature,
                    report.amm_id.as_deref(),
                    report.base_mint.as_deref(),
                    report.quote_mint.as_deref(),
                ));
                self.follow_pool(mint, &report).await;
            }
            Ok(None) => {
                info!(mint = %mint, "graduated but no migration tx found yet; will retry on later ticks");
                // Allow a later tick to retry the hunt (the migration tx
                // may not be indexed yet right after graduation).
                self.migrated.lock().await.remove(mint);
            }
            Err(e) => {
                warn!(%mint, error = %e, "migration hunt failed");
                // Allow a later tick to retry the hunt.
                self.migrated.lock().await.remove(mint);
            }
        }
    }

    /// Register a migrated Raydium pool for post-migration price tracking.
    /// PumpSwap migrations carry no pool: nothing to follow.
    async fn follow_pool(&self, mint: &str, report: &MigrationReport) {
        let (Some(amm), Some(base_vault), Some(quote_vault)) = (
            report.amm_id.clone(),
            report.base_vault.clone(),
            report.quote_vault.clone(),
        ) else {
            // Pool unresolvable (trust-gate failure or PumpSwap): loud, so
            // a missing follow is never mistaken for a quiet market.
            warn!(mint = %mint, amm = ?report.amm_id, "migration without resolvable pool; not following");
            return;
        };
        if let Err(e) = self
            .vaults
            .watch_defaults(mint, &amm, &base_vault, &quote_vault)
            .await
        {
            warn!(amm = %amm, error = %e, "pool watch failed");
            return;
        }
        info!(%mint, amm = %amm, "following migrated pool");
    }

    /// One post-migration pool tick: seed the entry on first sight, then
    /// alert once the price doubles off it. Paper tracking only — entries
    /// need a Raydium swap builder (future work).
    pub async fn on_pool_tick(&self, tick: &PoolTick) {
        // Validate at the boundary: non-finite/non-positive prices must not
        // seed baselines or trigger alerts; stale ticks are dropped.
        if !tick.price_usd.is_finite() || tick.price_usd <= 0.0 {
            warn!(mint = %tick.mint, price = tick.price_usd, "ignoring degenerate pool tick");
            return;
        }
        if now_secs().saturating_sub(tick.observed_at) > POOL_TICK_STALE_SECS {
            warn!(mint = %tick.mint, observed_at = tick.observed_at, "ignoring stale pool tick");
            return;
        }
        let mut followed = self.followed.lock().await;
        let state = followed.entry(tick.mint.clone()).or_insert(FollowState {
            entry_price: tick.price_usd,
            alerted_spike: false,
        });
        if state.alerted_spike {
            return;
        }
        if tick.price_usd >= state.entry_price * FOLLOW_SPIKE_MULT {
            state.alerted_spike = true;
            info!(mint = %tick.mint, multiple = FOLLOW_SPIKE_MULT, price = tick.price_usd, "post-migration spike");
            self.emit(Alerter::format_follow_spike(
                &tick.mint,
                FOLLOW_SPIKE_MULT,
                tick.price_usd,
            ));
            // Alert fired: release the pool watch and the follow entry so a
            // dead pool is not polled forever (unbounded growth).
            let mint = tick.mint.clone();
            drop(followed);
            self.vaults.unwatch(&mint).await;
            self.followed.lock().await.remove(&mint);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulator::FillModel;
    use std::sync::Mutex as StdMutex;

    /// Canned audit outcomes for launch-path tests.
    enum AuditOutcome {
        Allow(u32),
        Reject,
        Broken,
    }

    struct StubAuditor {
        outcome: AuditOutcome,
    }

    impl Auditor for StubAuditor {
        async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
            match &self.outcome {
                AuditOutcome::Allow(score) => Ok(RugCheckReport {
                    mint: mint.to_string(),
                    score: *score,
                    risks: vec![],
                    token: None,
                }),
                AuditOutcome::Reject => Err(RugCheckError::SecurityRejection("rug".to_string())),
                AuditOutcome::Broken => Err(RugCheckError::Deserialization(
                    serde_json::from_str::<u32>("bogus").expect_err("must fail"),
                )),
            }
        }
    }

    /// Canned live sink with call tracking (shared handle for assertions).
    #[derive(Clone)]
    struct StubLive {
        inner: Arc<StubLiveInner>,
    }

    struct StubLiveInner {
        buys: StdMutex<Vec<(String, String, String, f64)>>,
        sells: StdMutex<Vec<(String, u64)>>,
        fail_buys: std::sync::atomic::AtomicBool,
        fail_sells: std::sync::atomic::AtomicBool,
    }

    impl StubLive {
        fn working() -> Self {
            Self {
                inner: Arc::new(StubLiveInner {
                    buys: StdMutex::new(vec![]),
                    sells: StdMutex::new(vec![]),
                    fail_buys: std::sync::atomic::AtomicBool::new(false),
                    fail_sells: std::sync::atomic::AtomicBool::new(false),
                }),
            }
        }

        fn buys(&self) -> Vec<(String, String, String, f64)> {
            self.inner.buys.lock().expect("lock").clone()
        }

        fn sells(&self) -> Vec<(String, u64)> {
            self.inner.sells.lock().expect("lock").clone()
        }
    }

    impl LiveSink for StubLive {
        async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError> {
            let mint = req.mint;
            let curve = req.curve;
            let assoc_curve = req.assoc_curve;
            let notional_usd = req.notional_usd;
            if self
                .inner
                .fail_buys
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::BadOrder("stubbed failure".to_string()));
            }
            self.inner.buys.lock().expect("lock").push((
                mint.to_string(),
                curve.to_string(),
                assoc_curve.to_string(),
                notional_usd,
            ));
            Ok((
                LivePosition {
                    mint: mint.to_string(),
                    curve: curve.to_string(),
                    assoc_curve: assoc_curve.to_string(),
                    amount_tokens: 1_000,
                    creator: None,
                },
                "bundle-stub".to_string(),
            ))
        }

        async fn sell(
            &self,
            position: &LivePosition,
            _price_usd: f64,
            _sol_usd: f64,
        ) -> Result<String, LiveError> {
            if self
                .inner
                .fail_sells
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::BadOrder("stubbed sell failure".to_string()));
            }
            self.inner
                .sells
                .lock()
                .expect("lock")
                .push((position.mint.clone(), position.amount_tokens));
            Ok("bundle-stub".to_string())
        }
    }

    /// Canned reserve feed with watch/unwatch call tracking.
    struct StubReserves {
        tick: Option<PriceTick>,
        watched: StdMutex<Vec<(String, String)>>,
        unwatched: StdMutex<Vec<String>>,
    }

    impl StubReserves {
        fn with_tick(tick: PriceTick) -> Self {
            Self {
                tick: Some(tick),
                watched: StdMutex::new(vec![]),
                unwatched: StdMutex::new(vec![]),
            }
        }

        fn failing() -> Self {
            Self {
                tick: None,
                watched: StdMutex::new(vec![]),
                unwatched: StdMutex::new(vec![]),
            }
        }
    }

    impl ReserveFeed for StubReserves {
        async fn fetch_tick(&self, mint: &str, _curve: &str) -> Result<PriceTick, CurveError> {
            self.tick
                .clone()
                .filter(|t| t.mint == mint)
                .ok_or(CurveError::AccountMissing(mint.to_string()))
        }

        async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError> {
            self.watched
                .lock()
                .expect("lock")
                .push((mint.to_string(), curve.to_string()));
            Ok(())
        }

        async fn unwatch(&self, mint: &str) -> bool {
            self.unwatched.lock().expect("lock").push(mint.to_string());
            true
        }
    }

    /// Recording alert sink for firing assertions.
    #[derive(Clone, Default)]
    struct StubAlerts {
        inner: Arc<StdMutex<Vec<String>>>,
    }

    impl StubAlerts {
        fn messages(&self) -> Vec<String> {
            self.inner.lock().expect("lock").clone()
        }
    }

    impl AlertSink for StubAlerts {
        fn alert(&self, message: String) {
            self.inner.lock().expect("lock").push(message);
        }
    }

    fn tick(mint: &str, price: f64) -> PriceTick {
        PriceTick {
            mint: mint.to_string(),
            curve: "CURVE".to_string(),
            price_usd: price,
            liquidity_usd: 30_000.0,
            complete: false,
            observed_at: 2_000,
        }
    }

    fn settings(vars: &[(&str, &str)]) -> Settings {
        Settings::from_map(
            &vars
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    /// Heuristic-friendly knobs: reputable dev, fast buys (BUY branch).
    use listener::{MigrationDestination, SwapSide, fixture_wallet};

    /// Stand-in developer wallet (valid base58).
    fn dev_wallet() -> String {
        fixture_wallet(7)
    }

    /// Distinct-buyer swap records for gate tests.
    fn buyer_swaps(mint: &str, bytes: &[u8]) -> Vec<SwapRecord> {
        bytes
            .iter()
            .map(|b| {
                SwapRecord::new(
                    &format!("{mint}-sig{b}"),
                    &fixture_wallet(*b),
                    SwapSide::Buy,
                    1000 + u64::from(*b),
                    1_000_000,
                )
                .expect("fixture wallet")
            })
            .collect()
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum DevOutcome {
        Pass,
        Fail,
        Broken,
    }

    /// Canned screener: dev verdict plus per-mint swap observations.
    #[derive(Clone)]
    struct StubScreener {
        inner: Arc<StubScreenerInner>,
    }

    struct StubScreenerInner {
        dev: StdMutex<DevOutcome>,
        swaps: StdMutex<HashMap<String, Vec<SwapRecord>>>,
        bundle: StdMutex<BundleOutcome>,
        migration: StdMutex<Option<MigrationReport>>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BundleOutcome {
        Safe,
        Suspect,
        Reject,
        Broken,
    }

    impl StubScreener {
        fn passing() -> Self {
            Self {
                inner: Arc::new(StubScreenerInner {
                    dev: StdMutex::new(DevOutcome::Pass),
                    swaps: StdMutex::new(HashMap::new()),
                    bundle: StdMutex::new(BundleOutcome::Safe),
                    migration: StdMutex::new(None),
                }),
            }
        }

        fn with_dev(dev: DevOutcome) -> Self {
            let stub = Self::passing();
            *stub.inner.dev.lock().expect("lock") = dev;
            stub
        }

        fn with_swaps(mint: &str, swaps: Vec<SwapRecord>) -> Self {
            let stub = Self::passing();
            stub.inner
                .swaps
                .lock()
                .expect("lock")
                .insert(mint.to_string(), swaps);
            stub
        }
    }

    impl Screener for StubScreener {
        async fn screen_dev(&self, _dev: &str) -> Result<DevVerdict, ScreenError> {
            match *self.inner.dev.lock().expect("lock") {
                DevOutcome::Pass => Ok(DevVerdict {
                    pass: true,
                    age_secs: Some(7200),
                    prior_launches: 0,
                    reason: "stub".to_string(),
                }),
                DevOutcome::Fail => Ok(DevVerdict {
                    pass: false,
                    age_secs: Some(100),
                    prior_launches: 0,
                    reason: "stub reject".to_string(),
                }),
                DevOutcome::Broken => Err(ScreenError::BadAddress("stub".to_string())),
            }
        }

        async fn observe_curve(
            &self,
            mint: &str,
            _curve: &str,
            _since_sig: Option<&str>,
        ) -> Result<(Vec<SwapRecord>, Option<String>), ScreenError> {
            let swaps = self
                .inner
                .swaps
                .lock()
                .expect("lock")
                .get(mint)
                .cloned()
                .unwrap_or_default();
            Ok((swaps, Some("sig-stub".to_string())))
        }

        async fn screen_bundle(
            &self,
            _curve: &str,
            _creation_slot: u64,
        ) -> Result<BundleAnalysis, ScreenError> {
            match *self.inner.bundle.lock().expect("lock") {
                BundleOutcome::Safe => Ok(BundleAnalysis {
                    is_bundled: false,
                    creation_slot_buys: 1,
                    shared_funder_count: 0,
                    total_cluster_supply_pct: 2.0,
                    verdict: BundleVerdict::SafeOrganic,
                }),
                BundleOutcome::Suspect => Ok(BundleAnalysis {
                    is_bundled: true,
                    creation_slot_buys: 3,
                    shared_funder_count: 0,
                    total_cluster_supply_pct: 9.0,
                    verdict: BundleVerdict::SuspectLowVolume,
                }),
                BundleOutcome::Reject => Ok(BundleAnalysis {
                    is_bundled: true,
                    creation_slot_buys: 5,
                    shared_funder_count: 4,
                    total_cluster_supply_pct: 22.0,
                    verdict: BundleVerdict::RejectBundledMonopoly,
                }),
                BundleOutcome::Broken => Err(ScreenError::BadAddress("stub".to_string())),
            }
        }

        async fn detect_migration(
            &self,
            _curve: &str,
            _mint: &str,
        ) -> Result<Option<MigrationReport>, ScreenError> {
            let found = self.inner.migration.lock().expect("lock").clone();
            Ok(found)
        }
    }

    fn pipeline_screened(
        settings: Settings,
        outcome: AuditOutcome,
        reserves: StubReserves,
        portfolio: PaperPortfolio,
        screener: StubScreener,
        live: Option<StubLive>,
    ) -> Arc<Pipeline<StubAuditor, StubReserves, StubLive, StubScreener, StubAlerts>> {
        pipeline_alerted(settings, outcome, reserves, portfolio, screener, live, None)
    }

    fn pipeline_alerted(
        settings: Settings,
        outcome: AuditOutcome,
        reserves: StubReserves,
        portfolio: PaperPortfolio,
        screener: StubScreener,
        live: Option<StubLive>,
        alerts: Option<StubAlerts>,
    ) -> Arc<Pipeline<StubAuditor, StubReserves, StubLive, StubScreener, StubAlerts>> {
        let vaults = migration::VaultTicker::new(
            "https://api.mainnet-beta.solana.com",
            curve::CurveTicker::parse_commitment("confirmed").expect("known commitment"),
            std::time::Duration::from_millis(500),
            150.0,
        )
        .expect("vault ticker");
        Pipeline::shared(
            settings,
            StubAuditor { outcome },
            None,
            reserves,
            portfolio,
            live,
            screener,
            alerts,
            vaults,
        )
    }

    fn funded() -> PaperPortfolio {
        PaperPortfolio::new(100.0, 0.0).with_fill_model(FillModel::Exact)
    }

    #[test]
    fn entry_prefers_live_tick_over_override() {
        let live = tick("M", 2.0);
        assert_eq!(
            resolve_entry(Ok(live), Some(9.0), 500.0),
            Some((2.0, 30_000.0))
        );
    }

    #[test]
    fn entry_falls_back_to_override() {
        let err: Result<PriceTick, CurveError> = Err(CurveError::AccountMissing("M".to_string()));
        assert_eq!(resolve_entry(err, Some(9.0), 500.0), Some((9.0, 500.0)));
    }

    #[test]
    fn entry_is_none_without_price_or_override() {
        let err: Result<PriceTick, CurveError> = Err(CurveError::AccountMissing("M".to_string()));
        assert_eq!(resolve_entry(err, None, 0.0), None);
    }

    #[tokio::test]
    async fn launch_watchlists_but_does_not_open() {
        let screener = StubScreener::passing();
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;

        // Signal accepted, but no position until buyer quorum confirms.
        assert_eq!(pipe.pending_count().await, 1);
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn daily_breaker_blocks_opens_but_not_exits() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let mut settings = Settings::from_map(&HashMap::new());
        settings.max_daily_loss_usd = 5.0;
        let pipe = pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        // Seed a blown day, then run the full confirm flow.
        pipe.daily.lock().await.record(-10.0, now_secs());
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000).await;
        pipe.poll_gates().await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());

        // Exits still work past the cap: open manually, force a stop.
        let id = pipe
            .portfolio
            .lock()
            .await
            .open_position("MINT", Side::Long, 2.0, 10.0, 1e9, 75.0, -8.0, false, 0.0, 0, 0.0, now_secs())
            .expect("manual open");
        let mut stop = tick("MINT", 2.0);
        stop.price_usd = 1.0; // -50% << -8% SL.
        pipe.on_tick(&stop).await;
        assert!(pipe.portfolio.lock().await.position(id).is_none());
    }

    #[tokio::test]
    async fn confirm_flow_opens_with_measured_velocity() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        // 3 buyers → scalp branch with widened TP/SL (40/-10), not 18/-4.
        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.position_ids().len(), 1);
        let id = portfolio.position_ids()[0];
        let position = portfolio.position(id).expect("position");
        assert_eq!(position.entry_price, 2.0);
        assert_eq!((position.tp_percent, position.sl_percent), (35.0, -8.0));
        drop(portfolio);
        assert_eq!(pipe.pending_count().await, 0);
        assert_eq!(pipe.reserves.watched.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn confirm_high_momentum_keeps_trailing_profile() {
        let buyers: Vec<u8> = (20..55).collect();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &buyers));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        let portfolio = pipe.portfolio.lock().await;
        let id = portfolio.position_ids()[0];
        let position = portfolio.position(id).expect("position");
        assert_eq!((position.tp_percent, position.sl_percent), (60.0, -10.0));
        assert!(position.trailing_stop);
    }

    #[tokio::test]
    async fn confirm_low_buyers_passes_on_reevaluation() {
        // Quorum of 2 confirms, but velocity 2 < 5 → heuristic PASS at confirm.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            settings(&[("GATE_MIN_BUYERS", "2")]),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn gate_aborts_on_dev_dump() {
        let mut swaps = buyer_swaps("MINT", &[11]);
        swaps.push(
            SwapRecord::new("dump", &dev_wallet(), SwapSide::Sell, 1003, 1_000_000)
                .expect("fixture"),
        );
        let screener = StubScreener::with_swaps("MINT", swaps);
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn gate_times_out_on_dead_token() {
        // Zero window: the deadline is already past at first observation.
        let pipe = pipeline_screened(
            settings(&[("GATE_WINDOW_SECS", "0")]),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            StubScreener::passing(),
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        assert_eq!(pipe.pending_count().await, 1);
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn bundle_reject_drops_signal() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Reject;
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn bundle_suspect_trades_paper_only() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Suspect;
        let live = StubLive::working();
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            Some(live.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        // Paper opens (tightened risk = no real funds), live stays quiet.
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(live.buys().is_empty());
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn bundle_screen_error_drops_fail_closed() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Broken;
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn migration_card_fires_once_per_mint() {
        let alerts = StubAlerts::default();
        let screener = StubScreener::passing();
        *screener.inner.migration.lock().expect("lock") = Some(MigrationReport {
            signature: "migsig".to_string(),
            destination: MigrationDestination::RaydiumV4,
            amm_id: Some(fixture_wallet(20)),
            base_mint: Some("MINT".to_string()),
            quote_mint: Some("WSOL".to_string()),
            base_vault: Some(fixture_wallet(21)),
            quote_vault: Some(fixture_wallet(22)),
        });
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
            Some(alerts.clone()),
        );
        let mut graduated = tick("MINT", 2.0);
        graduated.complete = true;
        pipe.on_tick(&graduated).await;
        pipe.on_tick(&graduated).await;

        let cards = alerts.messages();
        assert_eq!(cards.len(), 1);
        assert!(cards[0].contains("[MIGRATION DETECTED]"));
        assert!(cards[0].contains("RaydiumV4"));
        assert!(cards[0].contains("explorer.solana.com/tx/migsig"));
        // Pool follow registered for post-migration tracking.
        assert_eq!(pipe.vaults.watched_count().await, 1);
    }

    #[tokio::test]
    async fn follow_spike_alerts_once_at_2x() {
        let alerts = StubAlerts::default();
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            StubScreener::passing(),
            None,
            Some(alerts.clone()),
        );
        let tick = |price: f64| PoolTick {
            mint: "MINT".to_string(),
            amm_id: "AMM".to_string(),
            price_usd: price,
            liquidity_usd: 50_000.0,
            observed_at: now_secs(),
        };
        pipe.on_pool_tick(&tick(1.0)).await;
        assert!(alerts.messages().is_empty());
        pipe.on_pool_tick(&tick(1.5)).await;
        assert!(alerts.messages().is_empty());
        pipe.on_pool_tick(&tick(2.0)).await;
        let cards = alerts.messages();
        assert_eq!(cards.len(), 1);
        assert!(cards[0].contains("[POST-MIGRATION SPIKE]"));
        assert!(cards[0].contains("2.0x"));
        // Second crossing does not re-fire.
        pipe.on_pool_tick(&tick(3.0)).await;
        assert_eq!(alerts.messages().len(), 1);
    }

    #[test]
    fn daily_breaker_trips_resets_and_ignores_nan() {
        let day = 1_000 * 86_400;
        let mut b = DailyBreaker::new(day);
        assert!(!b.tripped(10.0, day));
        assert!(!b.tripped(0.0, day)); // disabled cap never trips
        b.record(-6.0, day);
        assert!(!b.tripped(10.0, day));
        b.record(-4.0, day);
        assert!(b.tripped(10.0, day));
        // Wins offset within the day.
        b.record(5.0, day);
        assert!(!b.tripped(10.0, day));
        // NaN never corrupts the bucket.
        b.record(f64::NAN, day);
        assert!(!b.tripped(10.0, day));
        // New UTC day rolls the bucket.
        assert!(!b.tripped(10.0, day + 86_400));
        // Stale bucket read also rolls (no phantom trip).
        let mut stale = DailyBreaker { day, day_net: -1_000.0 };
        assert!(!stale.tripped(10.0, day + 86_400));
    }

    #[tokio::test]
    async fn stale_and_degenerate_pool_ticks_ignored() {
        let alerts = StubAlerts::default();
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            StubScreener::passing(),
            None,
            Some(alerts.clone()),
        );
        let old = PoolTick {
            mint: "MINT".to_string(),
            amm_id: "AMM".to_string(),
            price_usd: 100.0,
            liquidity_usd: 50_000.0,
            observed_at: 3000,
        };
        pipe.on_pool_tick(&old).await;
        for bad_price in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            pipe.on_pool_tick(&PoolTick {
                mint: "MINT".to_string(),
                amm_id: "AMM".to_string(),
                price_usd: bad_price,
                liquidity_usd: 50_000.0,
                observed_at: now_secs(),
            })
            .await;
        }
        assert!(alerts.messages().is_empty());
        assert!(pipe.followed.lock().await.is_empty());
    }

    #[tokio::test]
    async fn no_migration_card_when_hunt_finds_nothing() {
        let alerts = StubAlerts::default();
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            StubScreener::passing(),
            None,
            Some(alerts.clone()),
        );
        let mut graduated = tick("MINT", 2.0);
        graduated.complete = true;
        pipe.on_tick(&graduated).await;

        assert!(alerts.messages().is_empty());
    }

    #[tokio::test]
    async fn launch_skips_on_dev_reject_and_error() {
        for dev in [DevOutcome::Fail, DevOutcome::Broken] {
            let pipe = pipeline_screened(
                Settings::from_map(&HashMap::new()),
                AuditOutcome::Allow(100),
                StubReserves::with_tick(tick("MINT", 2.0)),
                funded(),
                StubScreener::with_dev(dev),
                None,
            );
            pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
                .await;
            assert!(pipe.portfolio.lock().await.position_ids().is_empty());
            assert_eq!(pipe.pending_count().await, 0);
        }
    }

    #[tokio::test]
    async fn launch_skips_on_audit_reject_and_error() {
        for outcome in [AuditOutcome::Reject, AuditOutcome::Broken] {
            let reserves = StubReserves::with_tick(tick("MINT", 2.0));
            let pipe = pipeline_screened(
                Settings::from_map(&HashMap::new()),
                outcome,
                reserves,
                funded(),
                StubScreener::passing(),
                None,
            );
            pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
                .await;
            assert!(pipe.portfolio.lock().await.position_ids().is_empty());
            assert_eq!(pipe.pending_count().await, 0);
            assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
        }
    }

    #[tokio::test]
    async fn confirm_drops_without_any_price() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn confirm_uses_quote_override_when_tick_fails() {
        // Override liquidity must itself clear the $10k moderate floor.
        let settings = settings(&[("QUOTE_USD", "3.0"), ("LIQUIDITY_USD", "15000")]);
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let pipe = pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        let portfolio = pipe.portfolio.lock().await;
        let id = portfolio.position_ids()[0];
        assert_eq!(portfolio.position(id).expect("position").entry_price, 3.0);
    }

    #[tokio::test]
    async fn tick_closes_on_take_profit_and_unwatches() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        // +50% over the 2.0 entry clears the 35% TP.
        pipe.on_tick(&tick("MINT", 3.0)).await;

        let portfolio = pipe.portfolio.lock().await;
        assert!(portfolio.position_ids().is_empty());
        assert_eq!(portfolio.history().len(), 1);
        let trade = &portfolio.history()[0];
        assert_eq!(trade.reason, ExitReason::TakeProfit);
        assert!(trade.net_pnl > 0.0);
        drop(portfolio);
        assert_eq!(
            pipe.reserves.unwatched.lock().expect("lock").as_slice(),
            ["MINT"]
        );
    }

    #[tokio::test]
    async fn tick_holds_without_trigger() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        pipe.on_tick(&tick("MINT", 2.1)).await;

        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(pipe.reserves.unwatched.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn tick_closes_other_mints_only() {
        let screener = StubScreener::with_swaps("A", buyer_swaps("A", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("A", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("A", "CURVE_A", "ASSOC_A", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        // Same portfolio, second mint opened directly at the same price.
        {
            let mut portfolio = pipe.portfolio.lock().await;
            portfolio
                .open_position(
                    "B",
                    Side::Long,
                    2.0,
                    10.0,
                    30_000.0,
                    75.0,
                    -8.0,
                    false,
                    0.0,
                    0,
                    0.0,
                    1000,
                )
                .expect("open B");
        }

        pipe.on_tick(&tick("A", 3.0)).await;

        let portfolio = pipe.portfolio.lock().await;
        let remaining: Vec<String> = portfolio
            .position_ids()
            .into_iter()
            .map(|id| portfolio.position(id).expect("position").mint.clone())
            .collect();
        assert_eq!(remaining, vec!["B".to_string()]);
    }

    #[tokio::test]
    async fn tick_timeout_closes_stale_position() {
        let mut portfolio = funded();
        portfolio
            .open_position(
                "MINT",
                Side::Long,
                2.0,
                10.0,
                30_000.0,
                75.0,
                -8.0,
                false,
                0.0,
                60,
                0.0,
                1000,
            )
            .expect("open");
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            portfolio,
            StubScreener::passing(),
            None,
        );

        let mut timeout_tick = tick("MINT", 2.0);
        timeout_tick.observed_at = 1061;
        pipe.on_tick(&timeout_tick).await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history()[0].reason, ExitReason::Timeout);
    }

    #[tokio::test]
    async fn tick_migration_forces_manual_close() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        let mut migrated = tick("MINT", 2.0);
        migrated.complete = true;
        pipe.on_tick(&migrated).await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history()[0].reason, ExitReason::Manual);
        assert!(portfolio.position_ids().is_empty());
    }

    #[tokio::test]
    async fn live_buy_fires_with_paper_open() {
        let live = StubLive::working();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            Some(live.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        assert_eq!(
            live.buys(),
            vec![(
                "MINT".to_string(),
                "CURVE".to_string(),
                "ASSOC".to_string(),
                10.0
            )]
        );
        let tracked = pipe.exec.live_positions.lock().await;
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked["MINT"].amount_tokens, 1_000);
    }

    #[tokio::test]
    async fn live_buy_failure_keeps_paper_position() {
        let live = StubLive::working();
        live.inner
            .fail_buys
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            Some(live.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        // Paper record survives; nothing tracked for exits.
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn live_sell_fires_on_paper_close() {
        let live = StubLive::working();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            Some(live.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.0)).await;

        assert_eq!(live.sells(), vec![("MINT".to_string(), 1_000)]);
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn no_live_calls_when_disabled() {
        // Paper-only pipeline (live=None) never touches live state.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn alerts_fire_on_open_and_close() {
        let alerts = StubAlerts::default();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
            Some(alerts.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.0)).await;

        let cards = alerts.messages();
        assert_eq!(cards.len(), 2);
        assert!(cards[0].contains("[TRADE OPENED]"));
        assert!(cards[0].contains("`MINT`"));
        assert!(cards[1].contains("[POSITION CLOSED]"));
        assert!(cards[1].contains("TakeProfit"));
        assert!(cards[1].contains("`MINT`"));
    }

    #[tokio::test]
    async fn alerts_fire_on_live_dispatch() {
        let alerts = StubAlerts::default();
        let live = StubLive::working();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            Some(live),
            Some(alerts.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.0)).await;

        let cards = alerts.messages();
        assert_eq!(cards.len(), 4);
        assert!(cards[1].contains("[LIVE BUY SENT]"));
        assert!(cards[1].contains("bundle-stub"));
        assert!(cards[3].contains("[LIVE SELL SENT]"));
    }

    #[tokio::test]
    async fn tp_scales_out_moonbag_then_rides() {
        // High-momentum confirm carries moonbag 20: TP exits 80%.
        let buyers: Vec<u8> = (20..55).collect();
        let alerts = StubAlerts::default();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &buyers));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
            Some(alerts.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        // +70% clears the 60% TP: 80% out, 20% runner stays armed-off.
        pipe.on_tick(&tick("MINT", 3.4)).await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history().len(), 1);
        assert_eq!(portfolio.history()[0].fraction_closed, 0.8);
        let id = portfolio.position_ids()[0];
        let runner = portfolio.position(id).expect("runner");
        assert_eq!((runner.quantity, runner.entry_notional), (1.0, 2.0));
        assert_eq!(runner.moonbag_percent, 0.0);
        // Scale-out ratchets the runner stop to breakeven.
        assert_eq!(runner.sl_percent, 0.0);
        drop(portfolio);
        // Still watched (position open) and the card is flagged moonbag.
        assert!(pipe.reserves.unwatched.lock().expect("lock").is_empty());
        let cards = alerts.messages();
        assert_eq!(cards.len(), 2);
        assert!(cards[1].contains("MOONBAG PARTIAL EXIT"));

        // Runner hits TP again: full exit this time, then unwatch.
        pipe.on_tick(&tick("MINT", 4.0)).await;
        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history().len(), 2);
        assert_eq!(portfolio.history()[1].fraction_closed, 1.0);
        assert!(portfolio.position_ids().is_empty());
        drop(portfolio);
        assert_eq!(
            pipe.reserves.unwatched.lock().expect("lock").as_slice(),
            ["MINT"]
        );
    }

    #[tokio::test]
    async fn live_sell_mirrors_partial_fractions() {
        let buyers: Vec<u8> = (20..55).collect();
        let alerts = StubAlerts::default();
        let live = StubLive::working();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &buyers));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            Some(live.clone()),
            Some(alerts.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        // Tracked 1000 base units: TP sells 800, runner keeps 200 tracked.
        pipe.on_tick(&tick("MINT", 3.4)).await;
        assert_eq!(live.sells(), vec![("MINT".to_string(), 800)]);
        assert_eq!(pipe.exec.live_positions.lock().await["MINT"].amount_tokens, 200);

        // Age the retry clock past the cooldown so the runner exit fires.
        pipe.exec.live_retry
            .lock()
            .await
            .insert("MINT".to_string(), (1u32, 0u64));
        pipe.on_tick(&tick("MINT", 4.0)).await;
        assert_eq!(
            live.sells(),
            vec![("MINT".to_string(), 800), ("MINT".to_string(), 200)]
        );
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn silenced_pipeline_sends_nothing() {
        // No alerts attached: firing paths are skipped without error.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert_eq!(pipe.portfolio.lock().await.history().len(), 1);
    }

    #[tokio::test]
    async fn heat_cap_blocks_extra_positions() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let pipe = pipeline_screened(
            settings(&[("MAX_OPEN_POSITIONS", "1")]),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
        );
        // Fill the single slot directly, then confirm must skip.
        {
            let mut portfolio = pipe.portfolio.lock().await;
            portfolio
                .open_position(
                    "OTHER",
                    Side::Long,
                    2.0,
                    10.0,
                    30_000.0,
                    35.0,
                    -8.0,
                    false,
                    0.0,
                    0,
                    0.0,
                    1000,
                )
                .expect("open");
        }
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.position_ids().len(), 1);
        assert_eq!(
            portfolio
                .position(portfolio.position_ids()[0])
                .expect("p")
                .mint,
            "OTHER"
        );
    }

    #[tokio::test]
    async fn cooldown_blocks_immediate_reentry() {
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let mut portfolio = funded();
        portfolio
            .open_position(
                "MINT",
                Side::Long,
                2.0,
                10.0,
                30_000.0,
                35.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                1000,
            )
            .expect("open");
        let id = portfolio.position_ids()[0];
        portfolio
            .close_position(id, 2.0, 30_000.0, ExitReason::Manual, now_secs())
            .expect("close");
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            portfolio,
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    }

    #[tokio::test]
    async fn live_sell_retries_on_cooldown_then_gives_up() {
        use std::sync::atomic::Ordering;
        let live = StubLive::working();
        live.inner.fail_sells.store(true, Ordering::Relaxed);
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            Some(live.clone()),
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        // First close attempt dispatches (and fails); immediate retries
        // are skipped by the cooldown.
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(live.sells().is_empty());
        let attempts = pipe.exec.live_retry.lock().await;
        assert_eq!(attempts.get("MINT"), Some(&(1u32, attempts["MINT"].1)));
        drop(attempts);

        // Pre-seed attempts at the cap with a stale timestamp: next tick
        // gives up, drops tracking, and stops watching the mint.
        {
            let mut retry = pipe.exec.live_retry.lock().await;
            retry.insert("MINT".to_string(), (5u32, 0u64));
        }
        // Paper already closed; drive another tick to trigger the retry path.
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(pipe.exec.live_positions.lock().await.is_empty());
        assert!(live.sells().is_empty());
    }
}
