//! Event processing: launches and tick exits.

use std::collections::HashMap;
use std::sync::Arc;

use alert::Alerter;
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

use crate::live::{LiveBuyParams, LiveError, LivePosition, LiveSellParams, LiveTrader, PendingRecheck, PumpSwapSellParams, RaydiumSellParams, escalated_tip};
use crate::live_mirror::{LiveExecution, live_base_mint};
use crate::follow::FollowState;
pub use crate::gate::{DailyBreaker, GateState};
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

/// Live sink. Only used with `--live`.
pub trait LiveSink {
    /// Buy per params. Returns position plus bundle.
    async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError>;

    /// Sell a tracked position. `None` fee is standard.
    async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError>;

    /// Raydium sell at a pool leg. Same contract as `sell`.
    async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError>;

    /// PumpSwap sell at a pool leg. Same contract as `sell`.
    async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError>;

    /// Base Jito tip (live_sell escalates it by attempts).
    fn base_tip_lamports(&self) -> u64;

    /// Congestion-aware tip for an attempt. Default is pure
    /// escalation (stubs have no oracle); live lifts to the floor.
    async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        escalated_tip(self.base_tip_lamports(), failed_attempts)
    }

    /// True when a buy awaits confirmation.
    fn has_pending(&self) -> bool {
        false
    }

    /// Mints awaiting confirmation.
    fn pending_mints(&self) -> Vec<String> {
        Vec::new()
    }

    /// True when the last sell bundle for `mint` is still
    /// engine-pending: resends must wait, never compete with it.
    /// Default proceeds (stubs have no bundle client).
    async fn prior_sell_pending(&self, _mint: &str) -> bool {
        false
    }

    /// Per-dispatch overhead in lamports.
    fn dispatch_cost_lamports(&self) -> u64 {
        0
    }

    /// Overhead at an explicit tip (escalated retries debit actuals).
    fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        tip_lamports
    }

    /// First-buy cost (dispatch plus rent). Debit on buys only.
    fn first_touch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_lamports()
    }

    /// Record a fill (call AFTER tracking).
    fn acknowledge_buy(&self, _position: &LivePosition, _bundle_id: &str) {}

    /// Record a manual bag (call BEFORE parking).
    fn record_manual(&self, _position: &LivePosition, _note: &str) {}

    /// Record a sell (call BEFORE subtracting).
    fn record_sell(&self, _mint: &str, _amount_tokens: u64, _bundle_id: &str) {}

    /// Re-poll pending buys once.
    async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        Vec::new()
    }
}

impl LiveSink for LiveTrader {
    async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError> {
        self.buy(req).await
    }

    async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError> {
        self.sell(req).await
    }

    async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError> {
        self.sell_raydium(req).await
    }

    async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError> {
        self.sell_pumpswap(req).await
    }

    fn base_tip_lamports(&self) -> u64 {
        self.base_tip()
    }

