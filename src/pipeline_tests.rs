use crate::live::{
    BuyFill, LAMPORTS_PER_SOL, PendingRecheck, PoolLeg, PoolVenue, RecheckOutcome, VENUE_FEE_BPS,
};
use crate::live::{
    TEST_ASSOC as ASSOC, TEST_CURVE as CURVE, TEST_MINT as MINT, TEST_MINT_A as MINT_A,
    TEST_MINT_B as MINT_B,
};
use crate::pipeline::*;
use executor::DeadReason;
use executor::{ExecutorError, JitoError};
use migration::PoolTick;
use simulator::FillModel;
use simulator::Side;
use std::sync::Mutex as StdMutex;

/// Parse a test address (all custody-type test fixtures use the shared
/// `TEST_*` mints, valid by construction).
fn pk(s: &str) -> solana_pubkey::Pubkey {
    executor::parse_pubkey(s).expect("test address")
}

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
    /// One knob for both real cost legs (first-touch and the
    /// landed-and-errored burn). 0 = books nothing, so the paper-only
    /// accounting assertions keep holding.
    dispatch_lamports: StdMutex<u64>,
    fail_buys: std::sync::atomic::AtomicBool,
    /// Simulated dead-buy verdict (dispatch proven dead before landing).
    dead_buys: StdMutex<Option<DeadReason>>,
    fail_sells: std::sync::atomic::AtomicBool,
    /// Transient sell failures (consume retry budget, unlike `BadOrder`).
    fail_sells_transient: std::sync::atomic::AtomicBool,
    /// Unknown sell dispatch (timeout with no receipt): unknown custody,
    /// parks for manual review with no resend.
    fail_sells_unknown: std::sync::atomic::AtomicBool,
    /// Deterministic Jito rejection (HTTP 429 rate limit): retryable,
    /// never unknown custody (AUD-002).
    fail_sells_rate_limited: std::sync::atomic::AtomicBool,
    /// Overlap-test hook: sells of `block_mint` wait on `unblock`.
    block_mint: StdMutex<Option<String>>,
    started: StdMutex<Vec<String>>,
    unblock: tokio::sync::Notify,
    /// Mints with a simulated unconfirmed buy (late-confirm hook).
    pending: StdMutex<Vec<String>>,
    /// Deterministic recheck hang: `std::future::pending` forever —
    /// any code path that awaits a recheck must not be on the tick
    /// path (AUD-013 regression guard).
    hang_recheck: std::sync::atomic::AtomicBool,
    /// Pending entries resolve as `Confirmed` on the next recheck.
    recheck_confirms: std::sync::atomic::AtomicBool,
    /// Pending entries resolve as provably `Dead` on the next recheck
    /// (AUD-003: divergence marking on the recheck path).
    recheck_dies: std::sync::atomic::AtomicBool,
    /// Simulated WAL write failure (entries must halt).
    wal_degraded: std::sync::atomic::AtomicBool,
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
                dispatch_lamports: StdMutex::new(0),
                fail_buys: std::sync::atomic::AtomicBool::new(false),
                dead_buys: StdMutex::new(None),
                fail_sells: std::sync::atomic::AtomicBool::new(false),
                fail_sells_transient: std::sync::atomic::AtomicBool::new(false),
                fail_sells_unknown: std::sync::atomic::AtomicBool::new(false),
                fail_sells_rate_limited: std::sync::atomic::AtomicBool::new(false),
                block_mint: StdMutex::new(None),
                started: StdMutex::new(vec![]),
                unblock: tokio::sync::Notify::new(),
                pending: StdMutex::new(vec![]),
                hang_recheck: std::sync::atomic::AtomicBool::new(false),
                recheck_confirms: std::sync::atomic::AtomicBool::new(false),
                recheck_dies: std::sync::atomic::AtomicBool::new(false),
                wal_degraded: std::sync::atomic::AtomicBool::new(false),
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

    /// Stated dispatch cost (lamports) for both real cost legs.
    fn charge_lamports(&self) -> u64 {
        *self.inner.dispatch_lamports.lock().expect("lock")
    }
}

impl BuySink for StubLive {
    async fn buy(
        &self,
        req: &LiveBuyParams,
    ) -> Result<(LivePosition, String, Option<BuyFill>), LiveError> {
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
        if let Some(reason) = *self.inner.dead_buys.lock().expect("lock") {
            return Err(LiveError::Dead {
                reason,
                context: "stubbed dead buy".to_string(),
            });
        }
        self.inner.buys.lock().expect("lock").push((
            mint.to_string(),
            curve.to_string(),
            assoc_curve.to_string(),
            notional_usd,
        ));
        Ok((
            LivePosition {
                mint,
                curve,
                assoc_curve,
                amount_tokens: 1_000,
                creator: None,
            },
            "bundle-stub".to_string(),
            // Stubs never observe fills: no fill, no entry gap.
            None,
        ))
    }
}

impl SellSink for StubLive {
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
            return Err(LiveError::Unconfirmed(
                "stubbed transient sell failure".to_string(),
            ));
        }
        if self
            .inner
            .fail_sells_unknown
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.inner
                .started
                .lock()
                .expect("lock")
                .push(position.mint.to_string());
            return Err(LiveError::UnknownDispatch {
                position: Box::new(position.clone()),
                reason: "stubbed unknown sell dispatch".to_string(),
            });
        }
        if self
            .inner
            .fail_sells_rate_limited
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            // Deterministic Jito refusal (429): the engine answered
            // without accepting, so this is a retryable failure and
            // never unknown custody (AUD-002).
            return Err(LiveError::Dispatch(ExecutorError::Jito(
                JitoError::RateLimited,
            )));
        }
        self.inner
            .started
            .lock()
            .expect("lock")
            .push(position.mint.to_string());
        if self.inner.block_mint.lock().expect("lock").as_deref()
            == Some(position.mint.to_string().as_str())
        {
            self.inner.unblock.notified().await;
        }
        self.inner
            .sells
            .lock()
            .expect("lock")
            .push((position.mint.to_string(), position.amount_tokens));
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
            return Err(LiveError::BadOrder(
                "stubbed raydium sell failure".to_string(),
            ));
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
        if self
            .inner
            .fail_sells_unknown
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(LiveError::UnknownDispatch {
                position: Box::new(position.clone()),
                reason: "stubbed unknown raydium sell dispatch".to_string(),
            });
        }
        self.inner.raydium_sells.lock().expect("lock").push((
            position.mint.to_string(),
            pool.amm.to_string(),
            position.amount_tokens,
        ));
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
            return Err(LiveError::BadOrder(
                "stubbed pumpswap sell failure".to_string(),
            ));
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
        if self
            .inner
            .fail_sells_unknown
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(LiveError::UnknownDispatch {
                position: Box::new(position.clone()),
                reason: "stubbed unknown pumpswap sell dispatch".to_string(),
            });
        }
        self.inner.pumpswap_sells.lock().expect("lock").push((
            position.mint.to_string(),
            pool.amm.to_string(),
            position.amount_tokens,
        ));
        self.inner
            .sell_tips
            .lock()
            .expect("lock")
            .push(req.tip_lamports);
        Ok("bundle-stub-pumpswap".to_string())
    }
}

impl PendingStore for StubLive {
    // No unconfirmed buys of its own: the `pending` hook states a
    // late-confirm scenario explicitly, empty by default.
    fn has_pending(&self) -> bool {
        !self.inner.pending.lock().expect("lock").is_empty()
    }

    fn pending_mints(&self) -> Vec<String> {
        self.inner.pending.lock().expect("lock").clone()
    }

    fn has_pending_for(&self, mint: &str) -> bool {
        self.inner
            .pending
            .lock()
            .expect("lock")
            .iter()
            .any(|m| m == mint)
    }

    async fn prior_sell_pending(&self, _mint: &str) -> bool {
        false
    }

    async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        if self
            .inner
            .hang_recheck
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            // Hang forever: deterministic stale-forever future. Used
            // by the AUD-013 guard test (a tick must never wait behind
            // a recheck round).
            std::future::pending::<()>().await;
            return Vec::new();
        }
        if self
            .inner
            .recheck_confirms
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self
                .inner
                .pending
                .lock()
                .expect("lock")
                .drain(..)
                .map(|mint| {
                    let mint = mint.clone();
                    PendingRecheck {
                        position: LivePosition {
                            mint: pk(&mint),
                            curve: pk(CURVE),
                            assoc_curve: pk(ASSOC),
                            amount_tokens: 1_000,
                            creator: None,
                        },
                        bundle_id: format!("bundle-{mint}"),
                        signature: format!("sig-{mint}"),
                        outcome: RecheckOutcome::Confirmed,
                    }
                })
                .collect();
        }
        if self
            .inner
            .recheck_dies
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self
                .inner
                .pending
                .lock()
                .expect("lock")
                .drain(..)
                .map(|mint| {
                    let mint = mint.clone();
                    PendingRecheck {
                        position: LivePosition {
                            mint: pk(&mint),
                            curve: pk(CURVE),
                            assoc_curve: pk(ASSOC),
                            amount_tokens: 1_000,
                            creator: None,
                        },
                        bundle_id: format!("bundle-{mint}"),
                        signature: format!("sig-{mint}"),
                        outcome: RecheckOutcome::Dead(DeadReason::Expired),
                    }
                })
                .collect();
        }
        Vec::new()
    }
}

impl CostLedger for StubLive {
    // Pure escalation (no oracle floor): the stub states its
    // ledger semantics explicitly instead of inheriting them.
    async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        escalated_tip(self.inner.tip_lamports, failed_attempts)
    }

    fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        tip_lamports
    }

    // One stated number for both real cost legs; 0 (the default) books
    // nothing, matching the old composed zero.
    fn first_touch_cost_lamports(&self) -> u64 {
        self.charge_lamports()
    }

    // A landed-and-errored dispatch burns base + priority (the tip
    // rolled back); the same knob prices it so the debit is testable.
    fn failed_dispatch_cost_lamports(&self) -> u64 {
        self.charge_lamports()
    }
}

impl WalSink for StubLive {
    // No durability: stub custody lives in test memory.
    fn acknowledge_buy(&self, _position: &LivePosition, _bundle_id: &str) {}

    fn record_manual(&self, _position: &LivePosition, _note: &str) {}

    fn record_sell(&self, _mint: &str, _amount_tokens: u64, _bundle_id: &str) {}

    fn unrecord_manual(&self, _position: &LivePosition) {}

