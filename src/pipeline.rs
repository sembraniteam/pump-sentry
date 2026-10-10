//! Event processing: launch handling and tick-driven exits.
//!
//! [`Pipeline`] is generic over [`Auditor`] and [`ReserveFeed`] so unit
//! tests inject stubs instead of network clients. Pure helpers
//! ([`resolve_entry`]) stay free functions for direct testing.

use std::collections::HashMap;
use std::sync::Arc;

use auditor::{RugCheckClient, RugCheckError, RugCheckReport};
use curve::{CurveError, CurveTicker, PriceTick};
use risk_engine::{Action, RiskEngine, TokenMetrics, evaluate_heuristic_fallback};
use simulator::{ExitReason, PaperPortfolio, Side, check_exit};
use tokio::sync::Mutex;
use tracing::{info, warn};

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

/// Shared pipeline state for the event and ticker tasks.
pub struct Pipeline<A = RugCheckClient, R = CurveTicker> {
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
}

impl<A, R> Pipeline<A, R> {
    /// Assemble shared state behind [`Arc`] for task sharing.
    pub fn shared(
        settings: Settings,
        auditor: A,
        engine: Option<RiskEngine>,
        reserves: R,
        portfolio: PaperPortfolio,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            auditor,
            engine,
            reserves,
            portfolio: Mutex::new(portfolio),
        })
    }
}

impl<A: Auditor, R: ReserveFeed> Pipeline<A, R> {
    /// Audit → reserve read → risk → open. Skips (with a log) on any
    /// fail-closed condition.
    pub async fn on_launch(&self, mint: &str, curve: &str) {
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

        // 2. Live reserves — entry price + liquidity from the curve.
        let tick_result = self.reserves.fetch_tick(mint, curve).await;
        if tick_result.is_err() {
            warn!(%mint, "reserve read failed; trying QUOTE_USD override");
        }
        let Some((entry_quote, liquidity_usd)) = resolve_entry(
            tick_result,
            self.settings.quote_usd,
            self.settings.liquidity_usd,
        ) else {
            warn!(%mint, "no live price and QUOTE_USD unset; skipping");
            return;
        };

        // 3. Risk — model with heuristic fallback.
        let metrics = TokenMetrics {
            liquidity_usd,
            buy_velocity_5s: self.settings.buy_velocity_5s,
            dev_reputation: self.settings.dev_reputation,
            rugcheck_score: report.score,
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
            info!(%mint, score = report.score, "risk PASS");
            return;
        }

        // 4. Simulate — open at the live curve price, then watch for exits.
        let mut portfolio = self.portfolio.lock().await;
        match portfolio.open_position(
            mint,
            Side::Long,
            entry_quote,
            self.settings.notional_usd,
            liquidity_usd,
            profile.tp_percent,
            profile.sl_percent,
            profile.trailing_stop,
            self.settings.trail_percent,
            profile.timeout_seconds,
            now_secs(),
        ) {
            Ok(id) => {
                let snap = portfolio.snapshot(&HashMap::new());
                info!(%mint, id, entry = format!("{entry_quote:.10}"),
                    tp = profile.tp_percent, sl = profile.sl_percent,
                    equity = format!("{:.2}", snap.equity), "paper position opened");
                drop(portfolio);
                if let Err(e) = self.reserves.watch(mint, curve).await {
                    warn!(%mint, error = %e, "ticker watch failed; exits need manual review");
                }
            }
            Err(e) => warn!(%mint, error = %e, "paper open failed"),
        }
    }

    /// One price tick: evaluate exits for every open position on the mint,
    /// close on trigger, unwatch when nothing remains.
    pub async fn on_tick(&self, tick: &PriceTick) {
        let mut portfolio = self.portfolio.lock().await;
        let ids: Vec<u64> = portfolio
            .position_ids()
            .into_iter()
            .filter(|id| portfolio.position(*id).is_some_and(|p| p.mint == tick.mint))
            .collect();
        if ids.is_empty() {
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
            match portfolio.close_position(
                id,
                tick.price_usd,
                tick.liquidity_usd,
                reason,
                tick.observed_at,
            ) {
                Ok(trade) => {
                    info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", trade.net_pnl),
                        pct = format!("{:+.2}%", trade.net_pnl_percent),
                        "paper position closed");
                }
                Err(e) => warn!(mint = %tick.mint, id, error = %e, "paper close failed"),
            }
        }