    async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        self.effective_tip(failed_attempts).await
    }

    fn has_pending(&self) -> bool {
        self.has_pending()
    }

    fn pending_mints(&self) -> Vec<String> {
        self.pending_mints()
    }

    async fn prior_sell_pending(&self, mint: &str) -> bool {
        self.prior_sell_pending(mint).await
    }

    fn dispatch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_lamports()
    }

    fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        self.dispatch_cost_for_tip(tip_lamports)
    }

    fn first_touch_cost_lamports(&self) -> u64 {
        self.first_touch_cost_lamports()
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

/// Alert sink.
pub trait AlertSink {
    fn alert(&self, message: String);
}

impl AlertSink for Alerter {
    fn alert(&self, message: String) {
        self.send(message);
    }
}

/// Screening source.
pub trait Screener {
    /// Dev verdict; `Err` skips fail-closed.
    async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError>;
    /// Swaps since `since_sig`, plus watermark and truncated flag.
    async fn observe_curve(
        &self,
        mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>, bool), ScreenError>;
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

    /// Fresh marks (stale removed).
    async fn fresh_marks(&self, now: u64) -> HashMap<String, Mark> {
        // Only `on_tick` writes; stale never pins equity.
        self.last_marks
            .lock()
            .await
            .iter()
            .filter(|(_, (_, ts))| now.saturating_sub(*ts) < CURVE_TICK_STALE_SECS)
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
        // Fee overrides die with the book.
        self.portfolio.lock().await.clear_venue_fee_for_mint(&tick.mint);
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
        let mut closed_fraction = 0.0f64;
        // Breaker and cards flush after unlock.
        let mut closed_pnls: Vec<f64> = Vec::new();
        let mut closed_cards: Vec<(String, f64, f64, f64, bool)> = Vec::new();
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
                        let headline = trade.net_pnl - trade.dispatch_usd;
                        closed_pnls.push(headline);
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
            // Settle deferred only when live is clear.
            let live_held = self.exec.is_tracked(&tick.mint).await
                || self.exec.manual_holds(&tick.mint).await;
            if !live_held {
                self.flush_deferred_pnl(&tick.mint).await;
            }
        }
        // Shared tail under fresh locks.
        self.maybe_unwatch(tick).await;
    }



}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LAMPORTS_PER_SOL, PoolLeg, PoolVenue};
    use migration::PoolTick;
    use simulator::Side;
    use simulator::FillModel;
    use std::sync::Mutex as StdMutex;

    /// Canned audit outcomes.
    enum AuditOutcome {
        Allow(u32),
        Reject,
        Broken,
        /// Transient failures before an allow.
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
                    rugged: false,
                    risks: vec![],
                    token: None,
                }),
                AuditOutcome::Reject => Err(RugCheckError::SecurityRejection("rug".to_string())),
                AuditOutcome::Flaky { fails_left, score } => {
                    let mut fails = fails_left.lock().expect("lock");
                    if *fails > 0 {
                        *fails -= 1;
                        // Transient indexing latency (retryable 429), not a
                        // schema break (terminal). See `is_retryable`.
                        Err(RugCheckError::transient_throttle())
                    } else {
                        Ok(RugCheckReport {
                            mint: mint.to_string(),
                            score: *score,
                            rugged: false,
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

    /// Canned live sink with call tracking.
    #[derive(Clone)]
    struct StubLive {
        inner: Arc<StubLiveInner>,
    }

    struct StubLiveInner {
        buys: StdMutex<Vec<(String, String, String, f64)>>,
        sells: StdMutex<Vec<(String, u64)>>,
        /// Raydium-leg sells `(mint, amm, amount)`.
        raydium_sells: StdMutex<Vec<(String, String, u64)>>,
        /// PumpSwap-leg sells `(mint, pool, amount)`.
        pumpswap_sells: StdMutex<Vec<(String, String, u64)>>,
        /// Tips passed per sell (escalation wiring proof).
        sell_tips: StdMutex<Vec<u64>>,
        /// Base tip the stub reports (mirrors prod default).
        tip_lamports: u64,
        fail_buys: std::sync::atomic::AtomicBool,
        fail_sells: std::sync::atomic::AtomicBool,
        /// Transient sell failures (consume retry budget, unlike `BadOrder`).
        fail_sells_transient: std::sync::atomic::AtomicBool,
        /// Overlap-test hook: sells of `block_mint` wait on `unblock`.
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
                    raydium_sells: StdMutex::new(vec![]),
                    pumpswap_sells: StdMutex::new(vec![]),
                    sell_tips: StdMutex::new(vec![]),
                    tip_lamports: 100_000,
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

        fn raydium_sells(&self) -> Vec<(String, String, u64)> {
            self.inner.raydium_sells.lock().expect("lock").clone()
        }

        fn pumpswap_sells(&self) -> Vec<(String, String, u64)> {
            self.inner.pumpswap_sells.lock().expect("lock").clone()
        }

        fn sell_tips(&self) -> Vec<u64> {
            self.inner.sell_tips.lock().expect("lock").clone()
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

        async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError> {
            let position = req.position;
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
            self.inner
                .sell_tips
                .lock()
                .expect("lock")
                .push(req.tip_lamports);
            Ok("bundle-stub".to_string())
        }

        async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError> {
            let position = req.position;
            let pool = req.pool;
            // Same failure hooks as the curve leg.
            if self
                .inner
                .fail_sells
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::BadOrder("stubbed raydium sell failure".to_string()));
            }
            if self
                .inner
                .fail_sells_transient
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::Unconfirmed(
                    "stubbed transient raydium sell failure".to_string(),
                ));
            }
            self.inner
                .raydium_sells
                .lock()
                .expect("lock")
                .push((position.mint.clone(), pool.amm.clone(), position.amount_tokens));
            self.inner
                .sell_tips
                .lock()
                .expect("lock")
                .push(req.tip_lamports);
            Ok("bundle-stub-raydium".to_string())
        }

        async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError> {
            let position = req.position;
            let pool = req.pool;
            // Same failure hooks as the curve leg.
            if self
                .inner
                .fail_sells
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::BadOrder("stubbed pumpswap sell failure".to_string()));
            }
            if self
                .inner
                .fail_sells_transient
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(LiveError::Unconfirmed(
                    "stubbed transient pumpswap sell failure".to_string(),
                ));
            }
            self.inner
                .pumpswap_sells
                .lock()
                .expect("lock")
                .push((position.mint.clone(), pool.amm.clone(), position.amount_tokens));
            self.inner
                .sell_tips
                .lock()
                .expect("lock")
                .push(req.tip_lamports);
            Ok("bundle-stub-pumpswap".to_string())
        }

        fn base_tip_lamports(&self) -> u64 {
            self.inner.tip_lamports
        }
    }

    /// Canned reserve feed with watch tracking.
    struct StubReserves {
        tick: Option<PriceTick>,
        watched: StdMutex<Vec<(String, String)>>,
        unwatched: StdMutex<Vec<String>>,
        /// When true, `watch` fails (exit-feed outage plumbing).
        fail_watch: bool,
        /// Mint decimals reported (6 = standard).
        decimals: u8,
        /// Venue fee reported (`None` = standard tier).
        fee_bps: Option<f64>,
    }

    impl StubReserves {
        fn with_tick(tick: PriceTick) -> Self {
            Self {
                tick: Some(tick),
                watched: StdMutex::new(vec![]),
                unwatched: StdMutex::new(vec![]),
                fail_watch: false,
                decimals: 6,
                fee_bps: None,
            }
        }

        fn failing() -> Self {
            Self {
                tick: None,
                watched: StdMutex::new(vec![]),
                unwatched: StdMutex::new(vec![]),
                fail_watch: false,
                decimals: 6,
                fee_bps: None,
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

        async fn venue_fee_bps(&self, _mint: &str, _curve: &str) -> Option<f64> {
            self.fee_bps
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

    /// Heuristic-friendly knobs.
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

    /// Canned screener.
    #[derive(Clone)]
    struct StubScreener {
        inner: Arc<StubScreenerInner>,
    }

    struct StubScreenerInner {
        dev: StdMutex<DevOutcome>,
        /// Wallets failing screening even when `dev` passes.
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

        /// Fail one wallet while others follow `dev`.
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
            listener::types::DEFAULT_HTTP_RPC_URL,
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
    fn entry_source_branches() {
        // Live tick wins over the override; errors fall back to it;
        // neither source means no entry.
        let live = tick("M", 2.0);
        assert_eq!(
            resolve_entry_with_flag(Ok(live), Some(9.0), 500.0),
            Some((2.0, 30_000.0, false))
        );
        let missing = || {
            Err(CurveError::AccountMissing("M".to_string()))
                as Result<PriceTick, CurveError>
        };
        assert_eq!(
            resolve_entry_with_flag(missing(), Some(9.0), 500.0),
            Some((9.0, 500.0, true))
        );
        assert_eq!(resolve_entry_with_flag(missing(), None, 0.0), None);
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
    async fn paper_cash_equity_is_realizable_over_fresh_ticks() {
        // Default bonding-curve model (venue fee applies): no tick yet
        // means book value, a fresh tick means exit costs deducted.
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            PaperPortfolio::new(100.0, 0.0),
            StubScreener::passing(),
            None,
        );
        let (cash_empty, equity_empty) = pipe.paper_cash_equity().await;
        assert_eq!((cash_empty, equity_empty), (100.0, 100.0));
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
                now_secs(),
            )
            .expect("manual open");
        // Before any tick: book value (zero unrealized).
        let (cash_book, equity_book) = pipe.paper_cash_equity().await;
        assert!((equity_book - 100.0).abs() < 1e-9, "book {equity_book}");
        assert_eq!(cash_book, 90.0);
        // A fresh flat tick marks equity realizable: the 125 bps venue
        // fee on the exit leg deducts, so equity drops below book.
        pipe.on_tick(&tick("MINT", 2.0)).await;
        assert_eq!(pipe.portfolio.lock().await.positions_for_mint("MINT").len(), 1);
        let snap = pipe
            .portfolio
            .lock()
            .await
            .snapshot(&HashMap::from([(
                "MINT".to_string(),
                simulator::Mark { price: 2.0, liquidity_usd: 30_000.0 },
            )]));
        let (cash, equity) = pipe.paper_cash_equity().await;
        assert_eq!(cash, snap.cash);
        assert!((equity - snap.equity).abs() < 1e-9);
        assert!(equity < 100.0, "exit costs must deduct: {equity}");
        assert!(snap.unrealized_pnl < 0.0);
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
    async fn confirm_registers_custom_venue_fee() {
        // FeeConfig read at confirm wires the paper ledger: a 345 bps
        // custom tier lands per mint, while an unreadable tier (None)
        // keeps the standard 125 bps default.
        for (fee, want) in [(Some(345.0), 345.0), (None, 125.0)] {
            let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
            let mut reserves = StubReserves::with_tick(tick("MINT", 2.0));
            reserves.fee_bps = fee;
            let pipe = pipeline_screened(
                Settings::from_map(&HashMap::new()),
                AuditOutcome::Allow(100),
                reserves,
                PaperPortfolio::new(100.0, 0.0),
                screener,
                None,
            );
            pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
                .await;
            pipe.poll_gates().await;
            assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
            assert!(
                (pipe.portfolio.lock().await.venue_fee_for("MINT") - want).abs() < 1e-9,
                "fee={fee:?}"
            );
            // Fee-aware stop: the moderate −8 stop survives the standard
            // tier but widens past round-trip costs on 345 bps (−10.9).
            let id = pipe.portfolio.lock().await.position_ids()[0];
            let sl = pipe.portfolio.lock().await.position(id).expect("pos").sl_percent;
            let want_sl = fee_aware_sl_floor(-8.0, want);
            assert!((sl - want_sl).abs() < 1e-9, "fee={fee:?} sl={sl}");
        }
    }

    #[tokio::test]
    async fn follow_pool_stores_leg_and_sweep_keeps_held_bags() {
        // Full leg lifecycle: hunt stores the leg over a tick-seeded
        // entry; the idle sweep keeps it while live custody holds and
        // releases it once the book clears.
        use listener::MigrationDestination;
        let live = StubLive::working();
        let screener = StubScreener::passing();
        *screener.inner.migration.lock().expect("lock") = Some(MigrationReport {
            signature: "migsig".to_string(),
            destination: MigrationDestination::RaydiumV4,
            amm_id: Some(fixture_wallet(20)),
            base_mint: Some("WSOL".to_string()),
            quote_mint: Some("MINT".to_string()),
            base_vault: Some(fixture_wallet(21)),
            quote_vault: Some(fixture_wallet(22)),
        });
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            screener,
            Some(live.clone()),
        );
        // Valid base58 throughout: the pool watch parses every address.
        let mint = fixture_wallet(30);
        let tick = PoolTick {
            mint: mint.clone(),
            amm_id: "AMM".to_string(),
            price_usd: 1.0,
            liquidity_usd: 50_000.0,
            observed_at: now_secs(),
        };
        // Tick seeds the entry with no leg...
        pipe.on_pool_tick(&tick).await;
        assert!(pipe.followed.lock().await[&mint].pool.is_none());
        // ...the hunt overwrites it (never `or_insert` past it).
        pipe.report_migration_once(&mint, "CURVE").await;
        {
            let followed = pipe.followed.lock().await;
            let leg = followed[&mint].pool.clone().expect("leg stored");
            assert_eq!(leg.amm, fixture_wallet(20));
        }
        // Age the entry past TTL with a live bag held: sweep keeps it.
        pipe.exec.live_positions.lock().await.insert(
            mint.clone(),
            LivePosition {
                mint: mint.clone(),
                curve: "CURVE".to_string(),
                assoc_curve: "ASSOC".to_string(),
                amount_tokens: 1_000,
                creator: None,
            },
        );
        {
            let mut followed = pipe.followed.lock().await;
            followed.get_mut(&mint).expect("entry").last_seen_secs = 0;
        }
        let other = PoolTick { mint: "OTHER".to_string(), ..tick.clone() };
        pipe.on_pool_tick(&other).await;
        assert!(pipe.followed.lock().await.contains_key(&mint));
        // Book clears: next sweep releases the leg.
        pipe.exec.live_positions.lock().await.remove(&mint);
        pipe.on_pool_tick(&other).await;
        assert!(!pipe.followed.lock().await.contains_key(&mint));
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
        // Retryable transport failure (429) watchlists, not rejects:
        // watchlist with no score, open nothing yet. Schema breaks
        // (Broken body) are terminal instead — see
        // `audit_schema_break_drops_immediately`.
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Flaky { fails_left: std::sync::Mutex::new(9999), score: 100 },
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
    async fn audit_schema_break_drops_immediately() {
        // Schema break is permanent: drop at once, never watchlist-retry.
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
        assert_eq!(pipe.pending_count().await, 0);
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
        // No pool leg: the Raydium leg stays silent.
        assert!(live.raydium_sells().is_empty());
    }

    #[tokio::test]
    async fn live_sell_routes_raydium_when_pool_known() {
        // Same close, but a validated pool leg is stored: the exit must
        // take the Raydium leg (curve leg untouched) with the pool's amm.
        use crate::follow::FollowState;
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
        pipe.followed.lock().await.insert(
            "MINT".to_string(),
            FollowState {
                entry_price: 2.0,
                alerted_spike: false,
                last_seen_secs: now_secs(),
                pool: Some(PoolLeg {
                    mint: "MINT".to_string(),
                    amm: "AMM".to_string(),
                    base_vault: "BASE".to_string(),
                    quote_vault: "QUOTE".to_string(),
                    venue: PoolVenue::RaydiumV4,
                }),
            },
        );
        pipe.on_tick(&tick("MINT", 3.0)).await;

        assert!(live.sells().is_empty());
        assert_eq!(
            live.raydium_sells(),
            vec![("MINT".to_string(), "AMM".to_string(), 1_000)]
        );
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn live_sell_routes_pumpswap_when_pool_known() {
        // Same close, but the leg is a PumpSwap pool: the exit must
        // take the PumpSwap leg (curve and Raydium legs untouched).
        use crate::follow::FollowState;
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
        pipe.followed.lock().await.insert(
            "MINT".to_string(),
            FollowState {
                entry_price: 2.0,
                alerted_spike: false,
                last_seen_secs: now_secs(),
                pool: Some(PoolLeg {
                    mint: "MINT".to_string(),
                    amm: "POOL".to_string(),
                    base_vault: "WSOLVAULT".to_string(),
                    quote_vault: "TOKENVAULT".to_string(),
                    venue: PoolVenue::PumpSwap,
                }),
            },
        );
        pipe.on_tick(&tick("MINT", 3.0)).await;

        assert!(live.sells().is_empty());
        assert!(live.raydium_sells().is_empty());
        assert_eq!(
            live.pumpswap_sells(),
            vec![("MINT".to_string(), "POOL".to_string(), 1_000)]
        );
        assert!(pipe.exec.live_positions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn paper_only_stop_fires_on_realizable_and_nets_bleed() {
        // No live leg: exits price the fee-aware fill (M1) and the
        // ledger nets dispatch bleed (M3). Default 125bps/side.
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline_screened(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            PaperPortfolio::new(100.0, 0.0),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet(), None, 5000)
            .await;
        pipe.poll_gates().await;
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        // -7.7% gross: a gross-mark stop holds here, but the fill nets
        // past -8% (125bps exit cost), so the realizable stop fires.
        pipe.on_tick(&tick("MINT", 1.87)).await;
        let portfolio = pipe.portfolio.lock().await;
        assert!(portfolio.position_ids().is_empty());
        assert_eq!(portfolio.history().len(), 1);
        let trade = portfolio.history()[0].clone();
        assert_eq!(trade.reason, ExitReason::StopLoss);
        let gross_move = (1.87 - trade.entry_price) / trade.entry_price * 100.0;
        assert!(gross_move > -8.0, "gross must hold: {gross_move}");
        // Paper bleed at sol $150, tip 100k: open (100k+5k+rent),
        // close (100k+5k), modeled on both legs.
        let open_leg = 2_144_280.0 / LAMPORTS_PER_SOL * 150.0;
        let close_leg = 105_000.0 / LAMPORTS_PER_SOL * 150.0;
        assert!((trade.dispatch_usd - close_leg).abs() < 1e-9);
        assert!((portfolio.total_dispatch_usd() - (open_leg + close_leg)).abs() < 1e-6);
        // Breaker and cards see edge net of bleed.
        let day_net = pipe.daily.lock().await.day_net;
        assert!((day_net - (trade.net_pnl - trade.dispatch_usd)).abs() < 1e-9);
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
    async fn live_sell_escalates_tip_with_attempts() {
        // Two prior failures: the next dispatch carries 3x base tip
        // (bounded congestion response), recorded per sell.
        let live = StubLive::working();
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
        {
            let mut retry = pipe.exec.live_retry.lock().await;
            retry.insert("MINT".to_string(), (2u32, 0u64));
        }
        pipe.on_tick(&tick("MINT", 3.0)).await;
        assert_eq!(live.sell_tips(), vec![300_000]);
        assert_eq!(live.sells(), vec![("MINT".to_string(), 1_000)]);
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