    fn wal_degraded(&self) -> bool {
        self.inner
            .wal_degraded
            .load(std::sync::atomic::Ordering::Relaxed)
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
    /// Venue fee reported (`Some` = readable tier, standard 125).
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
            fee_bps: Some(125.0),
        }
    }

    fn failing() -> Self {
        Self {
            tick: None,
            watched: StdMutex::new(vec![]),
            unwatched: StdMutex::new(vec![]),
            fail_watch: false,
            decimals: 6,
            fee_bps: Some(125.0),
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
        curve: CURVE.to_string(),
        price_usd: price,
        liquidity_usd: 30_000.0,
        complete: false,
        exec: None,
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
    /// Confirm-time fresh screens reject (serial-launcher detected
    /// between intake and quorum; AUD-004 wiring proof).
    fresh_rejects: std::sync::atomic::AtomicBool,
    /// Canned top-10 holder share (`None` = RPC failure).
    top_holders: StdMutex<Option<f64>>,
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
                fresh_rejects: std::sync::atomic::AtomicBool::new(false),
                top_holders: StdMutex::new(None),
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
        stub.inner
            .reject_wallets
            .lock()
            .expect("lock")
            .push(wallet.to_string());
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

    /// Canned top-10 holder share (`None` = RPC failure).
    fn with_swaps_and_top_holders(mint: &str, swaps: Vec<SwapRecord>, pct: Option<f64>) -> Self {
        let stub = Self::with_swaps(mint, swaps);
        *stub.inner.top_holders.lock().expect("lock") = pct;
        stub
    }
}

impl Screener for StubScreener {
    async fn screen_dev(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
        if self
            .inner
            .reject_wallets
            .lock()
            .expect("lock")
            .contains(&dev.to_string())
        {
            return Ok(DevVerdict {
                pass: false,
                age_secs: Some(100),
                prior_launches: 0,
                history_truncated: false,
                reason: "stub cohort reject".to_string(),
            });
        }
        match *self.inner.dev.lock().expect("lock") {
            DevOutcome::Pass => Ok(DevVerdict {
                pass: true,
                age_secs: Some(7200),
                prior_launches: 0,
                history_truncated: false,
                reason: "stub".to_string(),
            }),
            DevOutcome::Fail => Ok(DevVerdict {
                pass: false,
                age_secs: Some(100),
                prior_launches: 0,
                history_truncated: false,
                reason: "stub reject".to_string(),
            }),
            DevOutcome::Broken => Err(ScreenError::BadAddress("stub".to_string())),
        }
    }

    async fn screen_dev_fresh(&self, dev: &str) -> Result<DevVerdict, ScreenError> {
        if self
            .inner
            .fresh_rejects
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(DevVerdict {
                pass: false,
                age_secs: Some(7200),
                // Fresh history now shows the serial launch.
                prior_launches: 2,
                history_truncated: false,
                reason: "stub serial launcher at confirm".to_string(),
            });
        }
        Screener::screen_dev(self, dev).await
    }

    async fn observe_curve(
        &self,
        mint: &str,
        _curve: &str,
        _since_sig: Option<&str>,
    ) -> Result<CurveRead, ScreenError> {
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
        Ok(if truncated {
            CurveRead::Truncated
        } else {
            CurveRead::Full {
                swaps,
                watermark: Some("sig-stub".to_string()),
            }
        })
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

    async fn top_holder_pct(&self, _mint: &str) -> Result<f64, ScreenError> {
        self.inner
            .top_holders
            .lock()
            .expect("lock")
            .ok_or_else(|| ScreenError::BadAddress("stub".to_string()))
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

#[allow(clippy::too_many_arguments)]
fn pipeline_engined(
    settings: Settings,
    outcome: AuditOutcome,
    reserves: StubReserves,
    portfolio: PaperPortfolio,
    screener: StubScreener,
    live: Option<StubLive>,
    alerts: Option<StubAlerts>,
    engine: Option<risk_engine::RiskEngine>,
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
        engine,
        reserves,
        portfolio,
        live,
        screener,
        alerts,
        vaults,
        SolUsdFeed::new_for_test(150.0),
        None,
    )
}

/// Unreachable model endpoint: refuses fast, always heuristic. This is
/// the production-default shape (engine present, Ollama absent).
fn dead_engine() -> Option<risk_engine::RiskEngine> {
    risk_engine::RiskEngine::builder()
        .endpoint("http://127.0.0.1:9")
        .timeout(std::time::Duration::from_millis(100))
        .build()
        .ok()
}

#[tokio::test]
async fn engined_fallback_still_opens() {
    // Engine present but down: confirm falls back to heuristic and
    // opens — entries must not halt when the model does. Default
    // window: a zero window times out at first observation by design
    // (`gate_times_out_on_dead_token`), so it can never confirm here.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_engined(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        PaperPortfolio::new(100.0, 0.0),
        screener,
        None,
        None,
        dead_engine(),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
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
        None,
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
    let missing =
        || Err(CurveError::AccountMissing("M".to_string())) as Result<PriceTick, CurveError>;
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
    // wallets) scores below any survivor; heavy priors bottom out
    // at 0.0, tripping the heuristic `< 0.40` PASS rule instead of
    // saturating exactly at the cutoff.
    approx(dev_reputation_score(None, 0, 3600), 0.6);
    approx(dev_reputation_score(Some(3600), 100, 3600), 0.0);
    approx(dev_reputation_score(Some(3600), 10, 3600), 0.2);
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
            exec: None,
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
            exec: None,
            observed_at: 2_000,
        };
        assert_eq!(
            resolve_entry_with_flag(Ok(bad), Some(9.0), 500.0),
            None,
            "liq {liq}"
        );
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
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Flaky {
            fails_left: std::sync::Mutex::new(9999),
            score: 100,
        },
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert_eq!(pipe.pending_count().await, 1);
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    let state = pipe.gate_state.lock().await;
    assert_eq!(state.meta[MINT].audit_score, None);
}

#[tokio::test]
async fn audit_retry_recovers_and_opens() {
    // The retry lands (indexing caught up): quorum confirms with the
    // resolved score and the position opens.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Flaky {
            fails_left: std::sync::Mutex::new(1),
            score: 100,
        },
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert_eq!(pipe.pending_count().await, 1);
    // Age the attempt past the retry throttle so this round re-polls.
    pipe.gate_state
        .lock()
        .await
        .meta
        .get_mut(MINT)
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12]));
    let pipe = pipeline_screened(
        settings(&[("GATE_WINDOW_SECS", "0")]),
        AuditOutcome::Flaky {
            fails_left: std::sync::Mutex::new(9999),
            score: 100,
        },
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
        StubReserves::with_tick(tick(MINT, 2.0)),
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
        state.poll = Some(0);
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
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: opened_now,
        })
        .expect("manual open");
    pipe.exec.live_positions.lock().await.insert(
        "11111111111111111111111111111111".to_string(),
        LivePosition {
            mint: pk("11111111111111111111111111111111"),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
            amount_tokens: 1_000,
            creator: None,
        },
    );
    assert_eq!(pipe.open_position_mints().await, vec![MINT.to_string()]);
    assert_eq!(
        pipe.live_tracked_mints().await,
        vec!["11111111111111111111111111111111".to_string()]
    );
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
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: now_secs(),
        })
        .expect("manual open");
    // Before any tick: book value (zero unrealized). Cash reflects
    // the actually-invested size (floored whole tokens at the filled
    // entry), not the nominal ticket.
    let (cash_book, equity_book) = pipe.paper_cash_equity().await;
    assert!((equity_book - 100.0).abs() < 1e-9, "book {equity_book}");
    let invested = {
        let portfolio = pipe.portfolio.lock().await;
        let id = portfolio.position_ids()[0];
        let pos = portfolio.position(id).expect("pos");
        pos.quantity as f64 * pos.entry_price
    };
    assert!(
        (cash_book - (100.0 - invested)).abs() < 1e-9,
        "cash {cash_book}"
    );
    // A fresh flat tick marks equity realizable: the 125 bps venue
    // fee on the exit leg deducts, so equity drops below book.
    pipe.on_tick(&tick(MINT, 2.0)).await;
    assert_eq!(
        pipe.portfolio.lock().await.positions_for_mint(MINT).len(),
        1
    );
    let snap = pipe.portfolio.lock().await.snapshot(&HashMap::from([(
        MINT.to_string(),
        simulator::Mark::new(2.0, 30_000.0),
    )]));
    let (cash, equity) = pipe.paper_cash_equity().await;
    assert_eq!(cash, snap.cash);
    assert!((equity - snap.equity).abs() < 1e-9);
    assert!(equity < 100.0, "exit costs must deduct: {equity}");
    assert!(snap.unrealized_pnl < 0.0);
}

#[tokio::test]
async fn daily_breaker_blocks_opens_but_not_exits() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
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
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());

    // Exits still work past the cap: open manually, force a stop.
    let id = pipe
        .portfolio
        .lock()
        .await
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: now_secs(),
        })
        .expect("manual open");
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0; // -50% << -8% SL.
    pipe.on_tick(&stop).await;
    assert!(pipe.portfolio.lock().await.position(id).is_none());
    // The exit loss still feeds the breaker even though recording now
    // happens after the portfolio guard is released (lock-order fix).
    assert!(pipe.daily.lock().await.day_net() < -10.0);
}

/// Ticket shape under test: default $10 notional on the $30k test
/// tick. Seeds must land in that same cell ($10 = $2 paper × 5
/// tokens; $30k liq), or the gate reads a different cell.
type BleedPipe = Arc<Pipeline<StubAuditor, StubReserves, StubLive, StubScreener, StubAlerts>>;

fn bleed_pipe(cap_usd: f64) -> BleedPipe {
    let mut settings = Settings::from_map(&HashMap::new());
    settings.max_expected_entry_gap_usd = cap_usd;
    pipeline_screened(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
        None,
    )
}

async fn seed_entry_bleed(pipe: &BleedPipe, samples: usize, fill_price: f64) {
    for i in 0..samples {
        pipe.exec
            .gaps
            .lock()
            .await
            .note_entry_gap(
                &format!("SEED{i}"),
                2.0,
                fill_price,
                5.0,
                "curve",
                30_000.0,
                1_000 + i as u64,
            )
            .expect("seed gap");
    }
}

async fn launch_once(pipe: &BleedPipe) {
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
}

#[tokio::test]
async fn entry_bleed_cap_blocks_only_proven_bleeders() {
    // Proven bleed worse than the cap: nothing opens ($2 paper, $3
    // fill × 5 tokens = -$5.00 per ticket against a $1 cap).
    let pipe = bleed_pipe(1.0);
    seed_entry_bleed(&pipe, 3, 3.0).await;
    launch_once(&pipe).await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "proven bleed must refuse the open"
    );

    // The refusal never touches exits: a manually opened position
    // still stop-losses out through the same tick path.
    let id = pipe
        .portfolio
        .lock()
        .await
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: now_secs(),
        })
        .expect("manual open");
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0; // -50% << -8% SL.
    pipe.on_tick(&stop).await;
    assert!(
        pipe.portfolio.lock().await.position(id).is_none(),
        "bleed cap must never block an exit"
    );

    // Thin cell (2 samples < floor): unknown allows, never refuses.
    let pipe = bleed_pipe(1.0);
    seed_entry_bleed(&pipe, 2, 3.0).await;
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "thin cell is unknown, not a verdict"
    );

    // Proven bleed within the cap (-$0.50 vs $1): opens.
    let pipe = bleed_pipe(1.0);
    seed_entry_bleed(&pipe, 3, 2.1).await;
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "tolerable bleed opens"
    );

    // Cap off (0): proven bleed opens — measurement without refusal.
    let pipe = bleed_pipe(0.0);
    seed_entry_bleed(&pipe, 3, 3.0).await;
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "cap off disables the gate"
    );
}

fn holder_pipe(cap_pct: f64, holders: Option<f64>) -> BleedPipe {
    let mut settings = Settings::from_map(&HashMap::new());
    settings.max_top_holder_pct = cap_pct;
    pipeline_screened(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::with_swaps_and_top_holders(
            MINT,
            buyer_swaps(MINT, &[11, 12, 13, 14, 15]),
            holders,
        ),
        None,
    )
}

