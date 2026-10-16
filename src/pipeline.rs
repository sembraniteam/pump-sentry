//! Event processing: launches and tick exits.

use std::collections::HashMap;
use std::sync::Arc;

use alert::{Alerter, CardCtx};
use auditor::{RugCheckClient, RugCheckError, RugCheckReport};
use curve::{CurveError, CurveTicker, PriceTick, SolUsdFeed};
use listener::{
    BundleAnalysis, DevVerdict, EntryGate, MigrationReport, RpcScreener, ScreenError, SwapRecord,
};
use migration::VaultTicker;
use risk_engine::{RiskEngine, StrategyMode};
use simulator::{ExitReason, Mark, PaperPortfolio};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::live::{BuyFill, LiveBuyParams, LiveError, LivePosition, LiveSellParams, LiveTrader, PendingRecheck, PumpSwapSellParams, RaydiumSellParams};
#[cfg(test)]
use crate::live::escalated_tip;
use crate::live_mirror::{LiveExecution, breaker_headline, live_base_mint};
use crate::follow::FollowState;
pub use crate::gate::{DailyBreaker, GateState};
#[cfg(test)]
pub use crate::gate::LaunchRequest;
#[cfg(test)]
pub(crate) use crate::gate::{dev_reputation_score, fee_aware_sl_floor, resolve_entry_with_flag};
#[cfg(test)]
pub(crate) use listener::BundleVerdict;
use crate::settings::{Settings, now_secs};

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
        self.fetch_tick(mint, curve).await
    }

    async fn token_decimals(&self, mint: &str) -> Result<u8, CurveError> {
        self.token_decimals(mint).await
    }

    async fn fetch_tick_with_fee(
        &self,
        mint: &str,
        curve: &str,
    ) -> Result<(PriceTick, Option<f64>), CurveError> {
        self.fetch_tick_with_fee(mint, curve).await
    }

    async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError> {
        self.watch(mint, curve).await
    }

    async fn unwatch(&self, mint: &str) -> bool {
        self.unwatch(mint).await
    }
}

/// Live buy dispatch. Only used with `--live`.
pub trait BuySink {
    /// Buy per params. Returns position plus bundle.
    async fn buy(&self, req: &LiveBuyParams) -> Result<(LivePosition, String, Option<BuyFill>), LiveError>;
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

    /// True when the last sell bundle for `mint` is still
    /// engine-pending: resends must wait, never compete with it.
    async fn prior_sell_pending(&self, mint: &str) -> bool;

    /// Re-poll pending buys once.
    async fn recheck_pending(&self) -> Vec<PendingRecheck>;
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
}

/// Custody event log. Prod writes WAL-first; stubs record calls for
/// assertions. No defaults anywhere: every implementor states each
/// behavior, so a new method forces every stub to decide.
pub trait WalSink {
    /// Record a fill (call AFTER tracking).
    fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str);

    /// Record a manual bag (call BEFORE parking).
    fn record_manual(&self, position: &LivePosition, note: &str);

    /// Record a sell (call BEFORE subtracting).
    fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str);
}

/// Umbrella bound for the full live contract. New code depends on the
/// focused traits; this alias only keeps the six `L: ...` sites below
/// readable. Blanket-implemented, never implemented by hand.
pub trait LiveSink: BuySink + SellSink + PendingStore + CostLedger + WalSink {}

impl<T: BuySink + SellSink + PendingStore + CostLedger + WalSink> LiveSink for T {}

impl BuySink for LiveTrader {
    async fn buy(&self, req: &LiveBuyParams) -> Result<(LivePosition, String, Option<BuyFill>), LiveError> {
        self.buy(req).await
    }
}

impl SellSink for LiveTrader {
    async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError> {
        self.sell(req).await
    }

    async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError> {
        self.sell_raydium(req).await
    }

    async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError> {
        self.sell_pumpswap(req).await
    }
}

impl PendingStore for LiveTrader {
    fn has_pending(&self) -> bool {
        self.has_pending()
    }

    fn pending_mints(&self) -> Vec<String> {
        self.pending_mints()
    }

    async fn prior_sell_pending(&self, mint: &str) -> bool {
        self.prior_sell_pending(mint).await
    }

    async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        self.recheck_pending().await
    }
}

impl CostLedger for LiveTrader {
    async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        self.effective_tip(failed_attempts).await
    }

    fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        self.dispatch_cost_for_tip(tip_lamports)
    }

    fn first_touch_cost_lamports(&self) -> u64 {
        self.first_touch_cost_lamports()
    }
}

impl WalSink for LiveTrader {
    fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str) {
        self.acknowledge_buy(position, bundle_id)
    }

    fn record_manual(&self, position: &LivePosition, note: &str) {
        self.record_manual(position, note)
    }

    fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str) {
        self.record_sell(mint, amount_tokens, bundle_id)
    }
}

