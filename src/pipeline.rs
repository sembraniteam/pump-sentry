//! Event processing: launches and tick exits.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use alert::{Alerter, Card, CardCtx};
use auditor::{RugCheckClient, RugCheckError, RugCheckReport};
use curve::{CurveError, CurveTicker, PriceTick, SolUsdFeed};
use listener::{
    BundleAnalysis, DevVerdict, EntryGate, MigrationReport, RpcScreener, ScreenError, SwapRecord,
};
use migration::VaultTicker;
use risk_engine::{RiskEngine, StrategyMode};
use simulator::{ExitReason, Mark, PaperPortfolio};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::day_state::DayBook;
use crate::follow::FollowState;
#[cfg(test)]
pub use crate::gate::DailyBreaker;
pub use crate::gate::GateState;
#[cfg(test)]
pub use crate::gate::LaunchRequest;
#[cfg(test)]
pub(crate) use crate::gate::{dev_reputation_score, fee_aware_sl_floor, resolve_entry_with_flag};
use crate::journal::{JournalLine, JournalSink};
#[cfg(test)]
use crate::live::escalated_tip;
use crate::live::{
    BuyFill, LiveBuyParams, LiveError, LivePosition, LiveSellParams, LiveTrader, PendingRecheck,
    PumpSwapSellParams, RaydiumSellParams,
};
use crate::live_mirror::{LiveExecution, SellCover, breaker_headline, live_base_mint};
use crate::settings::{Settings, now_secs};

/// One paper-book read: the card-side cash/equity plus the `/pnl`
/// ledger panel, so every money surface answers the same snapshot
/// instead of two locks apart.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperBookView {
    pub cash: f64,
    pub equity: f64,
    pub pnl: crate::status::PnlReport,
}
#[cfg(test)]
pub(crate) use listener::BundleVerdict;

/// Audit source.
pub trait Auditor {
    /// Evaluate a mint; `Err` means skip.
    async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError>;
}

impl Auditor for RugCheckClient {
    async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        self.evaluate_token(mint).await
    }
}

/// Reserve source.
pub trait ReserveFeed {
    async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError>;
    /// Mint decimals (cached).
    async fn token_decimals(&self, mint: &str) -> Result<u8, CurveError>;
    /// Venue fee, read live. `None` keeps standard.
    async fn venue_fee_bps(&self, _mint: &str, _curve: &str) -> Option<f64> {
        None
    }
    /// Reserve read plus tier from one chain state.
    async fn fetch_tick_with_fee(
        &self,
        mint: &str,
        curve: &str,
    ) -> Result<(PriceTick, Option<f64>), CurveError> {
        let tick = self.fetch_tick(mint, curve).await?;
        let fee = self.venue_fee_bps(mint, curve).await;
        Ok((tick, fee))
    }
    async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError>;
    /// Stop watching; true when removed.
    async fn unwatch(&self, mint: &str) -> bool;
}

impl ReserveFeed for CurveTicker {
    async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError> {
        CurveTicker::fetch_tick(self, mint, curve).await
    }

    async fn token_decimals(&self, mint: &str) -> Result<u8, CurveError> {
        CurveTicker::token_decimals(self, mint).await
    }

    async fn fetch_tick_with_fee(
        &self,
        mint: &str,
        curve: &str,
    ) -> Result<(PriceTick, Option<f64>), CurveError> {
        CurveTicker::fetch_tick_with_fee(self, mint, curve).await
    }

    async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError> {
        CurveTicker::watch(self, mint, curve).await
    }

    async fn unwatch(&self, mint: &str) -> bool {
        CurveTicker::unwatch(self, mint).await
    }
}

/// Live buy dispatch. Only used with `--live`.
pub trait BuySink {
    /// Buy per params. Returns position plus bundle.
    async fn buy(
        &self,
        req: &LiveBuyParams,
    ) -> Result<(LivePosition, String, Option<BuyFill>), LiveError>;
}

/// Live sell dispatch: curve plus both pool venues. One trait (not
/// three) because every venue shares the floor/guard contract and
/// every caller handles all three arms together.
pub trait SellSink {
    /// Sell a tracked position. `None` fee is standard.
    async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError>;

    /// Raydium sell at a pool leg. Same contract as `sell`.
    async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError>;

    /// PumpSwap sell at a pool leg. Same contract as `sell`.
    async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError>;
}

/// Unconfirmed-buy tracking plus the resend guard. Split from dispatch
/// so stubs must state their pending semantics explicitly instead of
/// inheriting silent no-ops.
pub trait PendingStore {
    /// True when a buy awaits confirmation.
    fn has_pending(&self) -> bool;

    /// Mints awaiting confirmation.
    fn pending_mints(&self) -> Vec<String>;

    /// True while a buy for `mint` awaits confirmation. Late-confirm
    /// rechecks ride that mint's ticks, so its watch must not drop.
    fn has_pending_for(&self, mint: &str) -> bool;

    /// True when the last sell bundle for `mint` is still
    /// engine-pending: resends must wait, never compete with it.
    async fn prior_sell_pending(&self, mint: &str) -> bool;

    /// Re-poll pending buys once.
    async fn recheck_pending(&self) -> Vec<PendingRecheck>;