#[tokio::test]
async fn holder_concentration_cap_blocks_whale_mints() {
    // Top-10 own 60% against a 50% cap: nothing opens.
    let pipe = holder_pipe(50.0, Some(60.0));
    launch_once(&pipe).await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "concentrated supply must refuse the open"
    );

    // Within cap: opens.
    let pipe = holder_pipe(50.0, Some(30.0));
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "distributed supply opens"
    );

    // Cap off: the read never runs (concentrated supply still opens).
    let pipe = holder_pipe(0.0, Some(60.0));
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "cap off disables the read"
    );

    // Unreadable holdings while armed: fail closed.
    let pipe = holder_pipe(50.0, None);
    launch_once(&pipe).await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "unknown concentration must refuse while armed"
    );
}

#[tokio::test]
async fn entry_flip_cost_cap_blocks_cost_strapped_entries() {
    // The default tick (spot $2, $30k liquidity) prices a ~2.5% round
    // trip through the proxy fill: entry impact + exit impact + two
    // venue legs, all inside the preview that also feeds the model.
    // The stock test book fills at Exact (zero cost, deterministic
    // books) — the cap test needs the production curve model, which
    // is what the boot path builds.
    fn flip_pipe(cap_pct: f64) -> BleedPipe {
        let mut settings = Settings::from_map(&HashMap::new());
        settings.max_entry_flip_loss_pct = cap_pct;
        pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(MINT, 2.0)),
            PaperPortfolio::new(100.0, 0.0),
            StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
            None,
        )
    }

    // Cap under the measured cost: nothing opens; the ticket can never
    // clear its own round-trip cost, so no edge exists yet.
    let pipe = flip_pipe(2.0);
    launch_once(&pipe).await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "a ticket whose flip cost exceeds the cap must refuse the open"
    );

    // Cap over the measured cost: opens.
    let pipe = flip_pipe(5.0);
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "tolerable flip cost opens"
    );

    // Cap off (0): the preview still prices for the model and the
    // journal, but nothing refuses.
    let pipe = flip_pipe(0.0);
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "cap off disables the refusal"
    );
}

#[tokio::test]
async fn forced_exit_regime_shapes_the_open() {
    // Heuristic alone yields the moderate (conservative) band; the
    // forced baseline overrides the pick with tested momentum numbers
    // (TP 60, trailed) — the fixed baseline for evaluation, and the
    // LLM kill path.
    let mut settings = Settings::from_map(&HashMap::new());
    settings.force_exit_regime = "momentum".to_string();
    let pipe = pipeline_screened(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    let ids = pipe.portfolio.lock().await.position_ids();
    assert_eq!(ids.len(), 1);
    let pos = pipe
        .portfolio
        .lock()
        .await
        .position(ids[0])
        .expect("open")
        .clone();
    assert_eq!((pos.tp_percent, pos.trailing_stop), (60.0, true));
}

#[tokio::test]
async fn fallback_policy_defer_rewatches_then_opens_on_exhaustion() {
    // Engine present but down with policy=defer: the first confirm
    // postpones (signal re-watched, nothing opens); the second confirm
    // exhausts the default one-defer budget and opens on the validated
    // heuristic bands — never held open-ended, never skipped silently.
    let mut settings = Settings::from_map(&HashMap::new());
    settings.inference_fallback = "defer".to_string();
    assert_eq!(settings.inference_defer_max_n, 1);
    let pipe = pipeline_engined(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
        None,
        None,
        dead_engine(),
    );
    launch_once(&pipe).await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        0,
        "first confirm defers, never opens on broken inference"
    );
    // The signal is back on the watchlist, carrying its defer count.
    let watch = pipe.gate_state.lock().await.meta.clone();
    assert_eq!(
        watch.get(MINT).map(|s| s.defers),
        Some(1),
        "defer counted on the signal"
    );
    pipe.poll_gates().await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "exhausted defer lands on the heuristic, never dropped"
    );
}

#[tokio::test]
async fn fallback_policy_defer_zero_off_and_engine_absent_never_defers() {
    // DEFER_MAX_N=0 turns defer off at once: the confirm opens on the
    // heuristic in a single round. Engine-absent (deliberate mode)
    // also never defers — nothing would recover — so policy=defer
    // behaves as heuristic there in one round too.
    for defer_n in [0u32, 1] {
        let mut settings = Settings::from_map(&HashMap::new());
        settings.inference_fallback = "defer".to_string();
        settings.inference_defer_max_n = defer_n;
        let pipe = pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(MINT, 2.0)),
            funded(),
            StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
            None,
        );
        launch_once(&pipe).await;
        assert_eq!(
            pipe.portfolio.lock().await.position_ids().len(),
            1,
            "defer_n={defer_n}"
        );
    }
}

#[tokio::test]
async fn fallback_policy_pass_skips_entries_on_model_failure() {
    // No engine (the deliberate-heuristic path) with policy=pass:
    // the confirm skips instead of opening on the fallback profile.
    // Same tape with the default heuristic policy opens — the policy
    // is the only difference.
    for (policy, expect_open) in [("pass", false), ("heuristic", true)] {
        let mut settings = Settings::from_map(&HashMap::new());
        settings.inference_fallback = policy.to_string();
        let pipe = pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(MINT, 2.0)),
            funded(),
            StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
            None,
        );
        launch_once(&pipe).await;
        assert_eq!(
            pipe.portfolio.lock().await.position_ids().len(),
            usize::from(expect_open),
            "policy={policy}"
        );
    }
}

#[tokio::test]
async fn confirm_flow_opens_with_measured_velocity() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
async fn heat_counts_live_held_mints() {
    // Paper cash recycles on close while live SOL stays locked: a
    // parked manual bag must occupy heat, or a stuck book keeps
    // opening into divergence on recycled cash.
    let live = StubLive::working();
    let mut settings = Settings::from_map(&HashMap::new());
    settings.max_open_positions = 1;
    let pipe = pipeline_screened(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
        Some(live.clone()),
    );
    pipe.exec.live_manual.lock().await.insert(
        MINT.to_string(),
        LivePosition {
            mint: pk(MINT),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
            amount_tokens: 1_000,
            creator: None,
        },
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
}

#[tokio::test]
async fn kill_switch_pauses_entries_but_not_exits() {
    // The marker halts new risk only: a held position keeps managing
    // (exits, scale-outs) and clearing the switch re-arms entries.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    let dev = dev_wallet();
    let launch = LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev,
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    };
    pipe.set_entries_paused(true);
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "paused: nothing opens"
    );

    // Clearing the switch (marker removed, flag flips) re-arms entries.
    pipe.set_entries_paused(false);
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);

    // Paused again with a position open: the stop-loss still fires.
    pipe.set_entries_paused(true);
    let id = pipe.portfolio.lock().await.position_ids()[0];
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0; // -50% << -8% SL.
    pipe.on_tick(&stop).await;
    assert!(
        pipe.portfolio.lock().await.position(id).is_none(),
        "paused: exits keep running"
    );
}

#[tokio::test]
async fn data_degrade_pauses_entries_but_not_exits() {
    // Same choke point as the kill switch, separate (supervisor-owned)
    // state: unhealthy data halts new risk only — a held position keeps
    // managing, and recovery (all sources healthy) re-arms entries.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    let dev = dev_wallet();
    let launch = LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev,
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    };
    pipe.set_data_degraded(true, &["test source down".to_string()]);
    assert!(pipe.data_degraded());
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "degraded: nothing opens"
    );

    // Recovery clears it: entries re-arm without an operator marker.
    pipe.set_data_degraded(false, &[]);
    assert!(!pipe.data_degraded());
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);

    // Degraded again with a position open: the stop-loss still fires.
    pipe.set_data_degraded(true, &["test source down".to_string()]);
    let id = pipe.portfolio.lock().await.position_ids()[0];
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0; // -50% << -8% SL.
    pipe.on_tick(&stop).await;
    assert!(
        pipe.portfolio.lock().await.position(id).is_none(),
        "degraded: exits keep running"
    );
}

#[tokio::test]
async fn migrated_curve_rejects_entries() {
    // Entries are pre-migration only: a complete tick refuses at the
    // choke (paper must not open what live refuses at preflight), and
    // so does a pricing tick for a mint the pipeline already knows as
    // migrated (ticks that don't carry the flag).
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let dev = dev_wallet();
    let launch = LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev,
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    };
    // Case 1: the confirm tick reads complete.
    let mut done = tick(MINT, 2.0);
    done.complete = true;
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(done),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "complete tick: nothing opens"
    );

    // Case 2: pricing tick, but the mint is known-migrated.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.migrated
        .lock()
        .await
        .insert(MINT.to_string(), now_secs());
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "known-migrated mint: nothing opens"
    );
}

#[tokio::test]
async fn migration_hunt_routes_pool_exit_end_to_end() {
    // Venue switch without hand-inserted legs: open pre-migration,
    // resolve the pool through the hunt (`detect_migration` stub),
    // then exit — the live leg must ride the pool, never the curve.
    use listener::MigrationDestination;
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    *screener.inner.migration.lock().expect("lock") = Some(MigrationReport {
        signature: "migsig".to_string(),
        destination: MigrationDestination::PumpSwap,
        amm_id: Some(fixture_wallet(20)),
        base_mint: Some("WSOL".to_string()),
        quote_mint: Some(MINT.to_string()),
        base_vault: Some(fixture_wallet(21)),
        quote_vault: Some(fixture_wallet(22)),
    });
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    // Hunt (not a hand insert): the stub report becomes the pool leg.
    pipe.report_migration_once(MINT, CURVE).await;
    {
        let followed = pipe.followed.lock().await;
        let leg = followed
            .get(MINT)
            .and_then(|state| state.pool.clone())
            .expect("hunt stores the leg");
        assert_eq!(leg.amm, pk(&fixture_wallet(20)));
    }
    // Exit: the live leg rides PumpSwap; the curve leg stays silent.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(live.sells().is_empty());
    assert!(live.raydium_sells().is_empty());
    assert_eq!(
        live.pumpswap_sells(),
        vec![(MINT.to_string(), fixture_wallet(20), 1_000)]
    );
}

#[tokio::test]
async fn complete_tick_resolves_migration_before_first_sell_attempt() {
    // AUD-001: an open live bag held at curve completion must be sold
    // through the pool leg on the FIRST complete tick. Previously the
    // hunt ran only from `maybe_unwatch`, which returns early while a
    // bag is tracked — so five curve attempts failed `BadOrder` and the
    // bag parked as manual before the pool leg was ever registered.
    use listener::MigrationDestination;
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    *screener.inner.migration.lock().expect("lock") = Some(MigrationReport {
        signature: "migsig".to_string(),
        destination: MigrationDestination::PumpSwap,
        amm_id: Some(fixture_wallet(20)),
        base_mint: Some("WSOL".to_string()),
        quote_mint: Some(MINT.to_string()),
        base_vault: Some(fixture_wallet(21)),
        quote_vault: Some(fixture_wallet(22)),
    });
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(!live.buys().is_empty(), "live buy dispatched");
    // The complete tick itself must hunt, register the pool leg, and
    // route the same tick's sell through PumpSwap — never the curve.
    let mut done = tick(MINT, 2.0);
    done.complete = true;
    pipe.on_tick(&done).await;
    assert!(
        live.sells().is_empty(),
        "no curve sell may be attempted on a complete curve"
    );
    assert!(live.raydium_sells().is_empty());
    assert_eq!(
        live.pumpswap_sells(),
        vec![(MINT.to_string(), fixture_wallet(20), 1_000)]
    );
    // Sold in full: nothing parked for manual exit.
    assert!(pipe.exec.manual_keys().await.is_empty());
}