        let still_open = portfolio
            .position_ids()
            .into_iter()
            .any(|id| portfolio.position(id).is_some_and(|p| p.mint == tick.mint));
        drop(portfolio);
        if !still_open && self.reserves.unwatch(&tick.mint).await {
            info!(mint = %tick.mint, "unwatched (no open positions)");
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
    fn bullish_settings() -> Settings {
        settings(&[("DEV_REPUTATION", "0.9"), ("BUY_VELOCITY_5S", "40")])
    }

    fn pipeline(
        settings: Settings,
        outcome: AuditOutcome,
        reserves: StubReserves,
        portfolio: PaperPortfolio,
    ) -> Arc<Pipeline<StubAuditor, StubReserves>> {
        Pipeline::shared(settings, StubAuditor { outcome }, None, reserves, portfolio)
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
    async fn launch_opens_and_watches_on_buy() {
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.position_ids().len(), 1);
        let id = portfolio.position_ids()[0];
        let position = portfolio.position(id).expect("position");
        assert_eq!(position.entry_price, 2.0);
        assert_eq!(position.tp_percent, 75.0);
        drop(portfolio);

        // Watch registered for exits.
        // (Borrow ends before assertion to satisfy the async lock.)
        assert_eq!(pipe.reserves.watched.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn launch_skips_on_audit_reject_and_error() {
        for outcome in [AuditOutcome::Reject, AuditOutcome::Broken] {
            let reserves = StubReserves::with_tick(tick("MINT", 2.0));
            let pipe = pipeline(bullish_settings(), outcome, reserves, funded());
            pipe.on_launch("MINT", "CURVE").await;
            assert!(pipe.portfolio.lock().await.position_ids().is_empty());
            assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
        }
    }

    #[tokio::test]
    async fn launch_skips_without_any_price() {
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    }

    #[tokio::test]
    async fn launch_skips_on_pass() {
        // Default settings: dev_reputation 0.0 → heuristic PASS.
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline(
            Settings::from_map(&HashMap::new()),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    }

    #[tokio::test]
    async fn launch_uses_quote_override_when_tick_fails() {
        let settings = settings(&[
            ("DEV_REPUTATION", "0.9"),
            ("BUY_VELOCITY_5S", "40"),
            ("QUOTE_USD", "3.0"),
            ("LIQUIDITY_USD", "5000"),
        ]);
        let pipe = pipeline(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;
        let portfolio = pipe.portfolio.lock().await;
        let id = portfolio.position_ids()[0];
        assert_eq!(portfolio.position(id).expect("position").entry_price, 3.0);
    }

    #[tokio::test]
    async fn tick_closes_on_take_profit_and_unwatches() {
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;

        // +80% over the 2.0 entry clears the 75% TP.
        pipe.on_tick(&tick("MINT", 3.6)).await;

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
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;

        pipe.on_tick(&tick("MINT", 2.1)).await;

        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(pipe.reserves.unwatched.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn tick_closes_other_mints_only() {
        let reserves = StubReserves::with_tick(tick("A", 2.0));
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
        );
        pipe.on_launch("A", "CURVE_A").await;
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
                    1000,
                )
                .expect("open B");
        }

        pipe.on_tick(&tick("A", 3.6)).await;

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
                1000,
            )
            .expect("open");
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            portfolio,
        );

        let mut timeout_tick = tick("MINT", 2.0);
        timeout_tick.observed_at = 1061;
        pipe.on_tick(&timeout_tick).await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history()[0].reason, ExitReason::Timeout);
    }

    #[tokio::test]
    async fn tick_migration_forces_manual_close() {
        let reserves = StubReserves::with_tick(tick("MINT", 2.0));
        let pipe = pipeline(
            bullish_settings(),
            AuditOutcome::Allow(100),
            reserves,
            funded(),
        );
        pipe.on_launch("MINT", "CURVE").await;

        let mut migrated = tick("MINT", 2.0);
        migrated.complete = true;
        pipe.on_tick(&migrated).await;

        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history()[0].reason, ExitReason::Manual);
        assert!(portfolio.position_ids().is_empty());
    }
}