    /// Retire a pending buy whose resolved claim is durable and
    /// published: append the `Settled` tombstone and drop the
    /// in-memory entry. Call only AFTER the replacement claim
    /// (`Position`/`Manual`) is durable — WAL-before-ack keeps the
    /// pending claim of record until then (AUD-001).
    async fn retire_pending_for(&self, mint: &str);
}

/// Tip escalation plus dispatch cost accounting. Live lifts tips to
/// the oracle floor; stubs escalate purely. Only the composite
/// queries live here (`effective_tip`, `dispatch_cost_for_tip`,
/// `first_touch_cost_lamports`) — the pieces they are built from are
/// inherent methods on each type, not trait surface.
pub trait CostLedger {
    /// Congestion-aware tip for an attempt.
    async fn effective_tip(&self, failed_attempts: u32) -> u64;

    /// Overhead at an explicit tip (escalated retries debit actuals).
    fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64;

    /// First-buy cost (dispatch plus rent). Debit on buys only.
    fn first_touch_cost_lamports(&self) -> u64;

    /// What a landed-and-errored dispatch actually charged: base plus
    /// priority (the tip transfer rolled back with the failed tx).
    fn failed_dispatch_cost_lamports(&self) -> u64;
}

/// Custody event log. Prod writes WAL-first (each append awaited
/// inside the caller, on the blocking pool); stubs record calls for
/// assertions. Async: the durability wait (open+write+fsync) never
/// blocks a Tokio worker, so callers must be async too. No defaults
/// anywhere: every implementor states each behavior, so a new method
/// forces every stub to decide.
pub trait WalSink {
    /// Record a fill (call AFTER tracking).
    async fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str);

    /// Record a manual bag (call BEFORE parking).
    async fn record_manual(&self, position: &LivePosition, note: &str);

    /// Record a durable tracked claim with no bundle attribution: a
    /// parked manual bag being resurrected for a tracked exit. Call
    /// BEFORE `unrecord_manual` (AUD-002, WAL-before-ack).
    async fn record_position(&self, position: &LivePosition);

    /// Retire the tracked claim of a bag handed from the exit path into
    /// the manual notes, so every parked bag replays with one claim
    /// (AUD-002). Call AFTER the manual claim is durable.
    async fn take_position(&self, position: &LivePosition);

    /// Record a sell (call BEFORE subtracting).
    async fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str);

    /// Retire a manual bag taken for exit (call AFTER taking): without
    /// this, a crash before compaction replays the bag as a ghost.
    async fn unrecord_manual(&self, position: &LivePosition);

    /// True once a WAL write failed this run: the claim of record is
    /// broken, so no new entry may open until a restart (exits and
    /// `sell <mint>` keep running).
    fn wal_degraded(&self) -> bool;
}

/// Umbrella bound for the full live contract. New code depends on the
/// focused traits; this alias only keeps the six `L: ...` sites below
/// readable. Blanket-implemented, never implemented by hand.
pub trait LiveSink: BuySink + SellSink + PendingStore + CostLedger + WalSink {}

impl<T: BuySink + SellSink + PendingStore + CostLedger + WalSink> LiveSink for T {}

// Trait impls below call the inherent methods explicitly
// (`LiveTrader::buy(self, ..)`): a same-name `self.buy(..)` delegation
// silently becomes unbounded recursion the day the inherent method is
// renamed away.

impl BuySink for LiveTrader {
    async fn buy(
        &self,
        req: &LiveBuyParams,
    ) -> Result<(LivePosition, String, Option<BuyFill>), LiveError> {
        LiveTrader::buy(self, req).await
    }
}

impl SellSink for LiveTrader {
    async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError> {
        LiveTrader::sell(self, req).await
    }

    async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError> {
        LiveTrader::sell_raydium(self, req).await
    }

    async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError> {
        LiveTrader::sell_pumpswap(self, req).await
    }
}

impl PendingStore for LiveTrader {
    fn has_pending(&self) -> bool {
        LiveTrader::has_pending(self)
    }

    fn pending_mints(&self) -> Vec<String> {
        LiveTrader::pending_mints(self)
    }

    fn has_pending_for(&self, mint: &str) -> bool {
        LiveTrader::has_pending_for(self, mint)
    }

    async fn prior_sell_pending(&self, mint: &str) -> bool {
        LiveTrader::prior_sell_pending(self, mint).await
    }

    async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        LiveTrader::recheck_pending(self).await
    }

    async fn retire_pending_for(&self, mint: &str) {
        LiveTrader::retire_pending_for(self, mint).await
    }
}

impl CostLedger for LiveTrader {
    async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        LiveTrader::effective_tip(self, failed_attempts).await
    }

    fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        LiveTrader::dispatch_cost_for_tip(self, tip_lamports)
    }

    fn first_touch_cost_lamports(&self) -> u64 {
        LiveTrader::first_touch_cost_lamports(self)
    }

    fn failed_dispatch_cost_lamports(&self) -> u64 {
        LiveTrader::failed_dispatch_cost_lamports(self)
    }
}