/// Alert sink.
pub trait AlertSink {
    fn alert(&self, message: String);
}

impl AlertSink for Alerter {
    fn alert(&self, message: String) {
        self.send(message);
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
    /// Day-loss breaker.
    pub daily: Mutex<DailyBreaker>,
    /// Strategy.
    pub strategy: StrategyMode,
    /// Alerts; `None` silences.
    pub alerts: Option<T>,
}


/// Stale curve ticks are ignored.
pub const CURVE_TICK_STALE_SECS: u64 = 120;





impl<A, R, L, S, T> Pipeline<A, R, L, S, T> {
    /// Build shared state.
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
            last_marks: Mutex::new(HashMap::new()),
        })
    }


    /// Paper cash plus equity.
    pub async fn paper_cash_equity(&self) -> (f64, f64) {
        let marks = self.fresh_marks(now_secs()).await;
        let portfolio = self.portfolio.lock().await;
        let snap = portfolio.snapshot(&marks);
        (snap.cash, snap.equity)
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
}

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {











    /// Fire a card when enabled.
    pub(crate) fn emit(&self, message: String) {
        if let Some(alerts) = &self.alerts {
            alerts.alert(message);
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
        // Recheck before unwatch.
        self.recheck_live_pending().await;
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
            let live_held = self.exec.is_tracked(&tick.mint).await
                || self.exec.manual_holds_base(&tick.mint).await;
            if !live_held {
                self.flush_deferred_pnl(&tick.mint).await;
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
        Self::lock_for(&self.tick_locks, mint).await.lock_owned().await
    }

    /// Drop a mint's locks (terminal paths with no tick).
    pub(crate) async fn evict_mint_locks(&self, mint: &str) {
        self.tick_locks.lock().await.remove(mint);
        self.exec.live_sell_locks.lock().await.remove(mint);
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
        self.portfolio.lock().await.clear_venue_fee_for_mint(&tick.mint);
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
        // Record the mark first. Never cache migrated curves.
        if !tick.complete {
            self.last_marks.lock().await.insert(
                tick.mint.clone(),
                (
                    Mark { price: tick.price_usd, liquidity_usd: tick.liquidity_usd },
                    tick.observed_at,
                ),
            );
        }
        // Per-mint lock.
        let _serial = self.mint_serial(&tick.mint).await;
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
            // No paper: retry live, recheck, maybe unwatch.
            if self.exec.is_tracked(&tick.mint).await {
                self.live_sell(&tick.mint, tick.price_usd, tick.liquidity_usd, 1.0).await;
            }
            self.recheck_live_pending().await;
            self.maybe_unwatch(tick).await;
            return;
        } else {
            // No FX leg means no price exits; migration closes bypass.
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
            for id in ids {
                let reason = if tick.complete {
                    Some(ExitReason::Manual)
                } else {
                    // Realizable basis: triggers agree with settlement.
                    portfolio.check_exit_realizable(
                        id,
                        tick.price_usd,
                        tick.liquidity_usd,
                        tick.observed_at,
                    )
                };
                let Some(reason) = reason else { continue };
                // Moonbag TP scales out once.
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
                        // Defer the breaker feed. Headlines net of
                        // dispatch bleed (pct stays edge-based).
                        let headline = breaker_headline(trade.net_pnl, trade.dispatch_usd);
                        closed_pnls.push(headline);
                        // Execution-gap calibration: paper booked at the
                        // model exit; the live leg settles (or defers) below.
                        gap_notes.push((trade.exit_price, trade.quantity));
                        let is_moonbag_exit = trade.fraction_closed < 1.0;
                        let equity = portfolio.snapshot(&card_marks).equity;
                        info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", headline),
                        pct = format!("{:+.2}%", trade.net_pnl_percent),
                        "paper position closed");
                        closed_cards.push((
                            format!("{:?}", trade.reason),
                            headline,
                            trade.net_pnl_percent,
                            equity,
                            is_moonbag_exit,
                        ));
                        sold_qty += trade.quantity;
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
                &tick.mint, &reason, pnl, pct, equity, moonbag,
            ));
        }
        for (paper_price, qty) in gap_notes {
            self.exec.note_gap_paper_close(&tick.mint, paper_price, qty).await;
        }
        // Live first, breaker second: unsettled live defers the feed.
        let live_settled = if closed_fraction > 0.0 {
            self.live_sell(&tick.mint, tick.price_usd, tick.liquidity_usd, closed_fraction)
                .await
        } else {
            true
        };
        if live_settled {
            // Synthetic never feeds the gate.
            if self.exec.is_synthetic(&tick.mint).await {
                warn!(mint = %tick.mint, n = closed_pnls.len(), "synthetic paper PnL excluded from day-loss breaker (override price, no market)");
            } else {
                for pnl in closed_pnls {
                    self.daily.lock().await.record(pnl, now);
                }
            }
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
