//! Event processing: launch handling and tick-driven exits.
//!
//! [`Pipeline`] is generic over [`Auditor`] and [`ReserveFeed`] so unit
//! tests inject stubs instead of network clients. Pure helpers
//! ([`resolve_entry`]) stay free functions for direct testing.

use std::collections::HashMap;
use std::sync::Arc;

use auditor::{RugCheckClient, RugCheckError, RugCheckReport};
use curve::{CurveError, CurveTicker, PriceTick};
use listener::{DevVerdict, EntryGate, GateOutcome, RpcScreener, ScreenError, SwapRecord};
use risk_engine::{Action, RiskEngine, TokenMetrics, evaluate_heuristic_fallback};
use simulator::{ExitReason, PaperPortfolio, Side, check_exit};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::live::{LiveError, LivePosition, LiveTrader};
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
    /// Buy `notional_usd` at `price_usd` (both USD) via Jito bundle.
    async fn buy(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        notional_usd: f64,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<LivePosition, LiveError>;

    /// Sell a tracked position at `price_usd`; returns the bundle UUID.
    async fn sell(
        &self,
        position: &LivePosition,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<String, LiveError>;
}

impl LiveSink for LiveTrader {
    async fn buy(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        notional_usd: f64,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<LivePosition, LiveError> {
        self.buy(mint, curve, assoc_curve, notional_usd, price_usd, sol_usd)
            .await
    }

    async fn sell(
        &self,
        position: &LivePosition,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<String, LiveError> {
        self.sell(position, price_usd, sol_usd).await
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
}

impl Screener for RpcScreener {
    async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
        let dev = listener::parse_address(dev)?;
        self.dev_verdict(&dev, now_secs())
            .await
            .map_err(ScreenError::Rpc)
    }

    async fn observe_curve(
        &self,
        _mint: &str,
        curve: &str,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>), ScreenError> {
        let curve = listener::parse_address(curve)?;
        self.curve_swaps(&curve, since_sig)
            .await
            .map_err(ScreenError::Rpc)
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

/// Shared pipeline state for the event, gate, and ticker tasks.
pub struct Pipeline<A = RugCheckClient, R = CurveTicker, L = LiveTrader, S = RpcScreener> {
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
    /// Live dispatcher; `None` keeps the run paper-only.
    pub live: Option<L>,
    /// Tracked live fills by mint.
    pub live_positions: Mutex<HashMap<String, LivePosition>>,
    /// On-chain screening source (dev history + curve reads).
    pub screener: S,
    /// Watchlist state plus per-mint signal context (single lock).
    pub gate_state: Mutex<GateState>,
}

/// Watchlist entry context alongside [`EntryGate`] decisions.
#[derive(Debug, Clone)]
pub struct PendingSignal {
    /// Bonding-curve address (base58) for swap observation.
    pub curve: String,
    /// Curve ATA address (base58) for live exits.
    pub assoc_curve: String,
    /// RugCheck score captured at signal time.
    pub score: u32,
    /// Signature watermark for incremental curve reads.
    pub last_sig: Option<String>,
}

/// Entry gate plus signal context under one lock (no lock ordering).
pub struct GateState {
    /// Buyer-quorum state machine.
    pub gate: EntryGate,
    /// Per-mint context for pending entries.
    pub meta: HashMap<String, PendingSignal>,
}

impl<A, R, L, S> Pipeline<A, R, L, S> {
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
    ) -> Arc<Self> {
        let gate = EntryGate::new(settings.gate_window_secs, settings.gate_min_buyers);
        Arc::new(Self {
            settings,
            auditor,
            engine,
            reserves,
            portfolio: Mutex::new(portfolio),
            live,
            live_positions: Mutex::new(HashMap::new()),
            screener,
            gate_state: Mutex::new(GateState {
                gate,
                meta: HashMap::new(),
            }),
        })
    }

    /// Watchlisted mint count (for metrics/tests).
    pub async fn pending_count(&self) -> usize {
        self.gate_state.lock().await.gate.pending_count()
    }
}

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener> Pipeline<A, R, L, S> {
    /// Audit → dev screen → watchlist. Never opens here: real demand must
    /// show up in [`Pipeline::poll_gates`] first. Skips (with a log) on
    /// any fail-closed condition.
    pub async fn on_launch(&self, mint: &str, curve: &str, assoc_curve: &str, dev: &str) {
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
                last_sig: None,
            },
        );
        info!(%mint, score = report.score, "watchlisted; awaiting buyer quorum");
    }

    /// Drive every watchlisted mint one observation round: confirm opens
    /// (with live-measured velocity), aborts drop, timeouts sweep.
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
        for (mint, mut signal) in mints {
            let (swaps, newest) = match self
                .screener
                .observe_curve(&mint, &signal.curve, signal.last_sig.as_deref())
                .await
            {
                Ok(observed) => observed,
                Err(e) => {
                    warn!(%mint, error = %e, "curve observation failed; keeping watch");
                    continue;
                }
            };
            if let Some(sig) = newest {
                signal.last_sig = Some(sig.clone());
                self.gate_state
                    .lock()
                    .await
                    .meta
                    .insert(mint.clone(), signal.clone());
            }
            let outcome = self
                .gate_state
                .lock()
                .await
                .gate
                .observe(&mint, &swaps, now_secs());
            match outcome {
                GateOutcome::Confirm { buyers } => {
                    self.gate_state.lock().await.meta.remove(&mint);
                    self.confirm_open(&mint, &signal, buyers).await;
                }
                GateOutcome::Abort { reason } => {
                    self.gate_state.lock().await.meta.remove(&mint);
                    info!(%mint, %reason, "gate abort");
                }
                GateOutcome::Wait => {}
            }
        }
        let now = now_secs();
        for (mint, outcome) in self.gate_state.lock().await.gate.sweep_timeouts(now) {
            self.gate_state.lock().await.meta.remove(&mint);
            if let GateOutcome::Abort { reason } = outcome {
                info!(%mint, %reason, "gate abort");
            }
        }
    }

    /// Open a confirmed signal: fresh pricing, risk with measured buyer
    /// velocity and screened developer trust, then paper + live dispatch.
    async fn confirm_open(&self, mint: &str, signal: &PendingSignal, buyers: u32) {
        let tick_result = self.reserves.fetch_tick(mint, &signal.curve).await;
        if tick_result.is_err() {
            warn!(%mint, "reserve read failed; trying QUOTE_USD override");
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
        self.open_tracked(
            mint,
            &signal.curve,
            &signal.assoc_curve,
            entry_quote,
            liquidity_usd,
            &profile,
        )
        .await;
    }

    /// Open a paper position at a confirmed price, watch for exits, and
    /// mirror to live dispatch when enabled.
    async fn open_tracked(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        entry_quote: f64,
        liquidity_usd: f64,
        profile: &risk_engine::RiskProfile,
    ) {
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
                self.live_buy(mint, curve, assoc_curve, entry_quote).await;
            }
            Err(e) => warn!(%mint, error = %e, "paper open failed"),
        }
    }

    /// Mirror a paper open with a real Jito-routed buy (live mode only).
    /// Paper state is authoritative; live failures only log loudly.
    async fn live_buy(&self, mint: &str, curve: &str, assoc_curve: &str, entry_quote: f64) {
        let Some(trader) = &self.live else { return };
        match trader
            .buy(
                mint,
                curve,
                assoc_curve,
                self.settings.notional_usd,
                entry_quote,
                self.settings.sol_usd,
            )
            .await
        {
            Ok(position) => {
                self.live_positions
                    .lock()
                    .await
                    .insert(mint.to_string(), position);
                warn!(%mint, "LIVE buy dispatched via Jito (real funds)");
            }
            Err(e) => warn!(%mint, error = %e, "LIVE buy FAILED; paper position still open"),
        }
    }

    /// Mirror a paper close with a real Jito-routed sell (live mode only).
    /// No-ops when no live fill is tracked for the mint.
    async fn live_sell(&self, mint: &str, price_usd: f64) {
        let Some(trader) = &self.live else { return };
        let position = self.live_positions.lock().await.remove(mint);
        let Some(position) = position else { return };
        match trader
            .sell(&position, price_usd, self.settings.sol_usd)
            .await
        {
            Ok(bundle) => warn!(%mint, %bundle, "LIVE sell dispatched via Jito (real funds)"),
            Err(e) => warn!(%mint, error = %e, "LIVE sell FAILED; manual exit required"),
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
        let mut closed_any = false;
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
                    closed_any = true;
                }
                Err(e) => warn!(mint = %tick.mint, id, error = %e, "paper close failed"),
            }
        }

        let still_open = portfolio
            .position_ids()
            .into_iter()
            .any(|id| portfolio.position(id).is_some_and(|p| p.mint == tick.mint));
        drop(portfolio);
        if closed_any {
            self.live_sell(&tick.mint, tick.price_usd).await;
        }
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

    /// Canned live sink with call tracking (shared handle for assertions).
    #[derive(Clone)]
    struct StubLive {
        inner: Arc<StubLiveInner>,
    }

    struct StubLiveInner {
        buys: StdMutex<Vec<(String, String, String, f64)>>,
        sells: StdMutex<Vec<String>>,
        fail_buys: std::sync::atomic::AtomicBool,
    }

    impl StubLive {
        fn working() -> Self {
            Self {
                inner: Arc::new(StubLiveInner {
                    buys: StdMutex::new(vec![]),
                    sells: StdMutex::new(vec![]),
                    fail_buys: std::sync::atomic::AtomicBool::new(false),
                }),
            }
        }

        fn buys(&self) -> Vec<(String, String, String, f64)> {
            self.inner.buys.lock().expect("lock").clone()
        }

        fn sells(&self) -> Vec<String> {
            self.inner.sells.lock().expect("lock").clone()
        }
    }

    impl LiveSink for StubLive {
        async fn buy(
            &self,
            mint: &str,
            curve: &str,
            assoc_curve: &str,
            notional_usd: f64,
            _price_usd: f64,
            _sol_usd: f64,
        ) -> Result<LivePosition, LiveError> {
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
            Ok(LivePosition {
                mint: mint.to_string(),
                curve: curve.to_string(),
                assoc_curve: assoc_curve.to_string(),
                amount_tokens: 1_000,
            })
        }

        async fn sell(
            &self,
            position: &LivePosition,
            _price_usd: f64,
            _sol_usd: f64,
        ) -> Result<String, LiveError> {
            self.inner
                .sells
                .lock()
                .expect("lock")
                .push(position.mint.clone());
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
    use listener::{SwapSide, fixture_wallet};

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
    }

    impl StubScreener {
        fn passing() -> Self {
            Self {
                inner: Arc::new(StubScreenerInner {
                    dev: StdMutex::new(DevOutcome::Pass),
                    swaps: StdMutex::new(HashMap::new()),
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
    }

    fn pipeline_screened(
        settings: Settings,
        outcome: AuditOutcome,
        reserves: StubReserves,
        portfolio: PaperPortfolio,
        screener: StubScreener,
        live: Option<StubLive>,
    ) -> Arc<Pipeline<StubAuditor, StubReserves, StubLive, StubScreener>> {
        Pipeline::shared(
            settings,
            StubAuditor { outcome },
            None,
            reserves,
            portfolio,
            live,
            screener,
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;

        // Signal accepted, but no position until buyer quorum confirms.
        assert_eq!(pipe.pending_count().await, 1);
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;

        // 3 buyers → scalp branch with widened TP/SL (40/-10), not 18/-4.
        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.position_ids().len(), 1);
        let id = portfolio.position_ids()[0];
        let position = portfolio.position(id).expect("position");
        assert_eq!(position.entry_price, 2.0);
        assert_eq!((position.tp_percent, position.sl_percent), (40.0, -10.0));
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;

        let portfolio = pipe.portfolio.lock().await;
        let id = portfolio.position_ids()[0];
        let position = portfolio.position(id).expect("position");
        assert_eq!((position.tp_percent, position.sl_percent), (75.0, -8.0));
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn gate_aborts_on_dev_dump() {
        let mut swaps = buyer_swaps("MINT", &[11]);
        swaps.push(SwapRecord::new("dump", &dev_wallet(), SwapSide::Sell).expect("fixture"));
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        assert_eq!(pipe.pending_count().await, 1);
        pipe.poll_gates().await;

        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
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
            pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
            pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;
        assert!(pipe.portfolio.lock().await.position_ids().is_empty());
        assert_eq!(pipe.pending_count().await, 0);
    }

    #[tokio::test]
    async fn confirm_uses_quote_override_when_tick_fails() {
        let settings = settings(&[("QUOTE_USD", "3.0"), ("LIQUIDITY_USD", "5000")]);
        let screener = StubScreener::with_swaps("MINT", buyer_swaps("MINT", &[11, 12, 13, 14, 15]));
        let pipe = pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::failing(),
            funded(),
            screener,
            None,
        );
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;

        // +50% over the 2.0 entry clears the 40% TP.
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
        pipe.on_launch("A", "CURVE_A", "ASSOC_A", &dev_wallet())
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
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
        let tracked = pipe.live_positions.lock().await;
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;

        // Paper record survives; nothing tracked for exits.
        assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
        assert!(pipe.live_positions.lock().await.is_empty());
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.0)).await;

        assert_eq!(live.sells(), vec!["MINT".to_string()]);
        assert!(pipe.live_positions.lock().await.is_empty());
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
        pipe.on_launch("MINT", "CURVE", "ASSOC", &dev_wallet())
            .await;
        pipe.poll_gates().await;
        pipe.on_tick(&tick("MINT", 3.6)).await;
        assert!(pipe.live_positions.lock().await.is_empty());
    }
}