impl WalSink for LiveTrader {
    async fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str) {
        LiveTrader::acknowledge_buy(self, position, bundle_id).await
    }

    async fn record_manual(&self, position: &LivePosition, note: &str) {
        LiveTrader::record_manual(self, position, note).await
    }

    async fn record_position(&self, position: &LivePosition) {
        LiveTrader::record_position(self, position).await
    }

    async fn take_position(&self, position: &LivePosition) {
        LiveTrader::take_position(self, position).await
    }

    async fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str) {
        LiveTrader::record_sell(self, mint, amount_tokens, bundle_id).await
    }

    async fn unrecord_manual(&self, position: &LivePosition) {
        LiveTrader::unrecord_manual(self, position).await
    }

    fn wal_degraded(&self) -> bool {
        LiveTrader::wal_degraded(self)
    }
}

/// Alert sink: structured cards (text fallback + optional embed).
pub trait AlertSink {
    fn alert(&self, card: Card);
}

impl AlertSink for Alerter {
    fn alert(&self, card: Card) {
        self.send_card(card);
    }
}

/// Curve read outcome. `Truncated` carries no data on purpose:
/// partial views must never reach quorum math.
#[derive(Debug, Clone)]
pub enum CurveRead {
    /// Full window: swaps plus the newest watermark.
    Full {
        swaps: Vec<SwapRecord>,
        watermark: Option<String>,
    },
    /// Partial view (indexing/transport cut). Hold, never confirm.
    Truncated,
}