#[tokio::test]
async fn complete_tick_with_hunt_none_preserves_fail_closed_refusal() {
    // AUD-001 case (b): when the hunt finds no valid pool, the
    // fail-closed behavior is preserved — no phantom pool sell, the
    // curve leg is still the (refusing) route, and the bag stays held.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    // No migration report: hunt resolves None.
    *screener.inner.migration.lock().expect("lock") = None;
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // Force the curve refusal the real executor deterministically
    // raises on a complete curve (executor preflight refusal).
    live.inner
        .fail_sells
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut done = tick(MINT, 2.0);
    done.complete = true;
    pipe.on_tick(&done).await;
    assert!(
        live.pumpswap_sells().is_empty() && live.raydium_sells().is_empty(),
        "no pool leg may be materialized from a None hunt"
    );
    // The deterministic refusal consumed one attempt of the retry
    // budget: the bag is still tracked (not parked) after this tick.
    assert!(
        !pipe.exec.live_positions.lock().await.is_empty(),
        "bag still tracked on the first unresolved complete tick"
    );
    assert!(pipe.exec.manual_keys().await.is_empty());
    // The breaker feed defers (live unsettled), matching today's stash path.
    assert!(pipe.exec.take_deferred(MINT).await.is_some());
}

#[tokio::test]
async fn invalid_fee_bps_cannot_disable_paper_stops() {
    // AUD-007: an invalid FEE_BPS (>=100%) used to ride raw into the
    // gate's fee-aware stop widening, clamping the stop to the -100%
    // floor. The typed guard must neutralize it: the standard stop
    // still fires.
    let mut settings = HashMap::new();
    settings.insert("FEE_BPS".to_string(), "10000".to_string());
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&settings),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    assert!(
        Settings::from_map(&settings).fee().is_none(),
        "100% commission is untyped; live boot refuses it"
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // -50% from entry: the widened -100% stop would never fire; the
    // typed guard keeps the -8% stop firing.
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0;
    pipe.on_tick(&stop).await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "an out-of-range FEE_BPS must not remove stop-losses"
    );
}

#[tokio::test]
async fn confirm_rescreen_bypasses_dev_verdict_cache() {
    // AUD-004: a dev who launched again between intake and quorum gets
    // a confirm-time re-screen with FRESH history — the cached intake
    // pass must not admit the open.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    screener
        .inner
        .fresh_rejects
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "confirm must reject on fresh prior-launch history"
    );
}

#[tokio::test]
async fn recheck_dead_marks_paper_diverged() {
    // AUD-003: a pending buy proven dead on recheck leaves the paper
    // position without live cover — the divergence mark must be set
    // (as the direct-dead path does) so heartbeat counts and alerts
    // report the exposure gap.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    // Pending without a tracked bag; the recheck proves it dead.
    live.inner
        .recheck_dies
        .store(true, std::sync::atomic::Ordering::Relaxed);
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(MINT.to_string());
    pipe.recheck_live_pending().await;
    assert!(
        pipe.exec.live_diverged.lock().await.contains_key(MINT),
        "a proven-dead pending buy must mark the paper open diverged"
    );
    // The paper position stays open (coverage gap, kept until close).
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
}

#[tokio::test]
async fn wal_degraded_halts_entries_but_not_exits() {
    // A failed WAL write means custody can no longer be recorded: no
    // new risk opens (paper or live) until a restart, while a held
    // position keeps managing. Same choke point as the kill switch.
    let live = StubLive::working();
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
        Some(live.clone()),
    );
    let dev = dev_wallet();
    let launch = LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev,
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    };
    let set_degraded = |on: bool| {
        live.inner
            .wal_degraded
            .store(on, std::sync::atomic::Ordering::Relaxed);
    };
    set_degraded(true);
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().is_empty(),
        "degraded WAL: nothing opens"
    );
    assert!(
        live.inner.buys.lock().expect("lock").is_empty(),
        "degraded WAL: nothing dispatches"
    );

    // A working WAL (fresh run) re-arms entries.
    set_degraded(false);
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);

    // Degraded again with a position open: the stop-loss still fires.
    set_degraded(true);
    let id = pipe.portfolio.lock().await.position_ids()[0];
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0; // -50% << -8% SL.
    pipe.on_tick(&stop).await;
    assert!(
        pipe.portfolio.lock().await.position(id).is_none(),
        "degraded WAL: exits keep running"
    );
}

#[tokio::test]
async fn exposure_cap_bounds_total_tickets() {
    // Cap in USD, not in count: one paper ticket (basis 10 on MINT_A)
    // plus one unpaired live bag (counted as one ticket) leaves no room
    // for a third at cap 25; a wider cap admits it.
    async fn seed(pipe: &Pipeline<StubAuditor, StubReserves, StubLive, StubScreener, StubAlerts>) {
        pipe.portfolio
            .lock()
            .await
            .open_position_params(&simulator::OpenPositionParams {
                mint: MINT_A,
                side: Side::Long,
                mark: simulator::Mark::new(2.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: now_secs(),
            })
            .expect("manual paper ticket");
        pipe.exec.live_manual.lock().await.insert(
            MINT_B.to_string(),
            LivePosition {
                mint: pk(MINT_B),
                curve: pk(CURVE),
                assoc_curve: pk(ASSOC),
                amount_tokens: 1_000,
                creator: None,
            },
        );
    }
    fn build(
        cap: f64,
    ) -> Arc<Pipeline<StubAuditor, StubReserves, StubLive, StubScreener, StubAlerts>> {
        let mut settings = Settings::from_map(&HashMap::new());
        settings.notional_usd = 10.0;
        settings.max_exposure_usd = cap;
        pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(MINT, 2.0)),
            funded(),
            StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
            Some(StubLive::working()),
        )
    }
    let dev = dev_wallet();
    let launch = LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev,
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    };

    let pipe = build(25.0);
    seed(&pipe).await;
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert_eq!(
        pipe.portfolio.lock().await.position_ids().len(),
        1,
        "cap 25: 20 deployed + one ticket does not fit"
    );

    let pipe = build(40.0);
    seed(&pipe).await;
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    let portfolio = pipe.portfolio.lock().await;
    assert_eq!(
        portfolio.position_ids().len(),
        2,
        "cap 40 admits the ticket"
    );
    assert!(
        portfolio
            .position_ids()
            .iter()
            .filter_map(|id| portfolio.position(*id))
            .any(|p| p.mint == MINT),
        "the candidate opened"
    );
}

#[tokio::test]
async fn confirm_registers_custom_venue_fee() {
    // FeeConfig read at confirm wires the paper ledger: a 345 bps
    // custom tier lands per mint, while an unreadable tier (None)
    // fails closed (no open).
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let mut reserves = StubReserves::with_tick(tick(MINT, 2.0));
    reserves.fee_bps = Some(345.0);
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        PaperPortfolio::new(100.0, 0.0),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!((pipe.portfolio.lock().await.venue_fee_for(MINT) - 345.0).abs() < 1e-9,);
    // Fee-aware stop: widens past round-trip costs on 345 bps (−10.9).
    let id = pipe.portfolio.lock().await.position_ids()[0];
    let sl = pipe
        .portfolio
        .lock()
        .await
        .position(id)
        .expect("pos")
        .sl_percent;
    let want_sl = fee_aware_sl_floor(-8.0, 345.0);
    assert!((sl - want_sl).abs() < 1e-9, "sl={sl}");
    // Unreadable tier fails closed.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let mut reserves = StubReserves::with_tick(tick(MINT, 2.0));
    reserves.fee_bps = None;
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        PaperPortfolio::new(100.0, 0.0),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
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
        quote_mint: Some(MINT.to_string()),
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
    pipe.report_migration_once(&mint, CURVE).await;
    {
        let followed = pipe.followed.lock().await;
        let leg = followed[&mint].pool.clone().expect("leg stored");
        assert_eq!(leg.amm, pk(&fixture_wallet(20)));
    }
    // Age the entry past TTL with a live bag held: sweep keeps it.
    pipe.exec.live_positions.lock().await.insert(
        mint.clone(),
        LivePosition {
            mint: pk(&mint),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
            amount_tokens: 1_000,
            creator: None,
        },
    );
    {
        let mut followed = pipe.followed.lock().await;
        followed.get_mut(&mint).expect("entry").last_seen_secs = 0;
    }
    let other = PoolTick {
        mint: "OTHER".to_string(),
        ..tick.clone()
    };
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    screener
        .inner
        .truncated
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 1);
}

#[tokio::test]
async fn confirm_high_momentum_keeps_trailing_profile() {
    let buyers: Vec<u8> = (20..55).collect();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &buyers));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        settings(&[("GATE_MIN_BUYERS", "2")]),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn gate_aborts_on_dev_dump() {
    let mut swaps = buyer_swaps(MINT, &[11]);
    swaps.push(
        SwapRecord::new("dump", &dev_wallet(), SwapSide::Sell, 1003, 1_000_000).expect("fixture"),
    );
    let screener = StubScreener::with_swaps(MINT, swaps);
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::passing(),
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert_eq!(pipe.pending_count().await, 1);
    pipe.poll_gates().await;

    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn bundle_reject_drops_signal() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Reject;
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn bundle_suspect_trades_paper_only() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Suspect;
    let live = StubLive::working();
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    // Paper opens (tightened risk = no real funds), live stays quiet.
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(live.buys().is_empty());
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn diverged_close_never_feeds_live_day_loss_breaker() {
    // AUD-002: in live mode, a paper open whose live leg failed must
    // not feed its simulated PnL into `max_daily_loss_usd` — a phantom
    // paper win must not loosen (or offset into) a live circuit breaker.
    let live = StubLive::working();
    live.inner
        .fail_buys
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(live.buys().is_empty());
    assert!(pipe.portfolio.lock().await.position_ids().len() == 1);
    // TP close: a fat paper profit that used to be recorded here.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(
        pipe.daily.lock().await.day_net(),
        0.0,
        "an uncovered paper close must not move the live day-loss breaker"
    );
}

#[tokio::test]
async fn bundle_suspect_close_never_feeds_live_day_loss_breaker() {
    // AUD-002: paper-only opens (bundle suspect) settle through the
    // tick path with `live_settled == true` and no live custody — their
    // simulated PnL must be excluded from the live breaker.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Suspect;
    let live = StubLive::working();
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(live.buys().is_empty());
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(
        pipe.daily.lock().await.day_net(),
        0.0,
        "a bundle-suspect paper close must not move the live day-loss breaker"
    );
}

