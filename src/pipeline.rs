//! Event processing: launch handling and tick-driven exits.
//!
//! [`Pipeline`] is generic over [`Auditor`] and [`ReserveFeed`] so unit
//! tests inject stubs instead of network clients. Pure helpers
//! ([`resolve_entry`]) stay free functions for direct testing.

use std::collections::HashMap;
use std::sync::Arc;

use alert::Alerter;
use auditor::{RugCheckClient, RugCheckError, RugCheckReport};
use curve::{CurveError, CurveTicker, PriceTick, SolUsdFeed};
use listener::{
    BundleAnalysis, BundleVerdict, DevVerdict, EntryGate, GateOutcome, MigrationReport,
    RpcScreener, ScreenError, SwapRecord,
};
use migration::{PoolTick, VaultTicker};
use risk_engine::{Action, RiskEngine, SCALP_TP_MIN, StrategyConfig, StrategyMode, TokenMetrics, evaluate_heuristic_fallback, resolve_strategy};
use simulator::{ExitReason, PaperPortfolio, Side, check_exit};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::live::{LiveBuyParams, LiveError, LivePosition, LiveTrader, PendingRecheck};
use crate::live_mirror::LiveExecution;
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
    /// Mint decimals for base-unit math (cached upstream).
    async fn token_decimals(&self, mint: &str) -> Result<u8, CurveError>;
    /// Watch a curve for exit polling.
    async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError>;
    /// Stop watching; true when an entry was removed.
    async fn unwatch(&self, mint: &str) -> bool;
}

impl ReserveFeed for CurveTicker {
    async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError> {
        self.fetch_tick(mint, curve).await
    }

    async fn token_decimals(&self, mint: &str) -> Result<u8, CurveError> {
        self.token_decimals(mint).await
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
        liquidity_usd: f64,
    ) -> Result<String, LiveError>;

    /// True when at least one buy awaits confirmation (default: none).
    fn has_pending(&self) -> bool {
        false
    }

    /// Mints with buys awaiting confirmation (default: none).
    /// Used by the shutdown snapshot and heat cap so pending bags are
    /// never invisible while the recheck loop still owns them.
    fn pending_mints(&self) -> Vec<String> {
        Vec::new()
    }

    /// Per-dispatch on-chain overhead in lamports (tip + priority fee;
    /// default: none). Used for day-loss cost accounting.
    fn dispatch_cost_lamports(&self) -> u64 {
        0
    }

    /// Record a tracked fill in the crash-recovery WAL (default: none).
    /// Call AFTER inserting into `live_positions`.
    fn acknowledge_buy(&self, _position: &LivePosition, _bundle_id: &str) {}

    /// Record a human-exit bag in the crash-recovery WAL (default: none).
    /// Call BEFORE inserting into `live_manual`. `note` carries the
    /// lookup identifier (bundle UUID, signature, or cause) so the
    /// shutdown log tells the operator exactly what to check on-chain.
    fn record_manual(&self, _position: &LivePosition, _note: &str) {}

    /// Record a confirmed sell in the crash-recovery WAL (default: none).
    /// Call BEFORE subtracting custody.
    fn record_sell(&self, _mint: &str, _amount_tokens: u64, _bundle_id: &str) {}

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
        liquidity_usd: f64,
    ) -> Result<String, LiveError> {
        self.sell(position, price_usd, sol_usd, liquidity_usd).await
    }

    fn has_pending(&self) -> bool {
        self.has_pending()
    }

    fn pending_mints(&self) -> Vec<String> {
        self.pending_mints()
    }

    fn dispatch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_lamports()
    }

    fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str) {
        self.acknowledge_buy(position, bundle_id)
    }

    fn record_manual(&self, position: &LivePosition, note: &str) {
        self.record_manual(position, note)
    }

    fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str) {
        self.record_sell(mint, amount_tokens, bundle_id)
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
    /// Swap records for a curve since `since_sig`, plus the new watermark
    /// and a truncation flag (a truncated round must not advance quorum).
    async fn observe_curve(
        &self,
        mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>, bool), ScreenError>;
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
    ) -> Result<(Vec<SwapRecord>, Option<String>, bool), ScreenError> {
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
    /// Per-mint tick serialization (see [`Pipeline::on_tick`]).
    pub tick_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Real-funds execution state (dispatcher, fills, retries).
    pub exec: LiveExecution<L>,
    /// Mints already reported as migrated, with first-report time
    /// (pruned past a day; a stale entry only risks a duplicate card,
    /// never a missed migration).
    pub migrated: Mutex<HashMap<String, u64>>,
    /// Post-migration pool watchlist (paper tracking only).
    pub followed: Mutex<HashMap<String, FollowState>>,
    /// Raydium vault ticker for followed pools.
    pub vaults: VaultTicker,
    /// Shared SOL/USD feed (dynamic, stale-gated) for live sizing.
    pub sol_feed: Arc<SolUsdFeed>,
    /// On-chain screening source (dev history + curve reads).
    pub screener: S,
    /// Watchlist state plus per-mint signal context (single lock).
    pub gate_state: Mutex<GateState>,
    /// Day-loss circuit breaker (new entries halt past the cap; exits
    /// never block).
    pub daily: Mutex<DailyBreaker>,
    /// Execution strategy, parsed once at startup (unknown modes fall
    /// back to adaptive with a warning there, not per-tick spam).
    pub strategy: StrategyMode,
    /// Trade cards; `None` silences alerting entirely.
    pub alerts: Option<T>,
}


/// Post-migration spike multiple that fires the follow alert once.
pub const FOLLOW_SPIKE_MULT: f64 = 2.0;
/// Pool ticks older than this (seconds) are ignored: a stale baseline or
/// spike check is worse than none (thin/manipulated prints linger).
pub const POOL_TICK_STALE_SECS: u64 = 300;
/// Curve ticks older than this (seconds) are ignored: a hung ticker round
/// (15s budget + 60s overall timeout) must not drive TP/SL/trailing exits
/// on a stale print. Mirrors the pool rule above.
pub const CURVE_TICK_STALE_SECS: u64 = 120;

/// Post-migration follow state per mint.
#[derive(Debug, Clone)]
pub struct FollowState {
    /// First pool-tick price (baseline).
    pub entry_price: f64,
    /// Whether the spike card already fired.
    pub alerted_spike: bool,
    /// Last pool-tick time (unix secs): idle follows older than
    /// [`FOLLOW_TTL_SECS`] are evicted (dead pools, bounded growth).
    pub last_seen_secs: u64,
}