/// Screening source.
pub trait Screener {
    /// Dev verdict; `Err` skips fail-closed.
    async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError>;
    /// Dev verdict that must not ride a cached pass. Confirm-time
    /// re-screening uses this: fresh history between intake and quorum
    /// must be observed, not the intake-time verdict (AUD-004).
    /// Default defers to `screen_dev` so screens with no cache (or no
    /// history-walk) keep one path.
    async fn screen_dev_fresh(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
        self.screen_dev(dev).await
    }
    /// Swaps since `since_sig`, plus watermark. A truncated view must
    /// never confirm — a variant, not a flag, so new consumers cannot
    /// miss it.
    async fn observe_curve(
        &self,
        mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<CurveRead, ScreenError>;
    /// Bundle forensics; `Err` drops fail-closed.
    async fn screen_bundle(
        &self,
        curve: &str,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ScreenError>;
    /// Top-10 holder share of supply, percent; `Err` drops fail-closed.
    /// Called only when the concentration cap is armed, so the extra
    /// reads cost nothing by default.
    async fn top_holder_pct(&self, mint: &str) -> Result<f64, ScreenError>;
    /// Migration hunt; `None` when not found.
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

    async fn screen_dev_fresh(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
        let dev = listener::parse_address(dev)?;
        self.dev_verdict_fresh(&dev, now_secs()).await
    }

    async fn observe_curve(
        &self,
        _mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<CurveRead, ScreenError> {
        let curve = listener::parse_address(curve)?;
        let (swaps, watermark, truncated) = self.curve_swaps(&curve, since_sig).await?;
        Ok(if truncated {
            CurveRead::Truncated
        } else {
            CurveRead::Full { swaps, watermark }
        })
    }

    async fn screen_bundle(
        &self,
        curve: &str,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ScreenError> {
        let curve = listener::parse_address(curve)?;
        self.bundle_verdict(&curve, creation_slot).await
    }

    async fn top_holder_pct(&self, mint: &str) -> Result<f64, ScreenError> {
        let mint = listener::parse_address(mint)?;
        RpcScreener::top_holder_pct(self, &mint).await
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

/// A liquidity figure that cannot price a fill. Delivered ticks carry
/// parsed reserves (`liquidity_usd` clamps non-positive to 0), so a
/// zero/NaN here is a drained market or an unusable pool — not a
/// missing read: exits defer instead of booking a fee-only fiction.
fn unpriceable_liquidity(liquidity_usd: f64) -> bool {
    !liquidity_usd.is_finite() || liquidity_usd <= 0.0
}

/// AUD-027: hunt the migration before a complete tick's close only when
/// a live leg exists to route the exit through the pool. Paper closes
/// cannot use the pool and must not burn an exit-lane permit on the
/// bounded RPC hunt; their hunt still runs post-close in `maybe_unwatch`.
pub(crate) fn should_hunt_before_close(
    live_present: bool,
    complete: bool,
    has_custody: bool,
) -> bool {
    live_present && complete && has_custody
}

/// Shared state for event, gate, and ticker tasks. Generics allow stubs.
pub struct Pipeline<
    A = RugCheckClient,
    R = CurveTicker,
    L = LiveTrader,
    S = RpcScreener,
    T = Alerter,
> {
    pub settings: Settings,
    /// Audit source.
    pub auditor: A,
    /// Engine; `None` is heuristic.
    pub engine: Option<RiskEngine>,
    /// Reserves and watch set.
    pub reserves: R,
    /// Paper book.
    pub portfolio: Mutex<PaperPortfolio>,
    /// Last mark per mint (stale drops at use).
    pub last_marks: Mutex<HashMap<String, (Mark, u64)>>,
    /// Per-mint tick locks.
    pub tick_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Live state.
    pub exec: LiveExecution<L>,
    /// Reported migrations.
    pub migrated: Mutex<HashMap<String, u64>>,
    /// Followed pools.
    pub followed: Mutex<HashMap<String, FollowState>>,
    /// Vault ticker.
    pub vaults: VaultTicker,
    /// SOL/USD feed.
    pub sol_feed: Arc<SolUsdFeed>,
    /// Screener.
    pub screener: S,
    /// Gate state.
    pub gate_state: Mutex<GateState>,
    /// Kill switch: while set, no new entry opens (exits, manual sells,
    /// and pending-buy rechecks keep running). Polled from the marker
    /// file by `run_pipeline`; set here so tests can flip it directly.
    pub entries_paused: AtomicBool,
    /// Data-degrade pause: while set, a critical data source (RPC pool,
    /// SOL/USD feed, listener ingestion) is unhealthy and entries refuse
    /// to decide on it — same choke point as the kill switch, separate
    /// state (the operator owns the marker; the supervisor owns this).
    pub data_paused: AtomicBool,
    /// Day-loss breaker (persisted: a restart never resets the day).
    pub daily: Mutex<DayBook>,
    /// Strategy.
    pub strategy: StrategyMode,
    /// Alerts; `None` silences.
    pub alerts: Option<T>,
    /// Decision journal (analytics-only, fail-soft). Detached from
    /// custody: a broken journal never gates a trade — see
    /// `journal::JournalSink`.
    journal: OnceLock<Arc<JournalSink>>,
}

/// Stale curve ticks are ignored.
pub const CURVE_TICK_STALE_SECS: u64 = 120;

impl<A, R, L, S, T> Pipeline<A, R, L, S, T> {
    /// Build shared state. `day_state` is the day-loss tally file
    /// (`None` = in-memory only); the breaker resumes today's loss
    /// from it so a restart cannot reset the limit.
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
        day_state: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        let gate = EntryGate::new(settings.gate_window_secs, settings.gate_min_buyers);
        let strategy = settings.strategy_mode.parse().unwrap_or_else(|e| {
            warn!(mode = %settings.strategy_mode, error = %e, "bad strategy mode; using adaptive");
            StrategyMode::Adaptive
        });
        let boot_now = now_secs();
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
            entries_paused: AtomicBool::new(false),
            data_paused: AtomicBool::new(false),
            daily: Mutex::new(DayBook::open(day_state, boot_now)),
            strategy,
            alerts,
            journal: OnceLock::new(),
            vaults,
            sol_feed,
            last_marks: Mutex::new(HashMap::new()),
        })
    }

    /// Attach the (analytics-only) decision journal to a built
    /// pipeline; the first sink wins, later calls no-op.
    pub fn with_journal(&self, sink: Arc<JournalSink>) {
        let _ = self.journal.set(sink);
    }

    /// Journal sink; `None` until `with_journal` and never blocking.
    pub fn journal(&self) -> Option<&JournalSink> {
        self.journal.get().map(|a| a.as_ref())
    }

    /// Journal a market-updated paper exit (tick or manual-sell path).
    pub(crate) fn journal_exit(
        &self,
        mint: &str,
        trade: &simulator::ClosedTrade,
        fraction: f64,
        mark: &Mark,
        tick_observed_at: u64,
    ) {
        if let Some(j) = self.journal() {
            j.append(&JournalLine::Exit {
                ts: now_secs(),
                mint: mint.to_string(),
                fraction,
                tick_observed_at,
                trade: trade.clone(),
                mark: *mark,
            });
        }
    }

    /// One paper-book read for every money surface: the heartbeat
    /// (cash/equity), `/pnl` (ledger panel) and the status file all
    /// answer from this single snapshot, so no two answers can
    /// disagree mid-lock.
    pub async fn paper_book_snapshot(&self) -> PaperBookView {
        let marks = self.fresh_marks(now_secs()).await;
        let portfolio = self.portfolio.lock().await;
        let snap = portfolio.snapshot(&marks);
        PaperBookView {
            cash: snap.cash,
            equity: snap.equity,
            pnl: crate::status::PnlReport {
                // Headline realized: edge minus dispatch bleed (the
                // bleed is real money the round trips paid).
                realized_pnl: portfolio.realized_net_pnl(),
                unrealized_pnl: snap.unrealized_pnl,
                mark_value: snap.mark_value,
                realizable_value: snap.realizable_value,
                overdraft_usd: snap.overdraft_usd,
                closed_count: snap.closed_count,
                win_rate: portfolio.win_rate(),
            },
        }
    }

    /// Open paper positions as `/positions` lines (mark basis). One
    /// lock-brief read; unpriced positions carry `None`, never fiction.
    pub async fn open_position_views(&self) -> Vec<crate::status::OpenPosition> {
        let now = now_secs();
        let marks = self.fresh_marks(now).await;
        let portfolio = self.portfolio.lock().await;
        portfolio
            .position_ids()
            .iter()
            .filter_map(|id| portfolio.position(*id))
            .map(|p| {
                let mark = marks
                    .get(&p.mint)
                    .map(|m| m.price)
                    .filter(|pr| pr.is_finite() && *pr > 0.0);
                crate::status::OpenPosition {
                    id: p.id,
                    mint: p.mint.clone(),
                    entry_price: p.entry_price,
                    quantity: p.quantity,
                    entry_notional: p.entry_notional,
                    opened_at: p.opened_at,
                    timeout_seconds: p.timeout_seconds,
                    mark_price: mark,
                    unrealized_usd: mark.map(|pr| p.quantity as f64 * pr - p.entry_notional),
                }
            })
            .collect()
    }

    /// Watchlist for `/watchlist`: true total plus a capped sample
    /// (the gate itself caps at 10k — the file must not flood).
    pub async fn watchlist_view(&self) -> crate::status::WatchlistSnapshot {
        let state = self.gate_state.lock().await;
        let view: Vec<crate::status::WatchEntry> = state
            .gate
            .pending_view()
            .into_iter()
            .take(crate::status::WATCHLIST_MAX_PUBLISHED)
            .map(|v| crate::status::WatchEntry {
                mint: v.mint.clone(),
                buyers: v.buyers,
                created_secs: v.created_secs,
                deadline_secs: v.deadline_secs,
                audit_score: state.meta.get(&v.mint).and_then(|s| s.audit_score),
            })
            .collect();
        let total = state.gate.pending_count();
        crate::status::WatchlistSnapshot {
            total,
            entries: view,
            min_buyers: self.settings.gate_min_buyers,
            window_secs: self.settings.gate_window_secs,
        }
    }

    /// Exact gate inputs for `/risk` (same comparisons as `gate.rs`):
    /// paper mints + basis from the same position views, live extras
    /// from the same exec state the gate reads — never re-derived
    /// downstream. Returns `(live_extra, live_bags, paper_locked)`.
    pub async fn gate_limit_inputs(&self) -> (usize, usize, f64)
    where
        L: LiveSink,
    {
        let views = self.open_position_views().await;
        let paper_mints: std::collections::HashSet<String> =
            views.iter().map(|p| p.mint.clone()).collect();
        let paper_locked: f64 = views.iter().map(|p| p.entry_notional).sum();
        let live_extra = self
            .exec
            .held_base_mints()
            .await
            .into_iter()
            .filter(|m| !paper_mints.contains(m))
            .count();
        let live_bags = self.exec.live_unpaired_bag_count(&paper_mints).await;
        (live_extra, live_bags, paper_locked)
    }

    /// Paper ledger overdraft (AUD-010): dispatch costs debited past
    /// available cash. Reporting only — the book itself refuses new
    /// opens while cash is negative (fail-closed).
    pub async fn paper_overdraft_usd(&self) -> f64 {
        let portfolio = self.portfolio.lock().await;
        (-portfolio.cash()).max(0.0)
    }

    /// Fresh marks (stale evicted, never leaks).
    async fn fresh_marks(&self, now: u64) -> HashMap<String, Mark> {
        // Only `on_tick` writes; stale never pins equity. Evict here
        // so the map can't grow with dead mints.
        let mut guard = self.last_marks.lock().await;
        guard.retain(|_, (_, ts)| now.saturating_sub(*ts) < CURVE_TICK_STALE_SECS);
        guard
            .iter()
            .map(|(m, (mark, _))| (m.clone(), *mark))
            .collect()
    }

    /// Mints with open paper.
    pub async fn open_position_mints(&self) -> Vec<String> {
        let portfolio = self.portfolio.lock().await;
        portfolio
            .position_ids()
            .iter()
            .filter_map(|id| portfolio.position(*id).map(|p| p.mint.clone()))
            .collect()
    }

    /// Age (secs) of the oldest open paper position; 0 when none open.
    /// Open positions outliving their timeouts by hours read as stuck
    /// exits (drained pools, guard holds), not slow exits.
    pub async fn oldest_open_age_secs(&self, now_secs: u64) -> u64 {
        let portfolio = self.portfolio.lock().await;
        portfolio
            .position_ids()
            .iter()
            .filter_map(|id| portfolio.position(*id))
            .map(|p| now_secs.saturating_sub(p.opened_at))
            .max()
            .unwrap_or(0)
    }

    /// Seconds past its own timeout of the most overdue open position;
    /// 0 when none open or none overdue. Unlike the raw age this
    /// respects runner timeout lifts: an intentionally long hold is
    /// not "stuck" until it outlives its own exit horizon.
    pub async fn oldest_open_overdue_secs(&self, now_secs: u64) -> u64 {
        let portfolio = self.portfolio.lock().await;
        portfolio
            .position_ids()
            .iter()
            .filter_map(|id| portfolio.position(*id))
            .map(|p| {
                now_secs
                    .saturating_sub(p.opened_at)
                    .saturating_sub(p.timeout_seconds)
            })
            .max()
            .unwrap_or(0)
    }

    /// Live mints (suffixed keys report as base).
    pub async fn live_tracked_mints(&self) -> Vec<String>
    where
        L: LiveSink,
    {
        let mut mints: Vec<String> = self.exec.tracked_keys().await;
        mints.extend(self.exec.manual_keys().await);
        mints.extend(self.exec.diverged_keys().await);
        if let Some(trader) = &self.exec.live {
            mints.extend(trader.pending_mints());
        }
        mints = mints
            .into_iter()
            .map(|m| live_base_mint(&m).to_string())
            .collect();
        mints.sort();
        mints.dedup();
        mints
    }

    /// Flip the entry kill switch. Logs the edge only: the daemon polls
    /// the marker file every gate round, so steady state stays quiet.
    pub fn set_entries_paused(&self, paused: bool) {
        let was = self.entries_paused.swap(paused, Ordering::Relaxed);
        if paused && !was {
            warn!(
                "KILL SWITCH: entries paused — no new position opens; open-position management (exits, runner/moonbag scale-outs, `sell <mint>`) keeps running"
            );
        } else if !paused && was {
            info!("kill switch cleared: entries resume");
        }
    }

    /// Flip the data-degrade pause (supervisor-owned; see `data_paused`).
    /// Edge-logged like the kill switch; `reasons` name the unhealthy
    /// sources on the rising edge and stay quiet in steady state.
    pub fn set_data_degraded(&self, degraded: bool, reasons: &[String]) {
        let was = self.data_paused.swap(degraded, Ordering::Relaxed);
        if degraded && !was {
            warn!(
                reasons = ?reasons,
                "DATA DEGRADED: entries paused — no decisions from unhealthy data; open-position management keeps running"
            );
        } else if !degraded && was {
            info!("data sources healthy again: entries resume");
        }
    }

    pub fn data_degraded(&self) -> bool {
        self.data_paused.load(Ordering::Relaxed)
    }
}

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {
    /// Fire a card when enabled. Plain text and structured cards both
    /// flow through (`From<String>` wraps raw text; formatters return
    /// cards directly).
    pub(crate) fn emit(&self, message: impl Into<Card>) {
        if let Some(alerts) = &self.alerts {
            alerts.alert(message.into());
        }
    }

    /// Card context for Discord alerts: live/paper from dispatcher
    /// presence, effective strategy (parsed, never the raw setting).
    pub(crate) fn card_ctx(&self) -> CardCtx<'static> {
        CardCtx {
            mode: if self.exec.live.is_some() {
                "live"
            } else {
                "paper"
            },
            strategy: self.strategy.as_str(),
        }
    }

    /// Shared close tail: recheck pendings, clear custody marks when
    /// no paper position remains, settle deferred PnL once live is
    /// clear, then unwatch. Both the tick path and manual sell run it
    /// after the live leg settles; unsettled paths return before it.
    pub(crate) async fn settle_close_tail(&self, tick: &PriceTick) {
        // Pending-buy rechecks run on the dedicated supervisor task
        // (AUD-013) — never here on the tick path.
        if self
            .portfolio
            .lock()
            .await
            .positions_for_mint(&tick.mint)
            .is_empty()
        {
            self.exec.clear_diverged(&tick.mint).await;
            self.exec.clear_synthetic(&tick.mint).await;
            // Settle deferred PnL only when no live exposure remains.
            // Manual check is base-wide: a suffixed parked dup is still
            // held funds, even though only the exact key resurrects.
            // An unresolved pending buy is the same unsettled exposure
            // (AUD-008): the paper PnL stays deferred until the pending
            // recheck resolves it.
            let pending_buy = self
                .exec
                .live
                .as_ref()
                .is_some_and(|t| t.has_pending_for(&tick.mint));
            let live_held = pending_buy
                || self.exec.is_tracked(&tick.mint).await
                || self.exec.manual_holds_base(&tick.mint).await;
            if !live_held {
                self.flush_deferred_pnl(&tick.mint).await;
                // With no exposure left, no unsettled leg can owe the
                // breaker anything more. Coverage is stamped at buy
                // and kept through the settle so a late-flushed stash
                // stays eligible (it belongs to that same trade).
                self.exec.clear_live_covered(&tick.mint).await;
            }
        }
        // Shared tail under fresh locks.
        self.maybe_unwatch(tick).await;
    }

    /// Per-mint mutex, created on first use.
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

    /// Per-mint tick mutex.
    pub(crate) async fn mint_serial(&self, mint: &str) -> tokio::sync::OwnedMutexGuard<()> {
        Self::lock_for(&self.tick_locks, mint)
            .await
            .lock_owned()
            .await
    }

    /// Drop a mint's locks (terminal paths with no tick).
    pub(crate) async fn evict_mint_locks(&self, mint: &str) {
        self.tick_locks.lock().await.remove(mint);
        self.exec.live_sell_locks.lock().await.remove(mint);
    }

    /// Drained-tick notice: one warn per episode (tick-level noise is
    /// worse than useless), debug while the same drain lasts. A drained
    /// market holds everything — paper and live alike: the bag is real,
    /// the exit is what has no price.
    fn report_drained(&self, mint: &str, liquidity: f64, was_drained: bool, leg: &str) {
        if was_drained {
            debug!(%mint, %leg, "zero-liquidity tick — still deferred (same drained episode)");
        } else {
            warn!(%mint, liquidity, %leg,
                "zero-liquidity tick — deferred until a priced market returns (no market to price a fill)");
        }
    }

    /// Close-path tail: unwatch when empty, hunt migrations. Idempotent.
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
        if self.exec.is_tracked(&tick.mint).await {
            return;
        }
        // A pending buy's late-confirm recheck rides this mint's ticks
        // (close-path tail): unwatching would strand it unresolved.
        if let Some(trader) = &self.exec.live
            && trader.has_pending_for(&tick.mint)
        {
            return;
        }
        // Manual bags pin the watch while the curve is live.
        if !tick.complete && self.exec.manual_holds(&tick.mint).await {
            return;
        }
        if self.gate_state.lock().await.gate.contains(&tick.mint) {
            return;
        }
        // Always evict locks when empty (else leak).
        if self.reserves.unwatch(&tick.mint).await {
            info!(mint = %tick.mint, "unwatched (nothing left)");
        }
        // Fee overrides die with the book. The pending gap note
        // survives while any parked manual bag remains (exact or dup):
        // its eventual manual exit is the settle this note waits for.
        // Post-migration eviction must not orphan it.
        self.portfolio
            .lock()
            .await
            .clear_venue_fee_for_mint(&tick.mint);
        if !self.exec.manual_holds_base(&tick.mint).await {
            self.exec.clear_gap(&tick.mint).await;
        }
        self.last_marks.lock().await.remove(&tick.mint);
        self.tick_locks.lock().await.remove(&tick.mint);
        self.exec.live_sell_locks.lock().await.remove(&tick.mint);
        if tick.complete {
            self.report_migration_once(&tick.mint, &tick.curve).await;
        }
    }

    /// One tick: check exits, close on trigger.
    pub async fn on_tick(&self, tick: &PriceTick) {
        // Stale ticks never drive exits.
        let age = now_secs().saturating_sub(tick.observed_at);
        if age >= CURVE_TICK_STALE_SECS {
            warn!(mint = %tick.mint, age, observed_at = tick.observed_at, "stale curve tick — dropped");
            return;
        }
        // Record the mark first. Never cache migrated curves. The
        // previous mark also tells whether this tick continues a
        // drained episode: exits defer and the notice is per episode,
        // not per tick (a drain can last many rounds).
        // Exact execution state for this tick: reserves feed state- and
        // size-aware fills (paper entries and exits here, live sizing
        // and floors at dispatch).
        let mark = crate::live::paper_mark_from_tick(tick);
        let was_drained = {
            let mut marks = self.last_marks.lock().await;
            let was = marks
                .get(&tick.mint)
                .is_some_and(|(mark, _)| unpriceable_liquidity(mark.liquidity_usd));
            if !tick.complete {
                marks.insert(tick.mint.clone(), (mark, tick.observed_at));
            }
            was
        };
        // Migration closes bypass (stale-spot Manual path below): a
        // complete tick is not a drained market, it is a moved one.
        let unpriceable = !tick.complete && unpriceable_liquidity(tick.liquidity_usd);
        // Per-mint lock.
        let _serial = self.mint_serial(&tick.mint).await;
        // AUD-002/AUD-001: a complete tick for a mint with custody
        // must resolve the migration BEFORE the first sell attempt.
        // `maybe_unwatch` used to be the only hunt caller, but it
        // returns early while a bag is tracked — so the pool leg was
        // registered only after retries had already parked the exit.
        // Hunt here (deduped, None/Err retry on later ticks), then the
        // same tick's close routes through the fresh pool leg. Without
        // a resolvable pool the fail-closed curve refusal stands.
        // AUD-027: only a live leg can route the exit through the pool,
        // so paper-only closes must not spend an exit-lane permit on a
        // 15s RPC hunt. Paper mode still hunts after the close in
        // `maybe_unwatch` (already deduped).
        let has_custody = !self
            .portfolio
            .lock()
            .await
            .positions_for_mint(&tick.mint)
            .is_empty()
            || self.exec.is_tracked(&tick.mint).await;
        if should_hunt_before_close(self.exec.live.is_some(), tick.complete, has_custody) {
            self.report_migration_once(&tick.mint, &tick.curve).await;
        }
        // Clone marks before the book guard (never nest locks).
        let card_marks = self.fresh_marks(now_secs()).await;
        let mut portfolio = self.portfolio.lock().await;
        let ids: Vec<u64> = portfolio.positions_for_mint(&tick.mint);
        // Quantity-weighted close fraction: raw per-position fractions
        // must never be summed (two half-closes of unequal size are
        // not a full close). `trade.quantity` is the sold size.
        let total_qty: u64 = ids
            .iter()
            .filter_map(|id| portfolio.position(*id))
            .map(|p| p.quantity)
            .sum();
        let mut sold_qty = 0u64;
        // Breaker and cards flush after unlock.
        let mut closed_pnls: Vec<f64> = Vec::new();
        let mut closed_cards: Vec<(String, f64, f64, f64, bool)> = Vec::new();
        // Gap notes flush after unlock (never nest the gaps lock inside
        // the portfolio guard).
        let mut gap_notes: Vec<(f64, u64)> = Vec::new();
        let now = now_secs();
        if ids.is_empty() {
            drop(portfolio);
            // No paper: retry live, recheck, maybe unwatch. A tick with
            // no usable liquidity defers the live attempt too: the
            // curve floor cannot price it, so a dispatch would burn a
            // tip on a sell the chain must refuse. (Pool legs price off
            // vault reserves and ride complete ticks — never deferred.)
            if self.exec.is_tracked(&tick.mint).await {
                if unpriceable {
                    self.report_drained(&tick.mint, tick.liquidity_usd, was_drained, "live exit");
                } else {
                    self.live_sell(&tick.mint, &mark, 1.0, SellCover::LiveOnly)
                        .await;
                }
            }
            self.maybe_unwatch(tick).await;
            return;
        } else {
            // No FX leg means no price exits; migration closes bypass.
            if !tick.complete && self.sol_feed.price().is_none() {
                drop(portfolio);
                warn!(mint = %tick.mint, "stale SOL/USD — paper exits deferred (no static fallback)");
                self.maybe_unwatch(tick).await;
                return;
            }
            // A tick with no usable liquidity cannot price an exit. A
            // stop-loss must never book a fill into a drained pool:
            // defer instead (fail closed; the position stays held, heat
            // stays occupied, and the breaker books nothing until a
            // priced exit — no market, no fill). Migration closes
            // bypass (stale-spot Manual path below).
            if unpriceable {
                drop(portfolio);
                self.report_drained(&tick.mint, tick.liquidity_usd, was_drained, "paper exits");
                self.maybe_unwatch(tick).await;
                return;
            }
            if tick.complete {
                warn!(mint = %tick.mint, "curve migrated; closing positions (spot stale)");
            }
            // Paper-only bleed legs (live debits actuals instead).
            if self.exec.live.is_none()
                && let Some(sol_usd) = self.sol_feed.price()
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
            // Decision-input record BEFORE any trigger check: each
            // following exit re-derives from exactly this state line.
            if let Some(j) = self.journal() {
                j.append(&JournalLine::Tick {
                    ts: now,
                    mint: tick.mint.clone(),
                    mark,
                    sol_feed: self.sol_feed.price(),
                    tick_observed_at: tick.observed_at,
                    complete: tick.complete,
                    manual_exits: false,
                });
            }
            for id in ids {
                let reason = if tick.complete {
                    Some(ExitReason::Manual)
                } else {
                    // Realizable basis: triggers agree with settlement.
                    portfolio.check_exit_realizable(id, &mark, tick.observed_at)
                };
                let Some(reason) = reason else { continue };
                // Moonbag TP scales out once.
                let exit_fraction =
                    simulator::market::exit_fraction(reason, portfolio.moonbag_of(id));
                match portfolio.close_partial_position(
                    id,
                    exit_fraction,
                    &mark,
                    reason,
                    tick.observed_at,
                ) {
                    Ok(trade) => {
                        // Defer the breaker feed. Headlines net of
                        // dispatch bleed (pct stays edge-based).
                        let headline = breaker_headline(trade.net_pnl, trade.dispatch_usd);
                        closed_pnls.push(headline);
                        // Execution-gap calibration: paper booked at the
                        // model exit; the live leg settles (or defers) below.
                        gap_notes.push((trade.exit_price, trade.quantity));
                        let is_moonbag_exit = trade.fraction_closed < 1.0;
                        // What the exit actually netted vs the mark:
                        // size impact + venue fee, the liquidation
                        // discount the book realized.
                        let vs_mark_bps = if mark.price > 0.0 {
                            (trade.exit_price / mark.price - 1.0) * 10_000.0
                        } else {
                            f64::NAN
                        };
                        // Book-level mark vs realizable: what the open
                        // positions are worth at the mark and what
                        // liquidating the whole book would cost right
                        // now (size impact + venue fee), next to this
                        // trade's own realized gap.
                        let snapshot = portfolio.snapshot(&card_marks);
                        let equity = snapshot.equity;
                        info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", headline),
                        pct = format!("{:+.2}%", trade.net_pnl_percent),
                        realizable_vs_mark_bps = format!("{:+.0}", vs_mark_bps),
                        book_mark = format!("{:.2}", snapshot.mark_value),
                        book_discount = format!("{:.2}", snapshot.exit_discount_usd()),
                        "paper position closed");
                        closed_cards.push((
                            format!("{:?}", trade.reason),
                            headline,
                            trade.net_pnl_percent,
                            equity,
                            is_moonbag_exit,
                        ));
                        sold_qty += trade.quantity;
                        // Decision record: what closed, why, on which
                        // mark (the tick mark above is the same input).
                        self.journal_exit(
                            &tick.mint,
                            &trade,
                            exit_fraction,
                            &mark,
                            tick.observed_at,
                        );
                    }
                    Err(e) => warn!(mint = %tick.mint, id, error = %e, "paper close failed"),
                }
            }
        }

        drop(portfolio);
        let closed_fraction = if total_qty > 0 {
            (sold_qty as f64 / total_qty as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        for (reason, pnl, pct, equity, moonbag) in closed_cards {
            self.emit(Alerter::format_exit(
                &self.card_ctx(),
                &tick.mint,
                &reason,
                pnl,
                pct,
                equity,
                moonbag,
            ));
        }
        // One note per tick per mint (qty-weighted avg): the live leg
        // dispatches at most once per tick, so N notes would orphan N-1
        // and misattribute the next settle. Same-mint multi-positions
        // only arise via manual opens (the gate refuses seconds).
        if !gap_notes.is_empty() {
            let mut qty_total = 0u64;
            let mut px_qty = 0.0f64;
            for (paper_price, qty) in &gap_notes {
                qty_total = qty_total.saturating_add(*qty);
                px_qty += paper_price * (*qty as f64);
            }
            if qty_total > 0 {
                self.exec
                    .note_gap_paper_close(&tick.mint, px_qty / qty_total as f64, qty_total)
                    .await;
            }
        }
        // Live first, breaker second: unsettled live defers the feed.
        let live_settled = if closed_fraction > 0.0 {
            self.live_sell(&tick.mint, &mark, closed_fraction, SellCover::PaperLeg)
                .await
        } else {
            true
        };
        if live_settled {
            // Synthetic exclusion and uncovered-paper exclusion live in
            // the shared recorder (AUD-002).
            self.record_breaker_pnls(&tick.mint, closed_pnls, now).await;
        } else {
            warn!(mint = %tick.mint, "paper closed but live still holds; breaker PnL deferred until live settles");
            self.exec.stash_deferred_pnl(&tick.mint, closed_pnls).await;
        }
        self.settle_close_tail(tick).await;
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