#[tokio::test]
async fn covered_realized_loss_still_trips_live_day_loss_cap() {
    // AUD-002 control: a covered trade's realized (modeled) loss still
    // trips the cap at the configured threshold.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let mut settings = HashMap::new();
    settings.insert("notional_usd".to_string(), "100.0".to_string());
    settings.insert("max_daily_loss_usd".to_string(), "5.0".to_string());
    let pipe = pipeline_screened(
        Settings::from_map(&settings),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(
        !live.buys().is_empty(),
        "the live leg landed, so this paper book is covered"
    );
    // Stop-loss close on a covered position counts against the cap.
    let mut stop = tick(MINT, 2.0);
    stop.price_usd = 1.0; // -50%, far past the SL trigger.
    pipe.on_tick(&stop).await;
    let day_net = pipe.daily.lock().await.day_net();
    assert!(
        day_net <= -5.0,
        "a chain-mirrored realized loss must move and trip the cap (day_net={day_net})"
    );
}

#[tokio::test]
async fn bundle_screen_error_drops_fail_closed() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    *screener.inner.bundle.lock().expect("lock") = BundleOutcome::Broken;
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
        base_mint: Some(MINT.to_string()),
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
    assert!(
        matches!(state.stage, crate::follow::FollowStage::Seeding),
        "baseline set on first tick"
    );
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
        mint: MINT.to_string(),
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
    let mut stale = DailyBreaker {
        day,
        day_net: -1_000.0,
        halt_alerted_day: 0,
    };
    assert!(!stale.tripped(10.0, day + 86_400));
}

#[test]
fn halt_card_fires_once_per_day() {
    let day = 2_000 * 86_400;
    let mut b = DailyBreaker::new(day);
    // Not tripped: silence.
    assert_eq!(b.halt_state(10.0, day), (false, false));
    b.record(-10.0, day);
    // Newly tripped: card fires once, state stays tripped.
    assert_eq!(b.halt_state(10.0, day), (true, true));
    assert_eq!(b.halt_state(10.0, day), (true, false));
    // Rollover re-arms both trip and card.
    assert_eq!(b.halt_state(10.0, day + 86_400), (false, false));
    b.record(-10.0, day + 86_400);
    assert_eq!(b.halt_state(10.0, day + 86_400), (true, true));
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
        mint: MINT.to_string(),
        amm_id: "AMM".to_string(),
        price_usd: 100.0,
        liquidity_usd: 50_000.0,
        observed_at: 3000,
    };
    pipe.on_pool_tick(&old).await;
    for bad_price in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        pipe.on_pool_tick(&PoolTick {
            mint: MINT.to_string(),
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
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::passing(),
        None,
        Some(alerts.clone()),
    );
    let mut graduated = tick(MINT, 2.0);
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
            StubReserves::with_tick(tick(MINT, 2.0)),
            funded(),
            StubScreener::with_dev(dev),
            None,
        );
        pipe.on_launch(&LaunchRequest {
            mint: MINT,
            curve: CURVE,
            assoc_curve: ASSOC,
            dev: &dev_wallet(),
            creator: None,
            name: "TestToken",
            symbol: "TEST",
            slot: 5000,
        })
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
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::rejecting(&creator),
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: Some(&creator),
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn launch_watches_creator_cohort_dump() {
    // Distinct v2 creator is screened, watchlisted, and its dump
    // aborts the gate like the launcher's.
    let creator = fixture_wallet(8);
    let dump = SwapRecord::new("dump", &creator, SwapSide::Sell, 1003, 1_000_000).expect("fixture");
    let screener = StubScreener::with_swaps(MINT, vec![dump]);
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: Some(&creator),
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert_eq!(pipe.pending_count().await, 1);
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn launch_skips_on_audit_reject_and_error() {
    // Policy rejection drops at once (never queued).
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Reject,
        reserves,
        funded(),
        StubScreener::passing(),
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Flaky {
            fails_left: std::sync::Mutex::new(9999),
            score: 100,
        },
        reserves,
        funded(),
        StubScreener::passing(),
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 1);
    assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn audit_schema_break_drops_immediately() {
    // Schema break is permanent: drop at once, never watchlist-retry.
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Broken,
        reserves,
        funded(),
        StubScreener::passing(),
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
    assert!(pipe.reserves.watched.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn confirm_drops_without_any_price() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::failing(),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn confirm_drops_non_six_decimal_mint() {
    // Sizing scales by 10^6 everywhere: a 9-decimal mint would
    // misprice entries 1000x — drop loudly instead of guessing.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0)).with_decimals(9);
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.pending_count().await, 0);
}

#[tokio::test]
async fn confirm_uses_quote_override_when_tick_fails() {
    // Override liquidity must itself clear the $10k moderate floor.
    let settings = settings(&[("QUOTE_USD", "3.0"), ("LIQUIDITY_USD", "15000")]);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::failing(),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        settings,
        AuditOutcome::Allow(100),
        StubReserves::failing(),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert!(pipe.exec.synthetic_mints.lock().await.contains(MINT));
    // +100% over the 3.0 override entry clears TP with real profit.
    pipe.on_tick(&tick(MINT, 6.0)).await;
    let portfolio = pipe.portfolio.lock().await;
    assert_eq!(portfolio.history().len(), 1);
    assert!(portfolio.history()[0].net_pnl > 0.0);
    drop(portfolio);
    assert_eq!(pipe.daily.lock().await.day_net(), 0.0);
}

#[tokio::test]
async fn tick_closes_on_take_profit_and_unwatches() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    // +50% over the 2.0 entry clears the 35% TP.
    pipe.on_tick(&tick(MINT, 3.0)).await;

    let portfolio = pipe.portfolio.lock().await;
    assert!(portfolio.position_ids().is_empty());
    assert_eq!(portfolio.history().len(), 1);
    let trade = &portfolio.history()[0];
    assert_eq!(trade.reason, ExitReason::TakeProfit);
    assert!(trade.net_pnl > 0.0);
    drop(portfolio);
    assert_eq!(
        pipe.reserves.unwatched.lock().expect("lock").as_slice(),
        [MINT]
    );
}

#[tokio::test]
async fn zero_liquidity_tick_defers_the_stop_loss() {
    // A drained pool cannot price an exit. The realizable stop must
    // not book a fee-only fiction: the tick is deferred, the bag stays
    // held, and the watch survives.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        PaperPortfolio::new(100.0, 0.0),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // Same breach as `paper_only_stop_fires_on_realizable_and_nets_bleed`
    // (1.87 nets past -8%), but the pool reads empty: hold.
    let mut drained = tick(MINT, 1.87);
    drained.liquidity_usd = 0.0;
    pipe.on_tick(&drained).await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(pipe.reserves.unwatched.lock().expect("lock").is_empty());
    // Liquidity returns: the same price closes, so the hold above was
    // the missing market, not a missing trigger.
    pipe.on_tick(&tick(MINT, 1.87)).await;
    let portfolio = pipe.portfolio.lock().await;
    assert!(portfolio.position_ids().is_empty());
    assert_eq!(portfolio.history()[0].reason, ExitReason::StopLoss);
}

#[test]
fn unpriceable_liquidity_covers_drained_reads() {
    // Ticks carry parsed reserves: zero/NaN means a drained market or
    // an unusable pool, never a missing read. Positive figures price.
    assert!(unpriceable_liquidity(f64::NAN));
    assert!(unpriceable_liquidity(f64::INFINITY));
    assert!(unpriceable_liquidity(0.0));
    assert!(unpriceable_liquidity(-1.0));
    assert!(!unpriceable_liquidity(1.0));
}

#[tokio::test]
async fn zero_liquidity_tick_defers_the_live_exit() {
    // A live-only bag (no paper position) on a drained tick: the curve
    // floor cannot price it, so a dispatch would burn a tip on a sell
    // the chain must refuse. The attempt defers until a priced market
    // returns, then the whole bag converges (nothing to mirror).
    let live = StubLive::working();
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::passing(),
        Some(live.clone()),
    );
    pipe.exec.live_positions.lock().await.insert(
        MINT.to_string(),
        LivePosition {
            mint: pk(MINT),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
            amount_tokens: 1_000,
            creator: None,
        },
    );
    let mut drained = tick(MINT, 1.5);
    drained.liquidity_usd = 0.0;
    pipe.on_tick(&drained).await;
    assert!(
        live.sells().is_empty(),
        "drained tick must not dispatch a sell"
    );
    assert!(pipe.exec.live_positions.lock().await.contains_key(MINT));
    // A priced tick dispatches the full exit (ids-empty path, 1.0).
    pipe.on_tick(&tick(MINT, 1.5)).await;
    assert_eq!(live.sells(), vec![(MINT.to_string(), 1_000)]);
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn pending_buy_pins_the_watch_until_recheck() {
    // A late-confirmed buy is resolved by the dedicated recheck task:
    // unwatching while a buy is pending would strand it unresolved.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // A buy is still awaiting confirmation while the paper side closes
    // (and the mirror sells the tracked bag): nothing else holds the mint.
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(MINT.to_string());
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.reserves.unwatched.lock().expect("lock").is_empty());
    // Pending resolved: the next tick unwatches the empty mint.
    live.inner.pending.lock().expect("lock").clear();
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert_eq!(
        pipe.reserves.unwatched.lock().expect("lock").as_slice(),
        [MINT]
    );
}

#[tokio::test]
async fn tick_never_waits_behind_a_slow_pending_recheck() {
    // AUD-013: the tick path must not run pending-buy rechecks inline
    // (a resolved round can take a 60s confirm deadline). The stub's
    // recheck future hangs forever; if any tick path still awaits it,
    // this tick times out.
    let live = StubLive::working();
    live.inner
        .hang_recheck
        .store(true, std::sync::atomic::Ordering::Relaxed);
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(MINT.to_string());
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        pipe.on_tick(&tick(MINT, 3.0)),
    )
    .await;
    assert!(
        result.is_ok(),
        "a hanging pending recheck must not delay the tick's TP/SL path (AUD-013)"
    );
}

#[tokio::test]
async fn recheck_resolves_confirmed_buys_without_a_tick() {
    // AUD-013: the dedicated recheck task keeps resolving late-confirmed
    // buys with no tick activity — the buy already in `live_positions`
    // (live tracked) must confirm WITHOUT merging into a recheck round
    // from a tick and instead park the manual path.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    // Pending without any tracked position: resolves as Confirmed.
    live.inner
        .recheck_confirms
        .store(true, std::sync::atomic::Ordering::Relaxed);
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(MINT.to_string());
    pipe.recheck_live_pending().await;
    // Tracked now (parity with the old tick-path outcome).
    assert!(
        !pipe.exec.live_positions.lock().await.is_empty(),
        "the confirmed buy is tracked by the recheck path"
    );
    // And the manual park must not have used the dup path: exactly one
    // tracked bag, no manual bag.
    assert!(pipe.exec.manual_keys().await.is_empty());
    // The stub hook drains the pending entry on confirm: the pending
    // buy is fully resolved (tracked as a live bag, no manual park).
    assert_eq!(
        pipe.exec.live_pending_count(),
        0,
        "stub recheck consumed the pending entry on confirm"
    );
}

#[tokio::test]
async fn tick_holds_without_trigger() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    pipe.on_tick(&tick(MINT, 2.1)).await;

    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(pipe.reserves.unwatched.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn tick_closes_other_mints_only() {
    let screener = StubScreener::with_swaps(MINT_A, buyer_swaps(MINT_A, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT_A, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT_A,
        curve: "CURVE_A",
        assoc_curve: "ASSOC_A",
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // Same portfolio, second mint opened directly at the same price.
    {
        let mut portfolio = pipe.portfolio.lock().await;
        portfolio
            .open_position_params(&simulator::OpenPositionParams {
                mint: MINT_B,
                side: Side::Long,
                mark: simulator::Mark::new(2.0, 30_000.0),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: now_secs(),
            })
            .expect("open B");
    }

    pipe.on_tick(&tick(MINT_A, 3.0)).await;

    let portfolio = pipe.portfolio.lock().await;
    let remaining: Vec<String> = portfolio
        .position_ids()
        .into_iter()
        .map(|id| portfolio.position(id).expect("position").mint.clone())
        .collect();
    assert_eq!(remaining, vec![MINT_B.to_string()]);
}

#[tokio::test]
async fn tick_timeout_closes_stale_position() {
    let now = now_secs();
    let mut portfolio = funded();
    portfolio
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 30_000.0),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 60,
            moonbag_percent: 0.0,
            now_secs: now,
        })
        .expect("open");
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::failing(),
        portfolio,
        StubScreener::passing(),
        None,
    );

    let mut timeout_tick = tick(MINT, 2.0);
    timeout_tick.observed_at = now + 61;
    pipe.on_tick(&timeout_tick).await;

    let portfolio = pipe.portfolio.lock().await;
    assert_eq!(portfolio.history()[0].reason, ExitReason::Timeout);
}