/// Idle-follow TTL: post-migration pools that never spike stop being
/// polled after a day (evicted with their vault watch).
pub const FOLLOW_TTL_SECS: u64 = 86_400;
/// Reported-migration memory: entries older than a day are pruned (a
/// stale entry only risks a duplicate card, never a miss).
pub const MIGRATED_TTL_SECS: u64 = 86_400;
/// Backstop cap for reported-migration memory (oldest-first eviction).
pub const MIGRATED_CAP: usize = 5_000;

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
        sol_feed: Arc<SolUsdFeed>,
    ) -> Arc<Self> {
        let gate = EntryGate::new(settings.gate_window_secs, settings.gate_min_buyers);
        if StrategyMode::is_deprecated_alias(&settings.strategy_mode) {
            warn!(mode = %settings.strategy_mode, "deprecated strategy alias; use `scalp` or `runner` (removal in a future release)");
        }
        let strategy = settings.strategy_mode.parse().unwrap_or_else(|e| {
            warn!(mode = %settings.strategy_mode, error = %e, "bad strategy mode; using adaptive");
            StrategyMode::Adaptive
        });
        Arc::new(Self {
            settings,
            auditor,
            engine,
            reserves,
            portfolio: Mutex::new(portfolio),
            tick_locks: Mutex::new(HashMap::new()),
            exec: LiveExecution::paper().with_live(live),
            migrated: Mutex::new(HashMap::new()),
            followed: Mutex::new(HashMap::new()),
            screener,
            gate_state: Mutex::new(GateState::new(gate)),
            daily: Mutex::new(DailyBreaker::new(now_secs())),
            strategy,
            alerts,
            vaults,
            sol_feed,
        })
    }

    /// Watchlisted mint count (for metrics/tests).
    pub async fn pending_count(&self) -> usize {
        self.gate_state.lock().await.gate.pending_count()
    }

    /// Paper cash + mark-to-market equity (heartbeat cards).
    pub async fn paper_cash_equity(&self) -> (f64, f64) {
        let portfolio = self.portfolio.lock().await;
        let snap = portfolio.snapshot(&HashMap::new());
        (snap.cash, snap.equity)
    }

    /// Mints with open paper positions (shutdown snapshot: nothing open
    /// may vanish silently on SIGTERM).
    pub async fn open_position_mints(&self) -> Vec<String> {
        let portfolio = self.portfolio.lock().await;
        portfolio
            .position_ids()
            .iter()
            .filter_map(|id| portfolio.position(*id).map(|p| p.mint.clone()))
            .collect()
    }

    /// Mints with tracked live fills (shutdown snapshot: these need
    /// manual review when the drain does not finish them). Synthetic
    /// manual keys (`MINT#2-…`, `MINT#late-…`, `MINT#wal-N` — second bags
    /// parked without clobbering the first) report under their base mint.
    pub async fn live_tracked_mints(&self) -> Vec<String>
    where
        L: LiveSink,
    {
        let mut mints: Vec<String> = self
            .exec
            .live_positions
            .lock()
            .await
            .keys()
            .cloned()
            .collect();
        mints.extend(self.exec.live_manual.lock().await.keys().cloned());
        mints.extend(self.exec.live_diverged.lock().await.keys().cloned());
        if let Some(trader) = &self.exec.live {
            mints.extend(trader.pending_mints());
        }
        mints = mints
            .into_iter()
            .map(|m| match m.split_once('#') {
                // Base58 never contains '#', so the suffix is always ours.
                Some((base, _)) => base.to_string(),
                None => m,
            })
            .collect();
        mints.sort();
        mints.dedup();
        mints
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
        let tick_result = self.reserves.fetch_tick(mint, &signal.curve).await;
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
            self.exec.synthetic_mints.lock().await.insert(mint.to_string());
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
                            .live_diverged
                            .lock()
                            .await
                            .insert(mint.to_string(), "live buy skipped: ticker watch failed".to_string());
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







    /// Fire a card when alerting is enabled; silent otherwise.
    pub(crate) fn emit(&self, message: String) {
        if let Some(alerts) = &self.alerts {
            alerts.alert(message);
        }
    }

    /// Clone the per-mint serialization mutex, creating it on first use.
    /// Single home for the entry-or-insert dance (buy/sell/tick paths
    /// share it): one small mutex per mint, clones keep working after
    /// eviction. The map guard is released before the mutex is locked
    /// (never held across), so [`Pipeline::maybe_unwatch`] eviction
    /// cannot deadlock against this path.
    pub(crate) async fn lock_for(
        map: &Mutex<HashMap<String, Arc<Mutex<()>>>>,
        mint: &str,
    ) -> Arc<Mutex<()>> {
        map.lock()
            .await
            .entry(mint.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Clone the per-mint tick mutex, creating it on first use. The map
    /// guard is released before the mutex is locked (never held across),
    /// so [`Pipeline::maybe_unwatch`] eviction cannot deadlock against
    /// this path: clones held by in-flight tasks keep working after
    /// eviction, and eviction only happens on a confirmed-empty book.
    pub(crate) async fn mint_serial(&self, mint: &str) -> tokio::sync::OwnedMutexGuard<()> {
        Self::lock_for(&self.tick_locks, mint).await.lock_owned().await
    }

    /// Drop a mint's serialization entries without a tick. Used by
    /// `manual_sell` terminal paths (untracked / unroutable) where no
    /// tick will ever reach `maybe_unwatch`. Safe while a serial guard
    /// is held: in-flight clones keep working after map eviction.
    pub(crate) async fn evict_mint_locks(&self, mint: &str) {
        self.tick_locks.lock().await.remove(mint);
        self.exec.live_sell_locks.lock().await.remove(mint);
    }

    /// Shared close-path tail: unwatch the feed on a confirmed-empty
    /// state (no paper positions, no tracked live fills, no watchlisted
    /// signal), evict this mint's serialization locks so the lock maps
    /// stay bounded, and hunt migrations on graduated curves. Called by
    /// both `on_tick` branches so the rule cannot diverge again; every
    /// step is idempotent, so concurrent empties are harmless.
    ///
    /// Manual-parked bags do NOT pin the feed once the curve is gone:
    /// they are never auto-probed (only an explicit `sell <mint>`
    /// touches them, and it fetches its own tick plus recreates the
    /// serial entries on demand), so keeping graduated curves watched
    /// would grow the watch set without bound on every give-up. While
    /// the curve is still live, manual bags DO pin the watch — a later
    /// graduation must still fire the migration card and pool follow
    /// (the operator's `sell` tick would too, but automation should
    /// not depend on it). Custody stays listed in `live_manual` and the
    /// shutdown snapshot either way.
    pub(crate) async fn maybe_unwatch(&self, tick: &PriceTick) {
        if !self
            .portfolio
            .lock()
            .await
            .positions_for_mint(&tick.mint)
            .is_empty()
        {
            return;
        }
        if self.exec.live_positions.lock().await.contains_key(&tick.mint) {
            return;
        }
        // Manual-parked bags pin the watch only while the curve is still
        // live (later graduation must fire the migration hunt + follow).
        // Once complete, the hunt runs below and the feed is released.
        if !tick.complete && self.exec.live_manual.lock().await.contains_key(&tick.mint) {
            return;
        }
        if self.gate_state.lock().await.gate.contains(&tick.mint) {
            return;
        }
        // Lock-map eviction is unconditional on a confirmed-empty book:
        // gating it on `unwatch() == true` leaks entries for mints that
        // were never watched (restart replays, watch-failed diverges) or
        // were already unwatched by a concurrent empty. Clones held by
        // in-flight tasks keep working after eviction, so this is safe.
        if self.reserves.unwatch(&tick.mint).await {
            info!(mint = %tick.mint, "unwatched (nothing left)");
        }
        self.tick_locks.lock().await.remove(&tick.mint);
        self.exec.live_sell_locks.lock().await.remove(&tick.mint);
        if tick.complete {
            self.report_migration_once(&tick.mint, &tick.curve).await;
        }
    }

    /// One price tick: evaluate exits for every open position on the mint,
    /// close on trigger, retry failed live exits, unwatch when nothing
    /// remains.
    pub async fn on_tick(&self, tick: &PriceTick) {
        // Stale curve ticks must not drive exits: a hung ticker round
        // (60s overall timeout) would otherwise TP/SL on an old print.
        let age = now_secs().saturating_sub(tick.observed_at);
        if age >= CURVE_TICK_STALE_SECS {
            warn!(mint = %tick.mint, age, observed_at = tick.observed_at, "stale curve tick — dropped");
            return;
        }
        // Per-mint serialization: two ticks for one mint must not
        // interleave close math and live probes (fractions computed from
        // divergent views would double-sell or strand dust).
        let _serial = self.mint_serial(&tick.mint).await;
        let mut portfolio = self.portfolio.lock().await;
        let ids: Vec<u64> = portfolio.positions_for_mint(&tick.mint);
        let mut closed_fraction = 0.0f64;
        // Breaker inputs AND alert cards are collected under the guard
        // and flushed after it drops: neither the day-loss lock nor
        // alert dispatch may run while the portfolio lock is held.
        let mut closed_pnls: Vec<f64> = Vec::new();
        let mut closed_cards: Vec<(String, f64, f64, f64, bool)> = Vec::new();
        let now = now_secs();
        if ids.is_empty() {
            drop(portfolio);
            // No paper activity: retry a previously failed live exit, if any.
            if self.exec.live_positions.lock().await.contains_key(&tick.mint) {
                self.live_sell(&tick.mint, tick.price_usd, tick.liquidity_usd, 1.0).await;
            }
            // Pending buys recheck regardless of paper state (cheap when
            // a round is already in flight: late callers skip).
            self.recheck_live_pending().await;
            // Shared tail: unwatch on confirmed-empty, evict locks,
            // hunt migrations on graduated curves.
            self.maybe_unwatch(tick).await;
            return;
        } else {
            // `tick.price_usd` embeds the SOL price from fetch time, and
            // SOL may be staler than the curve print (up to ~240s
            // combined). Never fire price-triggered TP/SL on a USD value
            // whose FX leg is dead — defer the round. Migration closes
            // bypass this (the curve is gone; `live_sell` still gates on
            // fresh SOL itself).
            if !tick.complete && self.sol_feed.price().is_none() {
                drop(portfolio);
                warn!(mint = %tick.mint, "stale SOL/USD — paper exits deferred (no static fallback)");
                self.recheck_live_pending().await;
                self.maybe_unwatch(tick).await;
                return;
            }
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
                        // Defer the day-loss feed: recording here would
                        // acquire the breaker lock while the portfolio
                        // guard is held (lock-order inversion vs opens).
                        closed_pnls.push(trade.net_pnl);
                        let is_moonbag_exit = trade.fraction_closed < 1.0;
                        // Runner bookkeeping (stop ratchet, timeout lift,
                        // moonbag disarm) lives in settle(); nothing to do.
                        let equity = portfolio.snapshot(&HashMap::new()).equity;
                        info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", trade.net_pnl),
                        pct = format!("{:+.2}%", trade.net_pnl_percent),
                        "paper position closed");
                        closed_cards.push((
                            format!("{:?}", trade.reason),
                            trade.net_pnl,
                            trade.net_pnl_percent,
                            equity,
                            is_moonbag_exit,
                        ));
                        closed_fraction = (closed_fraction + trade.fraction_closed).min(1.0);
                    }
                    Err(e) => warn!(mint = %tick.mint, id, error = %e, "paper close failed"),
                }
            }
        }

        drop(portfolio);
        for (reason, pnl, pct, equity, moonbag) in closed_cards {
            self.emit(Alerter::format_exit(
                &tick.mint, &reason, pnl, pct, equity, moonbag,
            ));
        }
        // Live leg first, breaker second: when live still holds after a
        // paper close, the paper profit must NOT feed the day-loss gate
        // yet (it would mask real loss). Defer until the live leg
        // settles (sold / manual / nothing held).
        let live_settled = if closed_fraction > 0.0 {
            self.live_sell(&tick.mint, tick.price_usd, tick.liquidity_usd, closed_fraction)
                .await
        } else {
            true
        };
        // Feed the day-loss breaker with the guard released. Lock order
        // is now uniform everywhere (breaker and portfolio are never held
        // together), so concurrent opens and ticks cannot deadlock.
        // Exits keep working even past the cap — only new entries halt.
        if live_settled {
            // Synthetic (override-priced) paper has no market behind its
            // quote: recording it would let fiction trip — or mask — the
            // gate. Warn once per close batch, then drop it.
            if self.exec.synthetic_mints.lock().await.contains(&tick.mint) {
                warn!(mint = %tick.mint, n = closed_pnls.len(), "synthetic paper PnL excluded from day-loss breaker (override price, no market)");
            } else {
                for pnl in closed_pnls {
                    self.daily.lock().await.record(pnl, now);
                }
            }
        } else {
            warn!(mint = %tick.mint, "paper closed but live still holds; breaker PnL deferred until live settles");
            // Stash (don't drop): flushed into the breaker when the live
            // leg settles — otherwise the loss vanishes from the gate.
            if !closed_pnls.is_empty() {
                let mut stashed = self.exec.deferred_pnl.lock().await;
                let slot = stashed.entry(tick.mint.clone()).or_insert((0.0, 0));
                for pnl in closed_pnls {
                    // Mirror `DailyBreaker::record`: non-finite PnL never
                    // feeds the gate (skip here so one NaN cannot poison
                    // the stashed total into a skip-everything).
                    if pnl.is_finite() {
                        slot.0 += pnl;
                        slot.1 += 1;
                    }
                }
            }
        }
        // Pending buys recheck before the unwatch decision so a late
        // confirmation keeps its feed (cheap when a round is already in
        // flight: late callers skip).
        self.recheck_live_pending().await;
        // Divergence ends with the paper lifecycle: no open paper
        // position, nothing to be diverged from. Bounds `live_diverged`
        // growth (skips heal here if they never got a buy).
        if self
            .portfolio
            .lock()
            .await
            .positions_for_mint(&tick.mint)
            .is_empty()
        {
            self.exec.live_diverged.lock().await.remove(&tick.mint);
            self.exec.synthetic_mints.lock().await.remove(&tick.mint);
            // Deferred paper PnL settles here only when no live exposure
            // remains to settle it later (flushed, not dropped — the
            // economics already happened). A still-held bag keeps its
            // stash for the `live_sell` flush paths.
            let live_held = self.exec.live_positions.lock().await.contains_key(&tick.mint)
                || self.exec.live_manual.lock().await.contains_key(&tick.mint);
            if !live_held {
                self.flush_deferred_pnl(&tick.mint).await;
            }
        }
        // Shared tail (see above): re-checks under fresh locks, so a
        // position opened by the gate task during the awaits above is
        // never unwatched from under it.
        self.maybe_unwatch(tick).await;
    }

    /// Hunt and announce the migration exactly once per mint. Runs after
    /// positions close: classification plus pool addresses for instant
    /// Raydium routing follow-ups.
    async fn report_migration_once(&self, mint: &str, curve: &str) {
        {
            let mut migrated = self.migrated.lock().await;
            let now = now_secs();
            if migrated.insert(mint.to_string(), now).is_some() {
                return;
            }
            // Bounded growth: graduated mints never un-graduate, so
            // pruning day-old entries only risks a duplicate card.
            migrated.retain(|_, at| now.saturating_sub(*at) <= MIGRATED_TTL_SECS);
            if migrated.len() > MIGRATED_CAP
                && let Some(oldest) = migrated
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(mint, _)| mint.clone())
            {
                migrated.remove(&oldest);
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
        // Seed the follow entry NOW (baseline set on first valid tick):
        // pools that never tick would otherwise stay watched forever —
        // the idle sweep below only sees `followed`, and a dead pool
        // burns 2 vault RPCs per round with no expiry. `or_insert` keeps
        // a baseline a racing tick already seeded.
        self.followed
            .lock()
            .await
            .entry(mint.to_string())
            .or_insert(FollowState {
                entry_price: f64::NAN,
                alerted_spike: false,
                last_seen_secs: now_secs(),
            });
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
        let now = now_secs();
        // Evict idle follows first (dead pools): collect under the guard,
        // unwatch outside it — vaults own a separate lock that must never
        // be acquired while `followed` is held.
        let stale: Vec<String> = {
            let followed = self.followed.lock().await;
            followed
                .iter()
                .filter(|(_, state)| now.saturating_sub(state.last_seen_secs) > FOLLOW_TTL_SECS)
                .map(|(mint, _)| mint.clone())
                .collect()
        };
        for mint in &stale {
            self.vaults.unwatch(mint).await;
        }
        if !stale.is_empty() {
            let mut followed = self.followed.lock().await;
            for mint in &stale {
                followed.remove(mint);
            }
        }
        let mut followed = self.followed.lock().await;
        let state = followed.entry(tick.mint.clone()).or_insert(FollowState {
            entry_price: tick.price_usd,
            alerted_spike: false,
            last_seen_secs: now,
        });
        state.last_seen_secs = now;
        if state.alerted_spike {
            return;
        }
        // Baseline for follows seeded without a first tick (see
        // `follow_pool`): the first valid print sets it, never alerts.
        if state.entry_price.is_nan() {
            state.entry_price = tick.price_usd;
            return;
        }
        if tick.price_usd >= state.entry_price * FOLLOW_SPIKE_MULT {
            state.alerted_spike = true;
            let mint = tick.mint.clone();
            drop(followed);
            info!(mint = %mint, multiple = FOLLOW_SPIKE_MULT, price = tick.price_usd, "post-migration spike");
            self.emit(Alerter::format_follow_spike(
                &mint,
                FOLLOW_SPIKE_MULT,
                tick.price_usd,
            ));
            // Alert fired: release the pool watch and the follow entry so a
            // dead pool is not polled forever (unbounded growth).
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
        /// Transient RugCheck failures (indexing latency) before an
        /// allow: fails `fails_left` times, then allows with `score`.
        Flaky {
            fails_left: std::sync::Mutex<usize>,
            score: u32,
        },
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
                AuditOutcome::Flaky { fails_left, score } => {
                    let mut fails = fails_left.lock().expect("lock");
                    if *fails > 0 {
                        *fails -= 1;
                        Err(RugCheckError::Deserialization(
                            serde_json::from_str::<u32>("bogus").expect_err("must fail"),
                        ))
                    } else {
                        Ok(RugCheckReport {
                            mint: mint.to_string(),
                            score: *score,
                            risks: vec![],
                            token: None,
                        })
                    }
                }
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
        /// When set, sells fail transiently (`Unconfirmed`, consumes the
        /// retry budget) instead of deterministically (`BadOrder`, which
        /// re-probes without consuming it).
        fail_sells_transient: std::sync::atomic::AtomicBool,
        /// Overlap-test hook: sells of `block_mint` wait on `unblock`
        /// until notified; `started` records every entered sell so the
        /// test can prove two mints overlap instead of serializing.
        block_mint: StdMutex<Option<String>>,
        started: StdMutex<Vec<String>>,
        unblock: tokio::sync::Notify,
    }

    impl StubLive {
        fn working() -> Self {
            Self {
                inner: Arc::new(StubLiveInner {
                    buys: StdMutex::new(vec![]),
                    sells: StdMutex::new(vec![]),
                    fail_buys: std::sync::atomic::AtomicBool::new(false),
                    fail_sells: std::sync::atomic::AtomicBool::new(false),
                    fail_sells_transient: std::sync::atomic::AtomicBool::new(false),
                    block_mint: StdMutex::new(None),
                    started: StdMutex::new(vec![]),
                    unblock: tokio::sync::Notify::new(),
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
            _liquidity_usd: f64,
        ) -> Result<String, LiveError> {
            if self
                .inner
                .fail_sells
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::BadOrder("stubbed sell failure".to_string()));
            }
            if self
                .inner
                .fail_sells_transient
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::Unconfirmed("stubbed transient sell failure".to_string()));
            }
            self.inner
                .started
                .lock()
                .expect("lock")
                .push(position.mint.clone());
            if self.inner.block_mint.lock().expect("lock").as_deref()
                == Some(position.mint.as_str())
            {
                self.inner.unblock.notified().await;
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
        /// When true, `watch` fails (exit-feed outage plumbing).
        fail_watch: bool,
        /// Mint decimals reported by `token_decimals` (6 = standard).
        decimals: u8,
    }

    impl StubReserves {
        fn with_tick(tick: PriceTick) -> Self {
            Self {
                tick: Some(tick),
                watched: StdMutex::new(vec![]),
                unwatched: StdMutex::new(vec![]),
                fail_watch: false,
                decimals: 6,
            }
        }

        fn failing() -> Self {
            Self {
                tick: None,
                watched: StdMutex::new(vec![]),
                unwatched: StdMutex::new(vec![]),
                fail_watch: false,
                decimals: 6,
            }
        }

        fn with_decimals(mut self, decimals: u8) -> Self {
            self.decimals = decimals;
            self
        }
    }

    impl ReserveFeed for StubReserves {
        async fn fetch_tick(&self, mint: &str, _curve: &str) -> Result<PriceTick, CurveError> {
            self.tick
                .clone()
                .filter(|t| t.mint == mint)
                .ok_or(CurveError::AccountMissing(mint.to_string()))
        }

        async fn token_decimals(&self, _mint: &str) -> Result<u8, CurveError> {
            Ok(self.decimals)
        }

        async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError> {
            if self.fail_watch {
                return Err(CurveError::AccountMissing(mint.to_string()));
            }
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
            observed_at: now_secs(),
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
        /// Wallets that fail screening even when `dev` passes (creator
        /// cohort tests).
        reject_wallets: StdMutex<Vec<String>>,
        swaps: StdMutex<HashMap<String, Vec<SwapRecord>>>,
        bundle: StdMutex<BundleOutcome>,
        migration: StdMutex<Option<MigrationReport>>,
        truncated: std::sync::atomic::AtomicBool,
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
                    reject_wallets: StdMutex::new(Vec::new()),
                    swaps: StdMutex::new(HashMap::new()),
                    bundle: StdMutex::new(BundleOutcome::Safe),
                    migration: StdMutex::new(None),
                    truncated: std::sync::atomic::AtomicBool::new(false),
                }),
            }
        }

        fn with_dev(dev: DevOutcome) -> Self {
            let stub = Self::passing();
            *stub.inner.dev.lock().expect("lock") = dev;
            stub
        }

        /// Fail screening for one wallet (creator-cohort tests) while
        /// every other wallet follows `dev`.
        fn rejecting(wallet: &str) -> Self {
            let stub = Self::passing();
            stub.inner.reject_wallets.lock().expect("lock").push(wallet.to_string());
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
        async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
            if self.inner.reject_wallets.lock().expect("lock").contains(&dev.to_string()) {
                return Ok(DevVerdict {
                    pass: false,
                    age_secs: Some(100),
                    prior_launches: 0,
                    reason: "stub cohort reject".to_string(),
                });
            }
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
        ) -> Result<(Vec<SwapRecord>, Option<String>, bool), ScreenError> {
            let swaps = self
                .inner
                .swaps
                .lock()
                .expect("lock")
                .get(mint)
                .cloned()
                .unwrap_or_default();
            let truncated = self
                .inner
                .truncated
                .load(std::sync::atomic::Ordering::Relaxed);
            Ok((swaps, Some("sig-stub".to_string()), truncated))
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
        let vaults = VaultTicker::new(
            "https://api.mainnet-beta.solana.com",
            CurveTicker::parse_commitment("confirmed").expect("known commitment"),
            std::time::Duration::from_millis(500),
            SolUsdFeed::new_for_test(150.0),
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
            SolUsdFeed::new_for_test(150.0),
        )
    }

    fn funded() -> PaperPortfolio {
        PaperPortfolio::new(100.0, 0.0).with_fill_model(FillModel::Exact)
    }

    #[test]
    fn entry_prefers_live_tick_over_override() {
        let live = tick("M", 2.0);
        assert_eq!(
            resolve_entry_with_flag(Ok(live), Some(9.0), 500.0),
            Some((2.0, 30_000.0, false))
        );
    }

    #[test]
    fn entry_falls_back_to_override() {
        let err: Result<PriceTick, CurveError> = Err(CurveError::AccountMissing("M".to_string()));
        assert_eq!(
            resolve_entry_with_flag(err, Some(9.0), 500.0),
            Some((9.0, 500.0, true))
        );
    }

    #[test]
    fn entry_is_none_without_price_or_override() {
        let err: Result<PriceTick, CurveError> = Err(CurveError::AccountMissing("M".to_string()));
        assert_eq!(resolve_entry_with_flag(err, None, 0.0), None);
    }

    #[test]
    fn dev_reputation_ramps_with_age_and_priors() {
        fn approx(v: f64, want: f64) {
            assert!((v - want).abs() < 1e-12, "got {v}, want {want}");
        }
        // Just old enough: 0.7; long-aged: 1.0; priors discount slightly.
        approx(dev_reputation_score(Some(3600), 0, 3600), 0.7);
        approx(dev_reputation_score(Some(14_400), 0, 3600), 1.0);
        approx(dev_reputation_score(Some(100_000), 0, 3600), 1.0);
        approx(dev_reputation_score(Some(14_400), 1, 3600), 0.95);
        // Unknown age (unreachable via screening, which rejects ageless
        // wallets) scores below any survivor; heavy priors bottom out.
        approx(dev_reputation_score(None, 0, 3600), 0.6);
        approx(dev_reputation_score(Some(3600), 100, 3600), 0.4);
        // Disabled age gate scores full trust.
        approx(dev_reputation_score(Some(1), 0, 0), 1.0);
    }

    #[test]
    fn entry_rejects_degenerate_tick() {
        // A feed print of NaN/zero/negative price (or bad liquidity) is
        // worse than none: it must drop the signal, never open on it.
        for price in [f64::NAN, 0.0, -1.5, f64::INFINITY] {
            let bad = PriceTick {
                mint: "M".to_string(),
                curve: "C".to_string(),
                price_usd: price,
                liquidity_usd: 30_000.0,
                complete: false,
                observed_at: 2_000,
            };
            assert_eq!(
                resolve_entry_with_flag(Ok(bad), Some(9.0), 500.0),
                None,
                "price {price}"
            );
        }
        for liq in [f64::NAN, -100.0, f64::INFINITY] {
            let bad = PriceTick {
                mint: "M".to_string(),
                curve: "C".to_string(),
                price_usd: 2.0,
                liquidity_usd: liq,
                complete: false,
                observed_at: 2_000,
            };
            assert_eq!(resolve_entry_with_flag(Ok(bad), Some(9.0), 500.0), None, "liq {liq}");
        }
    }

    #[test]
    fn entry_rejects_bad_override_and_liquidity() {
        let missing =
            || Result::<PriceTick, CurveError>::Err(CurveError::AccountMissing("M".to_string()));
        for quote in [f64::NAN, 0.0, -9.0] {
            assert_eq!(
                resolve_entry_with_flag(missing(), Some(quote), 500.0),
                None,
                "quote {quote}"
            );
        }
        // Bad fallback liquidity degrades to 0 rather than poisoning risk.
        assert_eq!(
            resolve_entry_with_flag(missing(), Some(9.0), f64::NAN),
            Some((9.0, 0.0, true))
        );
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
    async fn audit_transient_watchlists_for_retry() {
        // RugCheck 400 on a seconds-old mint (indexing latency) must NOT
        // burn the launch at second 0: watchlist with no score and retry
        // on the gate cadence instead.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Flaky { fails_left: std::sync::Mutex::new(9999), score: 100 },
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        assert_eq!(pipe.pending_count().await, 1);
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        let state = pipe.gate_state.lock().await;
        assert_eq!(state.meta["MINT"].audit_score, None);
    }

    #[tokio::test]
    async fn audit_retry_recovers_and_opens() {
        // The retry lands (indexing caught up): quorum confirms with the
        // resolved score and the position opens.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Flaky { fails_left: std::sync::Mutex::new(1), score: 100 },
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        assert_eq!(pipe.pending_count().await, 1);
        // Age the attempt past the retry throttle so this round re-polls.
        pipe.gate_state
            .lock()
            .await
            .meta
            .get_mut("MINT")
            .expect("meta")
            .last_audit_secs = 0;
        pipe.poll_gates().await;
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn audit_unresolvable_aborts_at_deadline() {
        // Persistent indexing failure still fails closed — at the gate
        // deadline, never at second 0 (the mint gets its full window).
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12]));
        let pipe = pipeline_screened(
            settings(&[("GATE_WINDOW_SECS", "0")]),
            AuditOutcome::Flaky { fails_left: std::sync::Mutex::new(9999), score: 100 },
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
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
    async fn stale_poll_claim_is_taken_over() {
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            StubScreener::passing(),
            None,
        );
        assert!(pipe.try_begin_poll().await);
        // Fresh claim blocks a second overlapping round.
        assert!(!pipe.try_begin_poll().await);
        pipe.end_poll().await;
        assert!(pipe.try_begin_poll().await);
        pipe.end_poll().await;
        // Ancient claim (owning task died before end_poll) is taken over
        // instead of stalling the gate forever.
        {
            let mut state = pipe.gate_state.lock().await;
            state.poll_busy = true;
            state.poll_started_secs = 0;
        }
        assert!(pipe.try_begin_poll().await);
        pipe.end_poll().await;
    }

    #[tokio::test]
    async fn shutdown_snapshot_lists_open_and_live_state() {
        let live = StubLive::working();
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            StubScreener::passing(),
            Some(live),
        );
        // Empty book: nothing open, nothing tracked, nothing pending.
        assert!(pipe.open_position_mints().await.is_empty());
        assert!(pipe.live_tracked_mints().await.is_empty());
        assert_eq!(pipe.pending_count().await, 0);
        // Manual paper open + tracked live fill surface by mint.
        let opened_now = now_secs();
        pipe.portfolio
            .lock()
            .await
            .open_position(
                "MINT",
                Side::Long,
                2.0,
                10.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                opened_now,
            )
            .expect("manual open");
        pipe.exec.live_positions.lock().await.insert(
            "LIVE".to_string(),
            LivePosition {
                mint: "LIVE".to_string(),
                curve: "CURVE".to_string(),
                assoc_curve: "ASSOC".to_string(),
                amount_tokens: 1_000,
                creator: None,
            },
        );
        assert_eq!(pipe.open_position_mints().await, vec!["MINT".to_string()]);
        assert_eq!(pipe.live_tracked_mints().await, vec!["LIVE".to_string()]);
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
        // The exit loss still feeds the breaker even though recording now
        // happens after the portfolio guard is released (lock-order fix).
        assert!(pipe.daily.lock().await.day_net < -10.0);
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
    async fn truncated_round_never_confirms() {
        // Quorum swaps on a truncated round: no confirm, no watermark
        // advance — the entry stays watchlisted for the next round.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        screener
            .inner
            .truncated
            .store(true, std::sync::atomic::Ordering::Relaxed);
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
        assert_eq!(pipe.pending_count().await, 1);
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
        // Real pubkey mint: the pool follow validates it at the vault
        // boundary (garbage mints fail fast instead of tracking nothing).
        let mint = fixture_wallet(30);
        let pipe = pipeline_alerted(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(&mint, 2.0)),
            funded(),
            screener,
            None,
            Some(alerts.clone()),
        );
        let mut graduated = tick(&mint, 2.0);
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
        // Follow entry seeded at follow time (not first tick): dead pools
        // expire via the idle sweep instead of watching forever.
        let followed = pipe.followed.lock().await;
        let state = followed.get(&mint).expect("follow seeded");
        assert!(state.entry_price.is_nan(), "baseline set on first tick");
        assert!(!state.alerted_spike);
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
    async fn launch_skips_on_creator_reject() {
        // v2 creator fails screening while the launcher passes: no
        // watchlist entry (fail-closed AND over both insider wallets).
        let creator = fixture_wallet(8);
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            StubScreener::rejecting(&creator),
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), Some(&creator), 5000)
            .await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn launch_watches_creator_cohort_dump() {
        // Distinct v2 creator is screened, watchlisted, and its dump
        // aborts the gate like the launcher's.
        let creator = fixture_wallet(8);
        let dump = SwapRecord::new("dump", &creator, SwapSide::Sell, 1003, 1_000_000)
            .expect("fixture");
        let screener = StubScreener::with_swaps("MINT", vec![dump]);
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), Some(&creator), 5000)
            .await;
        assert_eq!(pipe.pending_count().await, 1);
        pipe.poll_gates().await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn launch_skips_on_audit_reject_and_error() {
        // Policy rejection drops at once (never queued).
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Reject,
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

    #[tokio::test]
    async fn launch_watchlists_on_audit_transport_error() {
        // Transport-level audit failure (Broken body) is retryable, not
        // a reject: watchlist with no score, open nothing yet.
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Broken,
            reserves,
            funded(),
            StubScreener::passing(),
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 1);
        assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
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
    async fn confirm_drops_non_six_decimal_mint() {
        // Sizing scales by 10^6 everywhere: a 9-decimal mint would
        // misprice entries 1000x — drop loudly instead of guessing.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0)).with_decimals(9);
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000).await;
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
    async fn synthetic_override_pnl_skips_breaker() {
        // Override-priced paper is fiction (operator-set quote, no
        // market): its close must not feed the day-loss gate, or fiction
        // could trip — or mask — the breaker.
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
        assert!(pipe.exec.synthetic_mints.lock().await.contains("MINT"));
        // +100% over the 3.0 override entry clears TP with real profit.
        pipe.on_tick(&tick("MINT", 6.0)).await;
        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history().len(), 1);
        assert!(portfolio.history()[0].net_pnl > 0.0);
        drop(portfolio);
        assert_eq!(pipe.daily.lock().await.day_net, 0.0);
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
                    now_secs(),
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
        let now = now_secs();
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
                now,
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
        timeout_tick.observed_at = now + 61;
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

        // Paper record survives; nothing tracked for exits, but the
        // divergence is explicit (paper WITHOUT live cover).
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(pipe.exec.live_positions.lock().await.is_empty());
        assert!(pipe.exec.live_diverged.lock().await.contains_key("MINT"));
        assert!(pipe.live_tracked_mints().await.contains(&"MINT".to_string()));
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
    async fn manual_sell_closes_both_legs() {
        // `sell <mint>`: paper closes at Manual and the whole tracked
        // live bag sells, without waiting for any TP/SL trigger. (A real
        // base58 mint: the daemon rejects malformed request files.)
        let sys = "11111111111111111111111111111111";
        let live = StubLive::working();
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(sys, 2.0)),
            funded(),
            StubScreener::passing(),
            Some(live.clone()),
        );
        let opened_now = now_secs();
        pipe.portfolio
            .lock()
            .await
            .open_position(
                sys,
                Side::Long,
                2.0,
                10.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                opened_now,
            )
            .expect("manual open");
        pipe.exec.live_positions.lock().await.insert(
            sys.to_string(),
            LivePosition {
                mint: sys.to_string(),
                curve: "CURVE".to_string(),
                assoc_curve: "ASSOC".to_string(),
                amount_tokens: 1_000,
                creator: None,
            },
        );
        assert!(pipe.manual_sell(sys).await);
        let portfolio = pipe.portfolio.lock().await;
        assert!(portfolio.position_ids().is_empty());
        assert_eq!(portfolio.history()[0].reason, ExitReason::Manual);
        drop(portfolio);
        assert_eq!(live.sells(), vec![(sys.to_string(), 1_000)]);
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn manual_sell_unknown_mint_settles() {
        // Nothing tracked anywhere: settle at once so the request file
        // goes away instead of retrying forever.
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            StubScreener::passing(),
            None,
        );
        assert!(pipe.manual_sell("NOPE").await);
        assert!(pipe.manual_sell("11111111111111111111111111111111").await);
    }

    #[tokio::test]
    async fn manual_sell_exits_parked_manual_bag() {
        // A give-up-parked bag has no automatic exit left — but an
        // explicit `sell` resurrects it into tracked and sells it. This
        // is the lifecycle that bounds `live_manual`: parked bags leave
        // through this command, never through silent eviction.
        let sys = "11111111111111111111111111111111";
        let live = StubLive::working();
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(sys, 2.0)),
            funded(),
            StubScreener::passing(),
            Some(live.clone()),
        );
        pipe.exec.live_manual.lock().await.insert(
            sys.to_string(),
            LivePosition {
                mint: sys.to_string(),
                curve: "CURVE".to_string(),
                assoc_curve: "ASSOC".to_string(),
                amount_tokens: 1_000,
                creator: None,
            },
        );
        assert!(pipe.manual_sell(sys).await);
        assert_eq!(live.sells(), vec![(sys.to_string(), 1_000)]);
        assert!(pipe.exec.live_manual.lock().await.is_empty());
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
    async fn scalp_mode_full_exits_in_tp_band() {
        // STRATEGY_MODE=scalp: high-momentum TP 60 clamps to 35, moonbag
        // killed → one full exit, no runner left behind.
        let buyers: Vec<u8> = (20..55).collect();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &buyers));
        let pipe = pipeline_screened(
            settings(&[("STRATEGY_MODE", "scalp")]),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        // +40% clears the clamped 35% TP: full exit, nothing rides.
        pipe.on_tick(&tick("MINT", 2.8)).await;
        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history().len(), 1);
        assert_eq!(portfolio.history()[0].fraction_closed, 1.0);
        assert!(portfolio.position_ids().is_empty());
    }

    #[tokio::test]
    async fn runner_mode_forces_moonbag_and_lifts_timeout() {
        // STRATEGY_MODE=runner: moderate momentum (TP 35, no moonbag, no
        // trail in profile) still scales out 80/20 with trailing armed,
        // and the runner survives past the 90s time-stop.
        let buyers: Vec<u8> = (20..30).collect();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &buyers));
        let pipe = pipeline_screened(
            settings(&[("STRATEGY_MODE", "runner")]),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("MINT", 2.0)),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        // +40% clears TP 35: 80% out, 20% runner with trailing armed
        // and the 90s time-stop lifted (checked directly: position
        // timeouts run on wall-clock, which tests cannot fast-forward).
        pipe.on_tick(&tick("MINT", 2.8)).await;
        {
            let portfolio = pipe.portfolio.lock().await;
            assert_eq!(portfolio.history()[0].fraction_closed, 0.8);
            let runner = portfolio.position(portfolio.position_ids()[0]).expect("runner");
            assert!(runner.trailing_stop);
            assert_eq!(runner.timeout_seconds, simulator::RUNNER_BACKSTOP_SECS);
            assert_eq!(runner.moonbag_percent, 0.0);
            // Runner trail is the wide one (20%), not the shared 5%.
            assert_eq!(runner.trail_percent, 20.0);
        }
        // Drifting inside the trail band: no exit fires either.
        pipe.on_tick(&tick("MINT", 2.68)).await;
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
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

        // Success resets the failure budget: the runner exit fires on the
        // very next tick with no cooldown workaround (previously the
        // success itself stamped the cooldown and ate the budget).
        assert!(!pipe.exec.live_retry.lock().await.contains_key("MINT"));
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
                    now_secs(),
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
                now_secs(),
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
        live.inner.fail_sells_transient.store(true, Ordering::Relaxed);
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
        // parks to manual (still HELD) and stops auto-probing the mint.
        {
            let mut retry = pipe.exec.live_retry.lock().await;
            retry.insert("MINT".to_string(), (5u32, 0u64));
        }
        // Paper already closed; drive another tick to trigger the retry path.
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(pipe.exec.live_positions.lock().await.is_empty());
        assert!(pipe.exec.live_manual.lock().await.contains_key("MINT"));
        assert!(pipe.live_tracked_mints().await.contains(&"MINT".to_string()));
        assert!(live.sells().is_empty());
        // Give-up settles the leg: the paper profit stashed while
        // holding flushes into the breaker (never vanishes).
        assert!(!pipe.exec.deferred_pnl.lock().await.contains_key("MINT"));
        assert!(pipe.daily.lock().await.day_net > 0.0);
    }

    #[tokio::test]
    async fn deferred_pnl_flushes_when_live_settles_late() {
        // Paper closes while live still holds: the breaker must NOT see
        // the profit yet, and must see it once the late sell lands.
        use std::sync::atomic::Ordering;
        let live = StubLive::working();
        live.inner.fail_sells_transient.store(true, Ordering::Relaxed);
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
        // Tick 1: TP closes paper, live sell fails transiently.
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(live.sells().is_empty());
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.daily.lock().await.day_net, 0.0);
        assert!(pipe.exec.deferred_pnl.lock().await.contains_key("MINT"));
        // Recover with a stale cooldown: the no-paper retry path sells
        // and the stashed profit settles into the breaker.
        live.inner.fail_sells_transient.store(false, Ordering::Relaxed);
        {
            let mut retry = pipe.exec.live_retry.lock().await;
            retry.insert("MINT".to_string(), (1u32, 0u64));
        }
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert_eq!(live.sells(), vec![("MINT".to_string(), 1_000)]);
        assert!(!pipe.exec.deferred_pnl.lock().await.contains_key("MINT"));
        assert!(pipe.daily.lock().await.day_net > 0.0);
    }

    #[tokio::test]
    async fn live_sell_deterministic_reject_is_bounded_then_manual() {
        // Pre-dispatch rejects (bad order, dust) re-probe at the cooldown
        // cadence but bounded: after the 5-attempt budget the bag parks
        // to manual instead of holding the feed forever.
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
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(live.sells().is_empty());
        let retry = pipe.exec.live_retry.lock().await;
        assert_eq!(retry.get("MINT").map(|(a, _)| *a), Some(1));
        drop(retry);
        // Still tracked for later re-probes (not yet given up).
        assert!(pipe.exec.live_positions.lock().await.contains_key("MINT"));
        // Pre-seed at the cap with a stale timestamp: next tick parks to
        // manual (still HELD) instead of retrying forever.
        {
            let mut retry = pipe.exec.live_retry.lock().await;
            retry.insert("MINT".to_string(), (5u32, 0u64));
        }
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert!(pipe.exec.live_positions.lock().await.is_empty());
        assert!(pipe.exec.live_manual.lock().await.contains_key("MINT"));
        assert!(pipe.live_tracked_mints().await.contains(&"MINT".to_string()));
    }

    #[tokio::test]
    async fn live_sells_for_different_mints_overlap() {
        // Per-mint serialization proof: a sell blocked mid-dispatch on
        // mint A must not hold up mint B. Under the old global lock this
        // deadlocks (B waits for A's lock while A waits for release) and
        // the timeouts fail the test instead of hanging CI.
        let live = StubLive::working();
        *live.inner.block_mint.lock().expect("lock") = Some("A".to_string());
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick("A", 2.0)),
            funded(),
            StubScreener::passing(),
            Some(live.clone()),
        );
        for mint in ["A", "B"] {
            pipe.exec.live_positions.lock().await.insert(
                mint.to_string(),
                LivePosition {
                    mint: mint.to_string(),
                    curve: "CURVE".to_string(),
                    assoc_curve: "ASSOC".to_string(),
                    amount_tokens: 1_000,
                    creator: None,
                },
            );
        }
        let secs = std::time::Duration::from_secs;
        tokio::time::timeout(secs(10), async {
            tokio::join!(
                async {
                    tokio::time::timeout(secs(5), pipe.live_sell("A", 3.0, 30_000.0, 1.0))
                        .await
                        .expect("A finishes after release");
                },
                async {
                    // Wait until A is inside the stub dispatch, then B
                    // must sail through while A is still blocked.
                    tokio::time::timeout(secs(5), async {
                        loop {
                            if live
                                .inner
                                .started
                                .lock()
                                .expect("lock")
                                .iter()
                                .any(|m| m == "A")
                            {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("A starts");
                    tokio::time::timeout(secs(5), pipe.live_sell("B", 3.0, 30_000.0, 1.0))
                        .await
                        .expect("B proceeds while A is blocked");
                    live.inner.unblock.notify_waiters();
                }
            )
        })
        .await
        .expect("no deadlock");
        assert!(live.sells().iter().any(|(m, _)| m == "A"));
        assert!(live.sells().iter().any(|(m, _)| m == "B"));
    }

    #[tokio::test]
    async fn live_mirror_skipped_when_watch_fails() {
        // Exit-feed outage: paper still opens (manual review), but no
        // real bag is bought without TP/SL automation.
        let live = StubLive::working();
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let mut reserves = StubReserves::with_tick(tick("MINT", 2.0));
        reserves.fail_watch = true;
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
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(live.buys().is_empty());
    }
}