#[tokio::test]
async fn tick_migration_forces_manual_close() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    let mut migrated = tick(MINT, 2.0);
    migrated.complete = true;
    pipe.on_tick(&migrated).await;

    let portfolio = pipe.portfolio.lock().await;
    assert_eq!(portfolio.history()[0].reason, ExitReason::Manual);
    assert!(portfolio.position_ids().is_empty());
}

#[tokio::test]
async fn live_buy_fires_with_paper_open() {
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    assert_eq!(
        live.buys(),
        vec![(MINT.to_string(), CURVE.to_string(), ASSOC.to_string(), 10.0)]
    );
    let tracked = pipe.exec.live_positions.lock().await;
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[MINT].amount_tokens, 1_000);
}

#[tokio::test]
async fn live_buy_failure_keeps_paper_position() {
    let live = StubLive::working();
    live.inner
        .fail_buys
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    // Paper record survives; nothing tracked for exits, but the
    // divergence is explicit (paper WITHOUT live cover).
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.exec.live_diverged.lock().await.contains_key(MINT));
    assert!(pipe.live_tracked_mints().await.contains(&MINT.to_string()));
}

#[tokio::test]
async fn live_sell_fires_on_paper_close() {
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;

    assert_eq!(live.sells(), vec![(MINT.to_string(), 1_000)]);
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    // No pool leg: the Raydium leg stays silent.
    assert!(live.raydium_sells().is_empty());
}

#[tokio::test]
async fn paper_close_stays_deferred_while_a_live_buy_is_pending() {
    // AUD-008: TP close while the live buy for the mint is still
    // pending — `live_sell` reports unsettled, the bag is unsold and
    // the PnL stays deferred (no breaker input before the outcome).
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // Window under fix: the live buy has a receipt but is NOT yet
    // chain-confirmed — untracked, pending only. Clear the stub's
    // immediate tracking to reproduce that state exactly.
    pipe.exec.live_positions.lock().await.clear();
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(MINT.to_string());
    pipe.on_tick(&tick(MINT, 3.0)).await;
    let hist = pipe.portfolio.lock().await.history().len();
    let deferred = pipe.exec.deferred_pnl.lock().await.contains_key(MINT);
    dbg!(hist, deferred);
    assert!(live.sells().is_empty(), "no exit while the buy is pending");
    assert!(
        pipe.exec.deferred_pnl.lock().await.contains_key(MINT),
        "paper PnL remains stashed, never flushed as settled"
    );
}

#[tokio::test]
async fn exposure_counts_each_unpaired_live_bag() {
    // AUD-014: duplicate bags on one mint are separate live tickets.
    // live mirror state: 2 tracked bags (one suffixed dup), 1 parked
    // manual dup, and a pending receipt on the same mint.
    let live = StubLive::working();
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        StubScreener::passing(),
        Some(live.clone()),
    );
    let bag = LivePosition {
        mint: pk(MINT),
        curve: pk(CURVE),
        assoc_curve: pk(ASSOC),
        amount_tokens: 1_000,
        creator: None,
    };
    pipe.exec
        .live_positions
        .lock()
        .await
        .insert(MINT.to_string(), bag.clone());
    pipe.exec
        .live_positions
        .lock()
        .await
        .insert(format!("{MINT}#2-deadbeef"), bag.clone());
    pipe.exec
        .live_manual
        .lock()
        .await
        .insert(format!("{MINT}#late-12345678"), bag.clone());
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(MINT.to_string());
    // No paper anywhere: every bag is unpaired (the pending ride is a
    // bag on a mint with bags underneath: ticket count unchanged).
    let count = pipe
        .exec
        .live_unpaired_bag_count(&std::collections::HashSet::new())
        .await;
    assert_eq!(count, 3, "three bags, not one mint");
    // One paper position on that mint pairs exactly one bag.
    pipe.portfolio
        .lock()
        .await
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: now_secs(),
        })
        .expect("paper open");
    let mut paper = std::collections::HashSet::new();
    paper.insert(MINT.to_string());
    let count = pipe.exec.live_unpaired_bag_count(&paper).await;
    assert_eq!(count, 2, "one bag pairs off the paper position");
}

#[tokio::test]
async fn exposure_cap_counts_duplicate_bags_before_opening() {
    // Gate regression: with two live dups already held on the candidate
    // mint, the dollar cap must count two tickets, not one distinct
    // mint (10 paper + 2 x 10 = 30 deployed; a 10 ticket would hit 40).
    async fn seed(pipe: &Pipeline<StubAuditor, StubReserves, StubLive, StubScreener, StubAlerts>) {
        pipe.portfolio
            .lock()
            .await
            .open_position_params(&simulator::OpenPositionParams {
                mint: MINT_A,
                side: Side::Long,
                mark: simulator::Mark::new(2.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: now_secs(),
            })
            .expect("manual paper ticket");
        let bag = LivePosition {
            mint: pk(MINT),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
            amount_tokens: 1_000,
            creator: None,
        };
        pipe.exec
            .live_positions
            .lock()
            .await
            .insert(MINT.to_string(), bag.clone());
        pipe.exec
            .live_positions
            .lock()
            .await
            .insert(format!("{MINT}#2-deadbeef"), bag);
    }
    let build = |cap: f64| {
        let mut settings = Settings::from_map(&HashMap::new());
        settings.notional_usd = 10.0;
        settings.max_exposure_usd = cap;
        pipeline_screened(
            settings,
            AuditOutcome::Allow(100),
            StubReserves::with_tick(tick(MINT, 2.0)),
            funded(),
            StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15])),
            Some(StubLive::working()),
        )
    };
    let dev = dev_wallet();
    let launch = LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev,
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    };
    let pipe = build(35.0);
    seed(&pipe).await;
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().len() == 1,
        "cap 35: 30 deployed from three tickets + a fourth would hit 40 — no open"
    );
    let pipe = build(45.0);
    seed(&pipe).await;
    pipe.on_launch(&launch).await;
    pipe.poll_gates().await;
    assert!(
        pipe.portfolio.lock().await.position_ids().len() == 2,
        "cap 45 admits the fourth ticket"
    );
}

#[tokio::test]
async fn live_sell_routes_raydium_when_pool_known() {
    // Same close, but a validated pool leg is stored: the exit must
    // take the Raydium leg (curve leg untouched) with the pool's amm.
    use crate::follow::{FollowStage, FollowState};
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.followed.lock().await.insert(
        MINT.to_string(),
        FollowState {
            stage: FollowStage::Watching { entry_price: 2.0 },
            last_seen_secs: now_secs(),
            pool: Some(PoolLeg {
                mint: pk(MINT),
                amm: crate::live::test_pubkey(50),
                base_vault: crate::live::test_pubkey(51),
                quote_vault: crate::live::test_pubkey(52),
                venue: PoolVenue::RaydiumV4,
            }),
        },
    );
    pipe.on_tick(&tick(MINT, 3.0)).await;

    assert!(live.sells().is_empty());
    assert_eq!(
        live.raydium_sells(),
        vec![(
            MINT.to_string(),
            crate::live::test_pubkey(50).to_string(),
            1_000
        )]
    );
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn live_sell_routes_pumpswap_when_pool_known() {
    // Same close, but the leg is a PumpSwap pool: the exit must
    // take the PumpSwap leg (curve and Raydium legs untouched).
    use crate::follow::{FollowStage, FollowState};
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.followed.lock().await.insert(
        MINT.to_string(),
        FollowState {
            stage: FollowStage::Watching { entry_price: 2.0 },
            last_seen_secs: now_secs(),
            pool: Some(PoolLeg {
                mint: pk(MINT),
                amm: crate::live::test_pubkey(50),
                base_vault: crate::live::test_pubkey(51),
                quote_vault: crate::live::test_pubkey(52),
                venue: PoolVenue::PumpSwap,
            }),
        },
    );
    pipe.on_tick(&tick(MINT, 3.0)).await;

    assert!(live.sells().is_empty());
    assert!(live.raydium_sells().is_empty());
    assert_eq!(
        live.pumpswap_sells(),
        vec![(
            MINT.to_string(),
            crate::live::test_pubkey(50).to_string(),
            1_000
        )]
    );
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn paper_only_stop_fires_on_realizable_and_nets_bleed() {
    // No live leg: exits price the fee-aware fill (M1) and the
    // ledger nets dispatch bleed (M3). Default 125bps/side.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        PaperPortfolio::new(100.0, 0.0),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    // -7.7% gross: a gross-mark stop holds here, but the fill nets
    // past -8% (125bps exit cost), so the realizable stop fires.
    pipe.on_tick(&tick(MINT, 1.87)).await;
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
    // Breaker sees edge net of BOTH legs: the open leg booked at
    // open, the close leg netted at close.
    let day_net = pipe.daily.lock().await.day_net();
    assert!((day_net - (trade.net_pnl - trade.dispatch_usd - open_leg)).abs() < 1e-6);
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
        .open_position_params(&simulator::OpenPositionParams {
            mint: sys,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: opened_now,
        })
        .expect("manual open");
    pipe.exec.live_positions.lock().await.insert(
        sys.to_string(),
        LivePosition {
            mint: pk(sys),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
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
async fn manual_sell_waits_while_a_live_buy_is_pending() {
    // AUD-007: `sell <mint>` during the unresolved LIVE-buy window must
    // NOT settle — the request is kept so the daemon drain retries
    // after the pending recheck confirms (and then sells the bag).
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
    live.inner
        .pending
        .lock()
        .expect("lock")
        .push(sys.to_string());
    assert!(
        !pipe.manual_sell(sys).await,
        "unsettled while the live buy for this mint is pending"
    );
    // The pending buy confirms through the recheck path: the bag is
    // tracked and the retained request sells it on the next drain.
    live.inner
        .recheck_confirms
        .store(true, std::sync::atomic::Ordering::Relaxed);
    pipe.recheck_live_pending().await;
    assert!(!pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.manual_sell(sys).await);
    assert_eq!(live.sells(), vec![(sys.to_string(), 1_000)]);
}

#[tokio::test]
async fn manual_sell_unknown_mint_settles() {
    // Nothing tracked anywhere: settle at once so the request file
    // goes away instead of retrying forever.
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
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
            mint: pk(sys),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
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
async fn manual_sell_drains_suffixed_dup_bags() {
    // A second fill that lands while the mint is tracked parks under
    // `{mint}#2-…` (suffixed keys are not valid `sell` targets
    // themselves): one `sell MINT` must drain the backlog, not strand
    // it until a code change.
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
        format!("{sys}#2-deadbeef"),
        LivePosition {
            mint: pk(sys),
            curve: pk(CURVE),
            assoc_curve: pk(ASSOC),
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn alerts_fire_on_open_and_close() {
    let alerts = StubAlerts::default();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_alerted(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
        Some(alerts.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;

    let cards = alerts.messages();
    assert_eq!(cards.len(), 2);
    assert!(cards[0].contains("[TRADE OPENED]"));
    assert!(cards[0].contains(&format!("`{MINT}`")));
    assert!(cards[1].contains("[POSITION CLOSED]"));
    assert!(cards[1].contains("TakeProfit"));
    assert!(cards[1].contains(&format!("`{MINT}`")));
}

#[tokio::test]
async fn alerts_fire_on_live_dispatch() {
    let alerts = StubAlerts::default();
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_alerted(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live),
        Some(alerts.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;

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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &buyers));
    let pipe = pipeline_screened(
        settings(&[("STRATEGY_MODE", "scalp")]),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // +40% clears the clamped 35% TP: full exit, nothing rides.
    pipe.on_tick(&tick(MINT, 2.8)).await;
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &buyers));
    let pipe = pipeline_screened(
        settings(&[("STRATEGY_MODE", "runner")]),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // +40% clears TP 35: 80% out, 20% runner with trailing armed
    // and the 90s time-stop lifted (checked directly: position
    // timeouts run on wall-clock, which tests cannot fast-forward).
    pipe.on_tick(&tick(MINT, 2.8)).await;
    {
        let portfolio = pipe.portfolio.lock().await;
        assert_eq!(portfolio.history()[0].fraction_closed, 0.8);
        let runner = portfolio
            .position(portfolio.position_ids()[0])
            .expect("runner");
        assert!(runner.trailing_stop);
        assert_eq!(runner.timeout_seconds, simulator::RUNNER_BACKSTOP_SECS);
        assert_eq!(runner.moonbag_percent, 0.0);
        // Runner trail is the wide one (20%), not the shared 5%.
        assert_eq!(runner.trail_percent, 20.0);
    }
    // Drifting inside the trail band: no exit fires either.
    pipe.on_tick(&tick(MINT, 2.68)).await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
}

#[tokio::test]
async fn tp_scales_out_moonbag_then_rides() {
    // High-momentum confirm carries moonbag 20: TP exits 80%.
    let buyers: Vec<u8> = (20..55).collect();
    let alerts = StubAlerts::default();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &buyers));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_alerted(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
        Some(alerts.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    // +70% clears the 60% TP: 80% out, 20% runner stays armed-off.
    pipe.on_tick(&tick(MINT, 3.4)).await;

    let portfolio = pipe.portfolio.lock().await;
    assert_eq!(portfolio.history().len(), 1);
    assert_eq!(portfolio.history()[0].fraction_closed, 0.8);
    let id = portfolio.position_ids()[0];
    let runner = portfolio.position(id).expect("runner");
    assert_eq!((runner.quantity, runner.entry_notional), (1, 2.0));
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
    pipe.on_tick(&tick(MINT, 4.0)).await;
    let portfolio = pipe.portfolio.lock().await;
    assert_eq!(portfolio.history().len(), 2);
    assert_eq!(portfolio.history()[1].fraction_closed, 1.0);
    assert!(portfolio.position_ids().is_empty());
    drop(portfolio);
    assert_eq!(
        pipe.reserves.unwatched.lock().expect("lock").as_slice(),
        [MINT]
    );
}

#[tokio::test]
async fn live_sell_mirrors_partial_fractions() {
    let buyers: Vec<u8> = (20..55).collect();
    let alerts = StubAlerts::default();
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &buyers));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_alerted(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
        Some(alerts.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    // Tracked 1000 base units: TP sells 800, runner keeps 200 tracked.
    pipe.on_tick(&tick(MINT, 3.4)).await;
    assert_eq!(live.sells(), vec![(MINT.to_string(), 800)]);
    assert_eq!(
        pipe.exec.live_positions.lock().await[MINT].amount_tokens,
        200
    );

    // Success resets the failure budget: the runner exit fires on the
    // very next tick with no cooldown workaround (previously the
    // success itself stamped the cooldown and ate the budget).
    assert!(!pipe.exec.live_retry.lock().await.contains_key(MINT));
    pipe.on_tick(&tick(MINT, 4.0)).await;
    assert_eq!(
        live.sells(),
        vec![(MINT.to_string(), 800), (MINT.to_string(), 200)]
    );
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn silenced_pipeline_sends_nothing() {
    // No alerts attached: firing paths are skipped without error.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_alerted(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        None,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert_eq!(pipe.portfolio.lock().await.history().len(), 1);
}

#[tokio::test]
async fn heat_cap_blocks_extra_positions() {
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        settings(&[("MAX_OPEN_POSITIONS", "1")]),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        None,
    );
    // Fill the single slot directly, then confirm must skip.
    {
        let mut portfolio = pipe.portfolio.lock().await;
        portfolio
            .open_position_params(&simulator::OpenPositionParams {
                mint: "OTHER",
                side: Side::Long,
                mark: simulator::Mark::new(2.0, 30_000.0),
                notional_usd: 10.0,
                tp_percent: 35.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: now_secs(),
            })
            .expect("open");
    }
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
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
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let mut portfolio = funded();
    portfolio
        .open_position_params(&simulator::OpenPositionParams {
            mint: MINT,
            side: Side::Long,
            mark: simulator::Mark::new(2.0, 30_000.0),
            notional_usd: 10.0,
            tp_percent: 35.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: now_secs(),
        })
        .expect("open");
    let id = portfolio.position_ids()[0];
    portfolio
        .close_position(
            id,
            &simulator::Mark::new(2.0, 30_000.0),
            ExitReason::Manual,
            now_secs(),
        )
        .expect("close");
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        portfolio,
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
}

#[tokio::test]
async fn live_sell_retries_on_cooldown_then_gives_up() {
    use std::sync::atomic::Ordering;
    let live = StubLive::working();
    live.inner
        .fail_sells_transient
        .store(true, Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // First close attempt dispatches (and fails); immediate retries
    // are skipped by the cooldown.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(live.sells().is_empty());
    let attempts = pipe.exec.live_retry.lock().await;
    assert_eq!(attempts.get(MINT), Some(&(1u32, attempts[MINT].1)));
    drop(attempts);

    // Pre-seed attempts at the cap with a stale timestamp: next tick
    // parks to manual (still HELD) and stops auto-probing the mint.
    {
        let mut retry = pipe.exec.live_retry.lock().await;
        retry.insert(MINT.to_string(), (5u32, 0u64));
    }
    // Paper already closed; drive another tick to trigger the retry path.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.exec.live_manual.lock().await.contains_key(MINT));
    assert!(pipe.live_tracked_mints().await.contains(&MINT.to_string()));
    assert!(live.sells().is_empty());
    // Give-up parks the bag manual (still HELD): the paper profit
    // stashed while holding stays deferred until the manual exit
    // clears — never flushed into the breaker while live holds.
    assert!(pipe.exec.deferred_pnl.lock().await.contains_key(MINT));
    assert_eq!(pipe.daily.lock().await.day_net(), 0.0);
}

#[tokio::test]
async fn sell_unknown_dispatch_parks_without_resend() {
    // AUD-002: a sell dispatch that times out with no receipt is
    // unknown custody — the mint parks for manual review on the first
    // failure (no retry budget spent), and the next tick must NOT
    // resend: a landed-but-unseen bundle plus a resend would
    // double-spend one bag.
    use std::sync::atomic::Ordering;
    let live = StubLive::working();
    live.inner.fail_sells_unknown.store(true, Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // Paper closes: the stubbed sell reports unknown dispatch.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    // Parked manual (still HELD), tracking dropped, no retry budget
    // spent — this was not a refusal, it was an ambiguity.
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.exec.live_manual.lock().await.contains_key(MINT));
    assert!(pipe.exec.live_retry.lock().await.get(MINT).is_none());
    assert_eq!(live.inner.started.lock().expect("lock").len(), 1);
    // Next tick: manual hold, nothing tracked — no resend dispatched.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert_eq!(live.inner.started.lock().expect("lock").len(), 1);
    assert!(pipe.exec.live_manual.lock().await.contains_key(MINT));
    assert!(pipe.exec.live_positions.lock().await.is_empty());
}

#[tokio::test]
async fn sell_rate_limited_retries_instead_of_parking() {
    // AUD-002: a Jito HTTP 429 is a deterministic rejection — the
    // engine answered without accepting, so the sell must consume the
    // retry budget (note_failure + cooldown retry) instead of parking
    // the bag as unknown custody exactly under congestion.
    use std::sync::atomic::Ordering;
    let live = StubLive::working();
    live.inner
        .fail_sells_rate_limited
        .store(true, Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;
    // NOT parked: the bag stays tracked and the retry budget spent one
    // attempt. Only ambiguous transport failures may park.
    assert!(
        !pipe.exec.live_manual.lock().await.contains_key(MINT),
        "a 429 refusal must never create a custody claim"
    );
    assert!(pipe.exec.live_positions.lock().await.contains_key(MINT));
    let attempts = pipe.exec.live_retry.lock().await;
    assert_eq!(
        attempts.get(MINT).map(|(n, _)| *n),
        Some(1u32),
        "refusal consumes one retry attempt"
    );
    drop(attempts);
    // Cooldown holds the immediate retry, but the paper close stays
    // deferred (unsettled) exactly like any other retryable failure.
    assert!(pipe.exec.deferred_pnl.lock().await.contains_key(MINT));
}

#[tokio::test]
async fn giveup_on_close_tick_defers_breaker_feed() {
    // G2 regression: when the paper close and the GiveUp land on the
    // SAME tick, the breaker must still not see model PnL for a bag
    // that never exited. Pre-seeded attempts push this tick's failing
    // sell straight to GiveUp.
    use std::sync::atomic::Ordering;
    let live = StubLive::working();
    live.inner
        .fail_sells_transient
        .store(true, Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    {
        let mut retry = pipe.exec.live_retry.lock().await;
        retry.insert(MINT.to_string(), (4u32, 0u64));
    }
    // TP closes paper AND the sell exhausts the budget on this tick.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert!(pipe.exec.live_manual.lock().await.contains_key(MINT));
    assert!(pipe.exec.deferred_pnl.lock().await.contains_key(MINT));
    assert_eq!(pipe.daily.lock().await.day_net(), 0.0);
}

#[tokio::test]
async fn deferred_pnl_flushes_when_live_settles_late() {
    // Paper closes while live still holds: the breaker must NOT see
    // the profit yet, and must see it once the late sell lands.
    use std::sync::atomic::Ordering;
    let live = StubLive::working();
    live.inner
        .fail_sells_transient
        .store(true, Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    // Tick 1: TP closes paper, live sell fails transiently.
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(live.sells().is_empty());
    assert!(pipe.portfolio.lock().await.position_ids().is_empty());
    assert_eq!(pipe.daily.lock().await.day_net(), 0.0);
    assert!(pipe.exec.deferred_pnl.lock().await.contains_key(MINT));
    // Recover with a stale cooldown: the no-paper retry path sells
    // and the stashed profit settles into the breaker.
    live.inner
        .fail_sells_transient
        .store(false, Ordering::Relaxed);
    {
        let mut retry = pipe.exec.live_retry.lock().await;
        retry.insert(MINT.to_string(), (1u32, 0u64));
    }
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert_eq!(live.sells(), vec![(MINT.to_string(), 1_000)]);
    assert!(!pipe.exec.deferred_pnl.lock().await.contains_key(MINT));
    assert!(pipe.daily.lock().await.day_net() > 0.0);
}

#[tokio::test]
async fn live_sell_escalates_tip_with_attempts() {
    // Two prior failures: the next dispatch carries 3x base tip
    // (bounded congestion response), recorded per sell.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    {
        let mut retry = pipe.exec.live_retry.lock().await;
        retry.insert(MINT.to_string(), (2u32, 0u64));
    }
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert_eq!(live.sell_tips(), vec![300_000]);
    assert_eq!(live.sells(), vec![(MINT.to_string(), 1_000)]);
}

#[tokio::test]
async fn live_sell_deterministic_reject_is_bounded_then_manual() {
    // Pre-dispatch rejects (bad order, dust) re-probe at the cooldown
    // cadence but bounded: after the 5-attempt budget the bag parks
    // to manual instead of holding the feed forever.
    use std::sync::atomic::Ordering;
    let live = StubLive::working();
    live.inner.fail_sells.store(true, Ordering::Relaxed);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT, 2.0)),
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(live.sells().is_empty());
    let retry = pipe.exec.live_retry.lock().await;
    assert_eq!(retry.get(MINT).map(|(a, _)| *a), Some(1));
    drop(retry);
    // Still tracked for later re-probes (not yet given up).
    assert!(pipe.exec.live_positions.lock().await.contains_key(MINT));
    // Pre-seed at the cap with a stale timestamp: next tick parks to
    // manual (still HELD) instead of retrying forever.
    {
        let mut retry = pipe.exec.live_retry.lock().await;
        retry.insert(MINT.to_string(), (5u32, 0u64));
    }
    pipe.on_tick(&tick(MINT, 3.0)).await;
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.exec.live_manual.lock().await.contains_key(MINT));
    assert!(pipe.live_tracked_mints().await.contains(&MINT.to_string()));
}

#[tokio::test]
async fn live_sells_for_different_mints_overlap() {
    // Per-mint serialization proof: a sell blocked mid-dispatch on
    // mint A must not hold up mint B. Under the old global lock this
    // deadlocks (B waits for A's lock while A waits for release) and
    // the timeouts fail the test instead of hanging CI.
    let live = StubLive::working();
    *live.inner.block_mint.lock().expect("lock") = Some(MINT_A.to_string());
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        StubReserves::with_tick(tick(MINT_A, 2.0)),
        funded(),
        StubScreener::passing(),
        Some(live.clone()),
    );
    for mint in [MINT_A, MINT_B] {
        pipe.exec.live_positions.lock().await.insert(
            mint.to_string(),
            LivePosition {
                mint: pk(mint),
                curve: pk(CURVE),
                assoc_curve: pk(ASSOC),
                amount_tokens: 1_000,
                creator: None,
            },
        );
    }
    let secs = std::time::Duration::from_secs;
    tokio::time::timeout(secs(10), async {
        tokio::join!(
            async {
                tokio::time::timeout(
                    secs(5),
                    pipe.live_sell(MINT_A, &simulator::Mark::new(3.0, 30_000.0), 1.0),
                )
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
                            .any(|m| m == MINT_A)
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("A starts");
                tokio::time::timeout(
                    secs(5),
                    pipe.live_sell(MINT_B, &simulator::Mark::new(3.0, 30_000.0), 1.0),
                )
                .await
                .expect("B proceeds while A is blocked");
                live.inner.unblock.notify_waiters();
            }
        )
    })
    .await
    .expect("no deadlock");
    assert!(live.sells().iter().any(|(m, _)| m == MINT_A));
    assert!(live.sells().iter().any(|(m, _)| m == MINT_B));
}

#[tokio::test]
async fn live_mirror_skipped_when_watch_fails() {
    // Exit-feed outage: paper still opens (manual review), but no
    // real bag is bought without TP/SL automation.
    let live = StubLive::working();
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let mut reserves = StubReserves::with_tick(tick(MINT, 2.0));
    reserves.fail_watch = true;
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(live.buys().is_empty());
}

/// Launch-scale curve state (1.073e15 virtual tokens, 793.1e12 real,
/// 6 decimals; `vq` is the virtual SOL side in lamports).
fn launch_reserves(vq: u64) -> curve::BondingCurveReserves {
    curve::BondingCurveReserves {
        virtual_token_reserves: 1_073_000_000_000_000,
        virtual_sol_reserves: vq,
        real_token_reserves: 793_100_000_000_000,
        real_sol_reserves: 0,
        token_total_supply: 1_000_000_000_000_000,
        complete: false,
    }
}

/// The same state as the simulator sees it (stub feed: SOL $150).
fn launch_mark(vq: u64) -> simulator::CurveReserves {
    crate::live::curve_reserves(&curve::ExecState {
        reserves: launch_reserves(vq),
        decimals: 6,
        sol_usd: 150.0,
    })
}

/// A tick carrying exact reserves, priced at the reserves' own spot.
fn curve_tick(mint: &str, vq: u64) -> PriceTick {
    let reserves = launch_reserves(vq);
    PriceTick {
        mint: mint.to_string(),
        curve: CURVE.to_string(),
        price_usd: reserves.price_usd(6, 150.0).expect("priced"),
        liquidity_usd: reserves.liquidity_usd(150.0),
        complete: false,
        exec: Some(curve::ExecState {
            reserves,
            decimals: 6,
            sol_usd: 150.0,
        }),
        observed_at: now_secs(),
    }
}

#[tokio::test]
async fn curve_tick_books_exact_state_based_fills() {
    // A tick carrying parsed reserves prices the paper fill from the
    // chain's own math (size impact + venue fee inside the price), and
    // the exit books the same exact sell — never a proxy estimate.
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(curve_tick(MINT, 200_000_000_000));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        PaperPortfolio::new(100.0, 0.0),
        screener,
        None,
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    pipe.poll_gates().await;

    let expected = launch_mark(200_000_000_000)
        .buy_fill(10.0, VENUE_FEE_BPS as f64)
        .expect("priced");
    let (quantity, entry_price) = {
        let pf = pipe.portfolio.lock().await;
        let id = pf.position_ids()[0];
        let p = pf.position(id).expect("open");
        (p.quantity, p.entry_price)
    };
    assert_eq!(quantity, expected.quantity, "exact curve size");
    assert!((entry_price - expected.price).abs() < 1e-15);

    // The curve doubles (virtual SOL 200 -> 400): the take-profit clears
    // and the booked exit prices the exact sell for the sold size.
    pipe.on_tick(&curve_tick(MINT, 400_000_000_000)).await;
    let pf = pipe.portfolio.lock().await;
    assert_eq!(pf.history().len(), 1);
    let trade = &pf.history()[0];
    assert_eq!(trade.reason, ExitReason::TakeProfit);
    assert!(trade.quantity > 0 && trade.quantity <= quantity);
    let moved = launch_mark(400_000_000_000);
    let expected_exit = moved
        .sell_fill_price(trade.quantity, VENUE_FEE_BPS as f64)
        .expect("priced");
    assert!(
        (trade.exit_price - expected_exit).abs() < 1e-15,
        "exit={} expected={expected_exit}",
        trade.exit_price
    );
    // The exit nets below the moved mark: impact + venue fee.
    let spot = launch_reserves(400_000_000_000)
        .price_usd(6, 150.0)
        .expect("priced");
    assert!(
        trade.exit_price < spot,
        "exit={} spot={spot}",
        trade.exit_price
    );
}

#[tokio::test]
async fn dead_on_chain_buy_debits_the_actual_burn() {
    // A buy that landed and errored burned base + priority on-chain
    // (the tip rolled back); the paper book and the day-loss gate must
    // book that burn, or the sim looks free exactly when it failed.
    let live = StubLive::working();
    *live.inner.dispatch_lamports.lock().expect("lock") = 100_000;
    live.inner
        .dead_buys
        .lock()
        .expect("lock")
        .replace(DeadReason::FailedOnChain);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), pipe.poll_gates())
        .await
        .expect("live buy path must complete");

    // Paper position survives, diverged (no live cover)...
    assert_eq!(pipe.portfolio.lock().await.position_ids().len(), 1);
    assert!(pipe.exec.live_positions.lock().await.is_empty());
    assert!(pipe.exec.live_diverged.lock().await.contains_key(MINT));
    // ...and the failed dispatch's burn is debited (stub cost at SOL
    // $150), never the first-touch cost of a fill that never happened.
    let burn = live.charge_lamports() as f64 / LAMPORTS_PER_SOL * 150.0;
    let pf = pipe.portfolio.lock().await;
    assert!(
        (pf.total_dispatch_usd() - burn).abs() < 1e-12,
        "bleed={}",
        pf.total_dispatch_usd()
    );
    drop(pf);
    assert!((pipe.daily.lock().await.day_net() + burn).abs() < 1e-12);
}

#[tokio::test]
async fn never_forwarded_buy_debits_nothing() {
    // A bundle the engine never forwarded (or an expired one) moved no
    // funds and paid no fees: nothing to debit.
    let live = StubLive::working();
    *live.inner.dispatch_lamports.lock().expect("lock") = 100_000;
    live.inner
        .dead_buys
        .lock()
        .expect("lock")
        .replace(DeadReason::NeverForwarded);
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), pipe.poll_gates())
        .await
        .expect("live buy path must complete");
    assert!(pipe.exec.live_diverged.lock().await.contains_key(MINT));
    // The knob is nonzero: a debit here would be visible, and there is
    // none (nothing moved, nothing was charged).
    assert_eq!(pipe.portfolio.lock().await.total_dispatch_usd(), 0.0);
    assert_eq!(pipe.daily.lock().await.day_net(), 0.0);
}

#[tokio::test]
async fn live_buy_first_touch_debit_completes() {
    // The success arm debits the first-touch cost while the buy match
    // is still in scope. A tier read taken inside the match scrutinee
    // keeps the book lock held through every arm, so that debit would
    // deadlock the pipeline (custody recorded, nothing settles). The
    // timeout turns a reintroduced hang into a failure.
    let live = StubLive::working();
    *live.inner.dispatch_lamports.lock().expect("lock") = 100_000;
    let screener = StubScreener::with_swaps(MINT, buyer_swaps(MINT, &[11, 12, 13, 14, 15]));
    let reserves = StubReserves::with_tick(tick(MINT, 2.0));
    let pipe = pipeline_screened(
        Settings::from_map(&HashMap::new()),
        AuditOutcome::Allow(100),
        reserves,
        funded(),
        screener,
        Some(live.clone()),
    );
    pipe.on_launch(&LaunchRequest {
        mint: MINT,
        curve: CURVE,
        assoc_curve: ASSOC,
        dev: &dev_wallet(),
        creator: None,
        name: "TestToken",
        symbol: "TEST",
        slot: 5000,
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), pipe.poll_gates())
        .await
        .expect("live buy path must complete");
    assert_eq!(live.buys().len(), 1);
    assert_eq!(pipe.exec.live_positions.lock().await.len(), 1);
    let burn = live.charge_lamports() as f64 / LAMPORTS_PER_SOL * 150.0;
    let pf = pipe.portfolio.lock().await;
    assert!(
        (pf.total_dispatch_usd() - burn).abs() < 1e-12,
        "bleed={}",
        pf.total_dispatch_usd()
    );
}
