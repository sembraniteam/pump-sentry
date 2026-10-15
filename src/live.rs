//! Live sizing plus Jito dispatch. Plans are pure; trader hits network.

use crate::settings::now_secs;
use crate::wal;
use executor::{BuyOrder, BundleStatus, ConfirmOutcome, ExecutorError, JitoReceipt, SellOrder, SwapExecutor, parse_pubkey};
use ids::{ATA_RENT_LAMPORTS, BASE_TX_FEE_LAMPORTS};
use simulator::{FillModel, Side};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Mint decimals. Confirm verifies 6; else drop.
/// Single source is `curve::PUMP_TOKEN_DECIMALS`.
pub const TOKEN_DECIMALS: u32 = curve::PUMP_TOKEN_DECIMALS as u32;
/// Lamports per SOL. Single source is `curve::layout::LAMPORTS_PER_SOL`.
pub const LAMPORTS_PER_SOL: f64 = curve::layout::LAMPORTS_PER_SOL;
/// Standard fee tier (bps/side). Single source is
/// `simulator::STANDARD_VENUE_FEE_BPS` (f64 domain there).
pub const VENUE_FEE_BPS: u64 = simulator::STANDARD_VENUE_FEE_BPS as u64;
/// Basis-point denominator shared by fee and slippage math.
/// Single source is `executor::quote::BPS_DENOMINATOR` (u128 domain there).
pub const BPS_DENOMINATOR: u64 = executor::quote::BPS_DENOMINATOR as u64;

/// Mark-price trail per mint for volatility-adaptive slippage.
type MarkTrail = std::sync::Arc<std::sync::Mutex<HashMap<String, VecDeque<(u64, f64)>>>>;

/// Live-trade failures.
#[derive(Error, Debug)]
pub enum LiveError {
    /// Bad inputs: {0}
    #[error("bad live order: {0}")]
    BadOrder(String),
    /// Dispatch failed: {0}
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] ExecutorError),
    /// Accepted but never confirmed. `Confirmed failure` means no fill.
    #[error("unconfirmed live fill: {0}")]
    Unconfirmed(String),
    /// Timed out with no signature: fill unknown, needs review.
    /// Boxed for `clippy::result_large_err`.
    #[error("unknown dispatch fill: {reason}")]
    UnknownDispatch {
        position: Box<LivePosition>,
        reason: String,
    },
}

/// Confirm deadline per dispatch.
pub const LIVE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// Dispatch deadline. Timeout means unknown fill, needs review.
pub const LIVE_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Min spacing between sell retries per mint.
pub const SELL_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
/// Failed sells before manual exit.
pub const SELL_MAX_ATTEMPTS: u32 = 5;
/// Recheck rounds before manual review.
pub const PENDING_MAX_ATTEMPTS: u32 = 5;

/// Post-migration pool venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolVenue {
    /// Raydium AMM V4 (legacy venue).
    RaydiumV4,
    /// PumpSwap AMM (live venue).
    PumpSwap,
}

/// Pool leg for one mint. Vault orientation is WSOL-side-first on
/// both venues (BASE holds WSOL, QUOTE the token).
#[derive(Debug, Clone)]
pub struct PoolLeg {
    pub mint: String,
    pub amm: String,
    /// Coin vault (WSOL).
    pub base_vault: String,
    /// Pc vault (token).
    pub quote_vault: String,
    /// Routing venue.
    pub venue: PoolVenue,
}

/// Tracked fill (base units).
#[derive(Debug, Clone)]
pub struct LivePosition {
    pub mint: String,
    pub curve: String,
    pub assoc_curve: String,
    /// Bought units (for full exits).
    pub amount_tokens: u64,
    /// Creator. `None` fails v3 dispatch.
    pub creator: Option<String>,
}

/// Buy request.
#[derive(Debug, Clone, Copy)]
pub struct LiveBuyParams<'a> {
    pub mint: &'a str,
    pub curve: &'a str,
    pub assoc_curve: &'a str,
    /// Exposure in USD.
    pub notional_usd: f64,
    /// Price in USD per token.
    pub price_usd: f64,
    /// SOL price in USD.
    pub sol_usd: f64,
    /// Curve liquidity in USD (zero pays fee-only).
    pub liquidity_usd: f64,
    /// Fee tier. `None` is standard.
    pub venue_fee_bps: Option<f64>,
    pub creator: Option<&'a str>,
}

/// Planned buy: amounts ready for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBuyPlan {
    pub amount_tokens: u64,
    /// Slippage guard in lamports.
    pub max_sol_cost: u64,
}

/// Curve-leg sell request. Bundled like buys (arity never grows).
#[derive(Debug, Clone, Copy)]
pub struct LiveSellParams<'a> {
    pub position: &'a LivePosition,
    pub price_usd: f64,
    pub sol_usd: f64,
    pub liquidity_usd: f64,
    /// Fee tier. `None` is standard.
    pub venue_fee_bps: Option<f64>,
    /// Jito tip (congestion-aware; caller computes via `effective_tip`).
    pub tip_lamports: u64,
}

/// Raydium-leg sell request. Floor prices from vault reserves, so no
/// USD inputs cross here (the curve print is gone on this path).
#[derive(Debug, Clone, Copy)]
pub struct RaydiumSellParams<'a> {
    pub position: &'a LivePosition,
    pub pool: &'a PoolLeg,
    /// Jito tip (congestion-aware; caller computes via `effective_tip`).
    pub tip_lamports: u64,
}

/// PumpSwap-leg sell request. Floor quotes from live reserves plus
/// the fee schedule, so no USD inputs cross here either.
#[derive(Debug, Clone, Copy)]
pub struct PumpSwapSellParams<'a> {
    pub position: &'a LivePosition,
    pub pool: &'a PoolLeg,
    /// Jito tip (congestion-aware; caller computes via `effective_tip`).
    pub tip_lamports: u64,
}

/// Max tip multiplier on sell retries. Congestion response is bounded:
/// a stuck bag must not burn the wallet, and floors never widen
/// instead (price protection is the policy; landing odds are the knob).
pub const TIP_ESCALATION_MAX_MULT: u64 = 4;

/// Tip for a sell attempt: base x (1 + prior failures), capped. Pure.
pub fn escalated_tip(base_tip_lamports: u64, failed_attempts: u32) -> u64 {
    let mult = failed_attempts
        .saturating_add(1)
        .min(TIP_ESCALATION_MAX_MULT as u32);
    base_tip_lamports.saturating_mul(mult as u64)
}

/// Paper-mode dispatch-bleed estimate per leg (open, close), USD.
/// Mirrors live debiting (first-touch plus ATA rent on open, dispatch
/// on close) so paper-only PnL nets the same bleed live pays.
/// Zero when priceless.
///
/// Absolute lamports, NOT a rate: at defaults (100k tip, SOL $150)
/// the round trip is ~$0.34 — ~0.3% on a $113 ticket but ~3.4% on the
/// default $10 `NOTIONAL_USD`, larger than the 2.53% venue round trip.
/// Live pays the moving Jito `tip_floor` oracle instead of this static
/// estimate, so expect small drift under congestion.
pub fn paper_dispatch_legs_usd(tip_lamports: u64, sol_usd: f64) -> (f64, f64) {
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return (0.0, 0.0);
    }
    let leg = |lamports: u64| lamports as f64 / LAMPORTS_PER_SOL * sol_usd;
    (
        leg(
            tip_lamports
                .saturating_add(BASE_TX_FEE_LAMPORTS)
                .saturating_add(ATA_RENT_LAMPORTS),
        ),
        leg(tip_lamports.saturating_add(BASE_TX_FEE_LAMPORTS)),
    )
}

/// Guard over a fee-inclusive quote: quote plus `slippage_bps`, up.
/// `None` on zero or overflow (fail-closed).
pub fn apply_slippage_ceiling(quoted_cost_incl_fees: u64, slippage_bps: u64) -> Option<u64> {
    // One constructor so both sides match.
    executor::quote::slippage_ceiling(quoted_cost_incl_fees, slippage_bps)
}

/// Size a buy from USD notional through the paper fill model.
/// Fee tier is required. `None` on bad inputs or overflow (fail-closed).
/// Float to `u64`, fail-closed outside range.
fn u64_from_f64_floor(v: f64) -> Option<u64> {
    if !v.is_finite() || v <= 0.0 || v >= u64::MAX as f64 {
        return None;
    }
    Some(v as u64)
}

pub fn plan_buy(
    notional_usd: f64,
    price_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
    liquidity_usd: f64,
    fee_bps_per_side: f64,
) -> Option<LiveBuyPlan> {
    if !notional_usd.is_finite() || notional_usd <= 0.0 {
        return None;
    }
    if !price_usd.is_finite() || price_usd <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if !fee_bps_per_side.is_finite() || fee_bps_per_side < 0.0 {
        return None;
    }
    // Paper parity: size from filled price, floored.
    let fill = FillModel::default().with_venue_fee_bps(fee_bps_per_side);
    let slip = fill.slip_fraction(Side::Long, notional_usd, liquidity_usd);
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    let amount_f = (notional_usd / price_usd / (1.0 + slip) * scale).floor();
    let amount_tokens = u64_from_f64_floor(amount_f)?;
    if amount_tokens == 0 {
        return None;
    }
    let lamports = notional_usd / sol_usd * LAMPORTS_PER_SOL;
    // Fee first at sizing tier, then slippage ceiling. The ceiling is
    // deliberate (tiers are integers on-chain, so it is a no-op there):
    // sizing must never underfund the fee-inclusive cost.
    let with_fee = u64::try_from(
        (u64_from_f64_floor(lamports)? as u128
            * (BPS_DENOMINATOR as u128 + fee_bps_per_side.ceil() as u128))
            .div_ceil(BPS_DENOMINATOR as u128),
    )
    .ok()?;
    // One guard everywhere.
    let max_sol_cost = apply_slippage_ceiling(with_fee, slippage_bps)?;
    Some(LiveBuyPlan {
        amount_tokens,
        max_sol_cost,
    })
}

/// Sell floor: modeled proceeds minus slippage. Fee tier is required.
/// `None` on bad inputs and zero floor (zero authorizes any price).
pub fn plan_sell_floor(
    amount_tokens: u64,
    price_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
    liquidity_usd: f64,
    fee_bps_per_side: f64,
) -> Option<u64> {
    if amount_tokens == 0 || !price_usd.is_finite() || price_usd <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if slippage_bps >= BPS_DENOMINATOR {
        return None;
    }
    if !fee_bps_per_side.is_finite() || fee_bps_per_side < 0.0 {
        return None;
    }
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    // Paper parity: floor from modeled proceeds.
    let gross_notional = amount_tokens as f64 / scale * price_usd;
    let slip = FillModel::default()
        .with_venue_fee_bps(fee_bps_per_side)
        .slip_fraction(Side::Short, gross_notional, liquidity_usd);
    let lamports = gross_notional * (1.0 - slip).max(0.0) / sol_usd * LAMPORTS_PER_SOL;
    // Same `>=` edge as `plan_buy` (cast saturates).
    if !lamports.is_finite() || lamports <= 0.0 || lamports >= u64::MAX as f64 {
        return None;
    }
    // Exact mul-div floor in u128. The `as u64` below is exact, not
    // saturating-by-accident: the guard above already rejected NaN,
    // non-positive, and everything >= 2^64 (unrepresentable in f64
    // anyway), so every surviving value converts losslessly.
    let base = lamports.floor() as u64;
    debug_assert!((base as f64) <= lamports);
    if base == 0 {
        return None;
    }
    let floor = (base as u128 * (BPS_DENOMINATOR as u128 - slippage_bps as u128)
        / BPS_DENOMINATOR as u128) as u64;
    if floor == 0 {
        return None;
    }
    Some(floor)
}

/// Jito-routed trader over a [`SwapExecutor`].
#[derive(Clone)]
pub struct LiveTrader {
    executor: SwapExecutor,
    slippage_bps: u64,
    tip_lamports: u64,
    /// Priority price for cost math.
    priority_fee_micro_lamports: u64,
    /// Compute limit for cost math.
    compute_unit_limit: u32,
    /// WAL path (`None` disables IO).
    wal_path: Option<PathBuf>,
    /// Unconfirmed buys, re-polled until settled or out of tries.
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
    /// Recent mark prices per mint for volatility-adaptive slippage.
    /// Fed from live dispatch prints (entry/exit); tick-level feed is
    /// future work, so thin histories simply yield the base slippage.
    marks: MarkTrail,
    /// Volatility window, secs (`0` = adaptive off, base slippage only).
    vol_window_secs: u64,
    /// Cap on the volatility add-on, bps (`0` = adaptive off).
    vol_extra_cap_bps: u64,
    /// Last sell bundle per mint (resend guard: never compete with
    /// an inflight-pending prior bundle for the same bag).
    last_sell_bundle: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// One recheck round at a time.
    recheck_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Buy awaiting confirmation. Never a fill until the chain says so.
#[derive(Debug, Clone)]
struct PendingBuy {
    position: LivePosition,
    signature: String,
    bundle_id: String,
    attempts: u32,
}

/// One recheck round result per mint.
#[derive(Debug, Clone)]
pub struct PendingRecheck {
    /// Expected position (valid only on `Confirmed`).
    pub position: LivePosition,
    /// Engine bundle UUID.
    pub bundle_id: String,
    /// Tx signature (lookup on GiveUp).
    pub signature: String,
    pub outcome: RecheckOutcome,
}

/// Recheck verdict for one pending buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecheckOutcome {
    /// Landed: track it and automate exits.
    Confirmed,
    /// Errored on-chain: nothing held, drop it.
    FailedOnChain,
    /// Still unknown: keep for next round.
    StillUnknown,
    /// Out of tries: drop and escalate.
    GiveUp,
}


impl LiveTrader {
    /// Wrap an executor with sizing policy.
    pub fn new(executor: SwapExecutor, slippage_bps: u64, tip_lamports: u64) -> Self {
        Self {
            executor,
            slippage_bps,
            tip_lamports,
            priority_fee_micro_lamports: 0,
            compute_unit_limit: 0,
            wal_path: None,
            marks: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            vol_window_secs: 0,
            vol_extra_cap_bps: 0,
            pending: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            last_sell_bundle: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            recheck_busy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Enable the crash-recovery WAL at `path`.
    pub fn with_wal(mut self, path: PathBuf) -> Self {
        self.wal_path = Some(path);
        self
    }

    /// Compute knobs for cost math (mirrors executor).
    pub fn with_dispatch_fees(
        mut self,
        compute_unit_limit: u32,
        priority_fee_micro_lamports: u64,
    ) -> Self {
        self.compute_unit_limit = compute_unit_limit;
        self.priority_fee_micro_lamports = priority_fee_micro_lamports;
        self
    }

    /// Volatility-adaptive slippage: `window_secs` of mark history per
    /// mint, with the add-on capped at `extra_cap_bps`. Zero/zero (the
    /// default) is bit-identical to static `slippage_bps`.
    pub fn with_adaptive_slippage(mut self, window_secs: u64, extra_cap_bps: u64) -> Self {
        self.vol_window_secs = window_secs;
        self.vol_extra_cap_bps = extra_cap_bps;
        self
    }

    /// Record a live print for the volatility window. Degenerate
    /// prices are ignored (never poison the range).
    pub fn note_mark(&self, mint: &str, price_usd: f64, now_secs: u64) {
        if !price_usd.is_finite() || price_usd <= 0.0 || self.vol_window_secs == 0 {
            return;
        }
        let window = self.vol_window_secs;
        let cutoff = now_secs.saturating_sub(window);
        match self.marks.lock() {
            Ok(mut guard) => {
                let q = guard.entry(mint.to_string()).or_default();
                q.push_back((now_secs, price_usd));
                while q.front().is_some_and(|(ts, _)| *ts < cutoff) {
                    q.pop_front();
                }
                // Bounded memory even when the clock stalls.
                while q.len() > 1024 {
                    q.pop_front();
                }
            }
            Err(_) => {
                tracing::warn!("marks-tracker lock poisoned during note_mark; skipping");
            }
        }
    }

    /// Slippage for this dispatch: base plus the in-window range as
    /// bps, capped. Thin or calm histories yield exactly the base, so
    /// static behavior is the default, not a special case.
    pub fn effective_slippage_bps(&self, mint: &str, now_secs: u64) -> u64 {
        let base = self.slippage_bps;
        if self.vol_window_secs == 0 || self.vol_extra_cap_bps == 0 {
            return base;
        }
        let window = self.vol_window_secs;
        let cutoff = now_secs.saturating_sub(window);
        let range_bps = match self.marks.lock() {
            Ok(mut guard) => {
                let Some(q) = guard.get_mut(mint) else {
                    return base;
                };
                while q.front().is_some_and(|(ts, _)| *ts < cutoff) {
                    q.pop_front();
                }
                let (mut lo, mut hi) = (f64::INFINITY, 0.0f64);
                for (_, p) in q.iter() {
                    lo = lo.min(*p);
                    hi = hi.max(*p);
                }
                if !(hi > lo && lo > 0.0 && hi.is_finite()) {
                    return base;
                }
                ((hi - lo) / lo * BPS_DENOMINATOR as f64).max(0.0)
            }
            Err(_) => {
                tracing::warn!("marks-tracker lock poisoned during effective_slippage; using base");
                return base;
            }
        };
        if !range_bps.is_finite() {
            return base;
        }
        base
            .saturating_add((range_bps as u64).min(self.vol_extra_cap_bps))
            .min(BPS_DENOMINATOR.saturating_sub(1))
    }

    /// Per-dispatch overhead: tip plus fees. Excludes first-buy ATA rent.
    pub fn dispatch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_for_tip(self.tip_lamports)
    }

    /// Overhead at an explicit tip (escalated retries debit actuals, not
    /// the base rate).
    pub fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        let prio = (self.compute_unit_limit as u128)
            .saturating_mul(self.priority_fee_micro_lamports as u128)
            .saturating_add(999_999)
            / 1_000_000;
        let prio = u64::try_from(prio).unwrap_or(u64::MAX);
        tip_lamports
            .saturating_add(prio)
            .saturating_add(BASE_TX_FEE_LAMPORTS)
    }

    /// First-buy cost: dispatch plus one ATA rent.
    pub fn first_touch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_lamports()
            .saturating_add(ATA_RENT_LAMPORTS)
    }

    /// Run `f` under the pending lock (one poison policy).
    fn with_pending<R>(        &self,
        op: &'static str,
        fallback: R,
        f: impl FnOnce(&mut std::collections::HashMap<String, PendingBuy>) -> R,
    ) -> R {
        match self.pending.lock() {
            Ok(mut guard) => f(&mut guard),
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned during {op}; using fallback");
                fallback
            }
        }
    }

    /// Remember a sell bundle for the resend guard.
    fn note_sell_bundle(&self, mint: &str, bundle_id: &str) {
        if let Ok(mut guard) = self.last_sell_bundle.lock() {
            guard.insert(mint.to_string(), bundle_id.to_string());
        }
    }

    /// Forget a sell bundle after confirm.
    fn clear_sell_bundle(&self, mint: &str) {
        if let Ok(mut guard) = self.last_sell_bundle.lock() {
            guard.remove(mint);
        }
    }

    /// True when the last sell bundle for `mint` is still
    /// engine-pending: resends must wait, never compete with it.
    /// `Unknown` proceeds (exits must not stall on a status outage).
    pub(crate) async fn prior_sell_pending(&self, mint: &str) -> bool {
        let id = match self.last_sell_bundle.lock() {
            Ok(guard) => guard.get(mint).cloned(),
            Err(_) => None,
        };
        let Some(id) = id else { return false };
        matches!(
            self.executor.bundle_status(&id).await,
            BundleStatus::Pending
        )
    }

    /// True when a buy awaits confirmation.
    pub fn has_pending(&self) -> bool {
        self.with_pending("has_pending", true, |guard| !guard.is_empty())
    }

    /// Pending mints (cloned).
    pub fn pending_mints(&self) -> Vec<String> {
        self.with_pending("pending_mints", Vec::new(), |guard| {
            guard.keys().cloned().collect()
        })
    }

    /// Re-poll pending buys once. Unknown stays until budget, then GiveUp.
    pub async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        if self
            .recheck_busy
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // Skips are routine; debug level.
            tracing::debug!("pending recheck already in flight; skipping round");
            return Vec::new();
        }
        // Reset on every exit: a stuck flag silences rechecks.
        struct ResetOnDrop {
            busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for ResetOnDrop {
            fn drop(&mut self) {
                self.busy.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _reset = ResetOnDrop {
            busy: std::sync::Arc::clone(&self.recheck_busy),
        };
        let snapshot: Vec<PendingBuy> =
            self.with_pending("recheck-snapshot", Vec::new(), |guard| {
                guard.values().cloned().collect()
            });
        let results = futures_util::future::join_all(snapshot.into_iter().map(|entry| async {
            let outcome = match self
                .executor
                .confirm_signature(&entry.signature, LIVE_CONFIRM_TIMEOUT)
                .await
            {
                Ok(ConfirmOutcome::Confirmed) => RecheckOutcome::Confirmed,
                Ok(ConfirmOutcome::FailedOnChain) => RecheckOutcome::FailedOnChain,
                Ok(ConfirmOutcome::Timeout) | Err(_) => {
                    let attempts = entry.attempts.saturating_add(1);
                    if attempts >= PENDING_MAX_ATTEMPTS {
                        // Last word from the engine before giving up:
                        // terminal states retire now instead of parking
                        // a dead bundle for manual review.
                        match self.executor.bundle_status(&entry.bundle_id).await {
                            BundleStatus::Failed | BundleStatus::Invalid => {
                                RecheckOutcome::FailedOnChain
                            }
                            BundleStatus::Landed => RecheckOutcome::Confirmed,
                            BundleStatus::Pending | BundleStatus::Unknown => {
                                RecheckOutcome::GiveUp
                            }
                        }
                    } else {
                        let mint = entry.position.mint.clone();
                        self.with_pending("recheck-bump", (), |guard| {
                            if let Some(stored) = guard.get_mut(&mint) {
                                stored.attempts = attempts;
                            }
                        });
                        RecheckOutcome::StillUnknown
                    }
                }
            };
            (entry, outcome)
        }))
        .await;
        let mut out = Vec::with_capacity(results.len());
        for (entry, outcome) in results {
            match outcome {
                RecheckOutcome::StillUnknown => {}
                // Terminal verdicts retire the WAL line.
                _ => {
                    self.write_wal_settled(&entry.bundle_id);
                    let mint = entry.position.mint.clone();
                    self.with_pending("recheck-settle", (), |guard| {
                        guard.remove(&mint);
                    });
                }
            }
            out.push(PendingRecheck {
                position: entry.position,
                bundle_id: entry.bundle_id,
                signature: entry.signature,
                outcome,
            });
        }
        out
    }

    /// Build a tracked position (one constructor).
    fn pending_position(
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        amount_tokens: u64,
        creator: Option<String>,
    ) -> LivePosition {
        LivePosition {
            mint: mint.to_string(),
            curve: curve.to_string(),
            assoc_curve: assoc_curve.to_string(),
            amount_tokens,
            creator,
        }
    }

    /// Buy `notional_usd` at `price_usd` via Jito.
    pub async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError> {
        let (mint, curve, assoc_curve, notional_usd, price_usd, sol_usd) = (
            req.mint, req.curve, req.assoc_curve, req.notional_usd, req.price_usd, req.sol_usd,
        );
        let creator = match req.creator {
            Some(c) => {
                Some(parse_pubkey(c).map_err(|e| LiveError::BadOrder(format!("bad creator '{c}': {e}")))?)
            }
            None => None,
        };
        let plan = plan_buy(
            notional_usd,
            price_usd,
            sol_usd,
            self.effective_slippage_bps(mint, now_secs()),
            req.liquidity_usd,
            req.venue_fee_bps.unwrap_or(VENUE_FEE_BPS as f64),
        )
        .ok_or_else(|| {
            LiveError::BadOrder(format!("cannot size buy of {mint} at {price_usd}"))
        })?;
        self.note_mark(mint, price_usd, now_secs());
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.buy_jito(&BuyOrder {
                mint: parse_pubkey(mint)
                    .map_err(|e| LiveError::BadOrder(format!("bad mint '{mint}': {e}")))?,
                bonding_curve: parse_pubkey(curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad curve '{curve}': {e}")))?,
                associated_bonding_curve: parse_pubkey(assoc_curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad assoc_curve '{assoc_curve}': {e}")))?,
                amount_tokens: plan.amount_tokens,
                max_sol_cost: plan.max_sol_cost,
                tip_lamports: self.effective_tip(0).await,
                creator,
                slippage_bps: self.effective_slippage_bps(mint, now_secs()),
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                // No signature: fill unknown, park for review.
                let position = Self::pending_position(
                    mint,
                    curve,
                    assoc_curve,
                    plan.amount_tokens,
                    req.creator.map(|c| c.to_string()),
                );
                return Err(LiveError::UnknownDispatch {
                    position: Box::new(position),
                    reason: format!(
                        "buy dispatch of {mint} timed out after {:?}; fill unknown, MANUAL REVIEW REQUIRED",
                        LIVE_DISPATCH_TIMEOUT
                    ),
                });
            }
        };
        tracing::info!(%mint, bundle = %receipt.bundle_id, amount = plan.amount_tokens, "LIVE buy dispatched");
        // Receipt exists from here; confirm removes it.
        let intended = Self::pending_position(
            mint,
            curve,
            assoc_curve,
            plan.amount_tokens,
            creator.map(|c| c.to_string()),
        );
        Self::track_pending(&self.pending, mint, intended.clone(), &receipt);
        self.write_wal_pending(&intended, &receipt.signature, &receipt.bundle_id);
        match self
            .executor
            .confirm_signature(&receipt.signature, LIVE_CONFIRM_TIMEOUT)
            .await
        {
            Ok(ConfirmOutcome::Confirmed) => {
                Self::untrack_pending(&self.pending, mint);
                Ok((
                    Self::pending_position(
                        mint,
                        curve,
                        assoc_curve,
                        plan.amount_tokens,
                        creator.map(|c| c.to_string()),
                    ),
                    receipt.bundle_id,
                ))
            }
            Ok(ConfirmOutcome::FailedOnChain) => {
                Self::untrack_pending(&self.pending, mint);
                // Proven no-fill: retire the line so boot skips it.
                self.write_wal_settled(&receipt.bundle_id);
                Err(LiveError::Unconfirmed(format!(
                    "buy bundle {} Confirmed failure: errored on-chain; no fill",
                    receipt.bundle_id
                )))
            }
            Ok(ConfirmOutcome::Timeout) => {
                // Engine verdict beats the silent signature: Failed /
                // Invalid is terminal (no fill), Landed confirms.
                match self.executor.bundle_status(&receipt.bundle_id).await {
                    BundleStatus::Failed | BundleStatus::Invalid => {
                        Self::untrack_pending(&self.pending, mint);
                        self.write_wal_settled(&receipt.bundle_id);
                        Err(LiveError::Unconfirmed(format!(
                            "buy bundle {} Confirmed failure: engine reports bundle failed/invalid; no fill",
                            receipt.bundle_id
                        )))
                    }
                    BundleStatus::Landed => {
                        Self::untrack_pending(&self.pending, mint);
                        Ok((
                            Self::pending_position(
                                mint,
                                curve,
                                assoc_curve,
                                plan.amount_tokens,
                                creator.map(|c| c.to_string()),
                            ),
                            receipt.bundle_id,
                        ))
                    }
                    BundleStatus::Pending | BundleStatus::Unknown => {
                        // May land late: keep for recheck.
                        Self::track_pending(
                            &self.pending,
                            mint,
                            Self::pending_position(
                                mint,
                                curve,
                                assoc_curve,
                                plan.amount_tokens,
                                creator.map(|c| c.to_string()),
                            ),
                            &receipt,
                        );
                        Err(LiveError::Unconfirmed(format!(
                            "buy bundle {} unconfirmed after {:?}; tracked as pending, rechecking",
                            receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
                        )))
                    }
                }
            }
            Err(e) => {
                // Transport failure: same as timeout.
                Self::track_pending(
                    &self.pending,
                    mint,
                    Self::pending_position(
                        mint,
                        curve,
                        assoc_curve,
                        plan.amount_tokens,
                        creator.map(|c| c.to_string()),
                    ),
                    &receipt,
                );
                Err(LiveError::Unconfirmed(format!(
                    "buy bundle {} confirm RPC failed ({e}); tracked as pending, rechecking",
                    receipt.bundle_id
                )))
            }
        }
    }

    /// Record an unconfirmed buy. First receipt wins per mint.
    fn track_pending(
        pending: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
        mint: &str,
        position: LivePosition,
        receipt: &JitoReceipt,
    ) {
        match pending.lock() {
            Ok(mut guard) => {
                if let Some(existing) = guard.get(mint) {
                    tracing::warn!(mint = %mint, old_bundle = %existing.bundle_id, new_bundle = %receipt.bundle_id,
                        "second pending buy for the same mint; keeping the first, MANUAL REVIEW REQUIRED for the second");
                }
                guard.entry(mint.to_string()).or_insert(PendingBuy {
                    position,
                    signature: receipt.signature.clone(),
                    bundle_id: receipt.bundle_id.clone(),
                    attempts: 1,
                });
            }
            // Poisoned lock would strand funds: escalate loudly.
            Err(_) => tracing::warn!(mint = %mint, bundle = %receipt.bundle_id,
                "pending-tracker lock poisoned; buy fill untracked, MANUAL REVIEW REQUIRED"),
        }
    }

    /// Remove a settled entry. Timeouts stay for recheck.
    fn untrack_pending(
        pending: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
        mint: &str,
    ) {
        match pending.lock() {
            Ok(mut guard) => {
                guard.remove(mint);
            }
            Err(_) => tracing::warn!(mint = %mint,
                "pending-tracker lock poisoned; settled entry not removed"),
        }
    }

    /// Append one line (best-effort, synced).
    fn wal_append(&self, line: String) {
        let Some(path) = self.wal_path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(path) {
            Ok(mut f) => {
                if writeln!(f, "{line}").is_err() || f.sync_all().is_err() {
                    tracing::warn!("live WAL append/fsync failed (non-fatal)");
                }
            }
            Err(e) => tracing::warn!(error = %e, "live WAL unwritable (non-fatal)"),
        }
    }

    /// JSON creator (`null` when unknown).
    fn wal_creator(creator: Option<&String>) -> String {
        creator
            .map(|c| format!("\"{c}\""))
            .unwrap_or_else(|| "null".to_string())
    }

    /// Record a dispatch BEFORE confirming.
    fn write_wal_pending(&self, position: &LivePosition, signature: &str, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"pending\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"sig\":\"{signature}\",\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            position.mint,
            position.curve,
            position.assoc_curve,
            position.amount_tokens,
            Self::wal_creator(position.creator.as_ref()),
            now_secs(),
        ));
    }

    /// Record a dead bundle so boot skips it.
    fn write_wal_settled(&self, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"settled\",\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            now_secs(),
        ));
    }

    /// Record a fill and retire pending. Call AFTER tracking it.
    pub fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"position\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            position.mint,
            position.curve,
            position.assoc_curve,
            position.amount_tokens,
            Self::wal_creator(position.creator.as_ref()),
            now_secs(),
        ));
        self.write_wal_settled(bundle_id);
    }

    /// Record a human-exit bag. `note` is a lookup id, unquoted.
    pub fn record_manual(&self, position: &LivePosition, note: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"manual\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"note\":\"{note}\",\"ts\":{}}}",
            position.mint,
            position.curve,
            position.assoc_curve,
            position.amount_tokens,
            Self::wal_creator(position.creator.as_ref()),
            now_secs(),
        ));
    }

    /// Record a confirmed sell.
    pub fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"sell\",\"mint\":\"{mint}\",\"amount\":{amount_tokens},\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            now_secs(),
        ));
    }

    /// Base Jito tip (live_sell escalates it by attempts).
    pub fn base_tip(&self) -> u64 {
        self.tip_lamports
    }

    /// Congestion-aware tip for an attempt: escalated base lifted to
    /// the landed-tip floor, bounded at 4x base. Static tips underland
    /// in congestion and overpay in calm; the floor adapts both legs.
    /// Zero base stays zero (explicit tipless).
    pub async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        let base = self.tip_lamports;
        let esc = escalated_tip(base, failed_attempts);
        if base == 0 {
            return esc;
        }
        let floor = self.executor.tip_floor_lamports().await;
        esc.max(floor.min(base.saturating_mul(TIP_ESCALATION_MAX_MULT)))
    }

    /// Import boot-replayed pendings into recheck.
    pub fn import_pending(&self, items: Vec<wal::WalPending>) -> usize {
        self.with_pending("wal-import", 0, |guard| {
            let mut n = 0usize;
            for item in items {
                let mint = item.position.mint.clone();
                guard.entry(mint).or_insert_with(|| {
                    n += 1;
                    PendingBuy {
                        position: item.position,
                        signature: item.signature,
                        bundle_id: item.bundle_id,
                        attempts: 1,
                    }
                });
            }
            n
        })
    }

    /// Sell the full tracked amount via Jito.
    pub async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError> {
        let position = req.position;
        let slip = self.effective_slippage_bps(&position.mint, now_secs());
        let floor = plan_sell_floor(
            position.amount_tokens,
            req.price_usd,
            req.sol_usd,
            slip,
            req.liquidity_usd,
            req.venue_fee_bps.unwrap_or(VENUE_FEE_BPS as f64),
        )
        .ok_or_else(|| LiveError::BadOrder(format!("cannot floor sell of {}", position.mint)))?;
        self.note_mark(&position.mint, req.price_usd, now_secs());
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_jito(&SellOrder {
                mint: parse_pubkey(&position.mint)
                    .map_err(|e| LiveError::BadOrder(format!("bad mint '{}': {e}", position.mint)))?,
                bonding_curve: parse_pubkey(&position.curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad curve '{}': {e}", position.curve)))?,
                associated_bonding_curve: parse_pubkey(&position.assoc_curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad assoc_curve '{}': {e}", position.assoc_curve)))?,
                amount_tokens: position.amount_tokens,
                min_sol_output: floor,
                tip_lamports: req.tip_lamports,
                creator: match &position.creator {
                    Some(c) => Some(
                        parse_pubkey(c)
                            .map_err(|e| LiveError::BadOrder(format!("bad creator '{c}': {e}")))?,
                    ),
                    None => None,
                },
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                return Err(LiveError::Unconfirmed(format!(
                    "sell dispatch of {} timed out after {:?}; still holding",
                    position.mint, LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE sell dispatched");
        self.note_sell_bundle(&position.mint, &receipt.bundle_id);
        self.confirm_pool_sell(&position.mint, &receipt, "sell").await
    }

    /// Confirm a pool-leg sell bundle. Shared by every venue (`label`
    /// names the leg in logs and errors). No WAL write here: the
    /// caller records the sell write-ahead of its custody subtract.
    async fn confirm_pool_sell(
        &self,
        mint: &str,
        receipt: &JitoReceipt,
        label: &str,
    ) -> Result<String, LiveError> {
        match self
            .executor
            .confirm_signature(&receipt.signature, LIVE_CONFIRM_TIMEOUT)
            .await
        {
            Ok(ConfirmOutcome::Confirmed) => {
                self.clear_sell_bundle(mint);
                Ok(receipt.bundle_id.clone())
            }
            Ok(ConfirmOutcome::FailedOnChain) => Err(LiveError::Unconfirmed(format!(
                "{label} bundle {} errored on-chain; still holding",
                receipt.bundle_id
            ))),
            Ok(ConfirmOutcome::Timeout) => {
                match self.executor.bundle_status(&receipt.bundle_id).await {
                    BundleStatus::Failed | BundleStatus::Invalid => {
                        Err(LiveError::Unconfirmed(format!(
                            "{label} bundle {} engine reports bundle failed/invalid; still holding",
                            receipt.bundle_id
                        )))
                    }
                    BundleStatus::Landed => {
                        self.clear_sell_bundle(mint);
                        Ok(receipt.bundle_id.clone())
                    }
                    BundleStatus::Pending | BundleStatus::Unknown => {
                        Err(LiveError::Unconfirmed(format!(
                            "{label} bundle {} unconfirmed after {:?}; still holding",
                            receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
                        )))
                    }
                }
            }
            // Transport failure: same as timeout.
            Err(e) => Err(LiveError::Unconfirmed(format!(
                "{label} bundle {} confirm RPC failed ({e}); still holding",
                receipt.bundle_id
            ))),
        }
    }

    /// Raydium sell at `pool` via Jito. Floor prices from vault reserves.
    pub async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError> {
        let position = req.position;
        let pool = req.pool;
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_raydium_jito(&executor::SellRaydiumOrder {
                mint: parse_pubkey(&position.mint)
                    .map_err(|e| LiveError::BadOrder(format!("bad mint '{}': {e}", position.mint)))?,
                amm: parse_pubkey(&pool.amm)
                    .map_err(|e| LiveError::BadOrder(format!("bad amm '{}': {e}", pool.amm)))?,
                base_vault: parse_pubkey(&pool.base_vault)
                    .map_err(|e| LiveError::BadOrder(format!("bad base_vault '{e}")))?,
                quote_vault: parse_pubkey(&pool.quote_vault)
                    .map_err(|e| LiveError::BadOrder(format!("bad quote_vault '{e}")))?,
                amount_tokens: position.amount_tokens,
                slippage_bps: self.effective_slippage_bps(&position.mint, now_secs()),
                tip_lamports: req.tip_lamports,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                return Err(LiveError::Unconfirmed(format!(
                    "raydium sell dispatch of {} timed out after {:?}; still holding",
                    position.mint, LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE raydium sell dispatched");
        self.note_sell_bundle(&position.mint, &receipt.bundle_id);
        self.confirm_pool_sell(&position.mint, &receipt, "raydium sell").await
    }

    /// PumpSwap sell at `pool` via Jito. Floor quotes from live
    /// reserves plus the fee schedule.
    pub async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError> {
        let position = req.position;
        let pool = req.pool;
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_pumpswap_jito(&executor::SellPumpSwapOrder {
                mint: parse_pubkey(&position.mint)
                    .map_err(|e| LiveError::BadOrder(format!("bad mint '{}': {e}", position.mint)))?,
                pool: parse_pubkey(&pool.amm)
                    .map_err(|e| LiveError::BadOrder(format!("bad pool '{}': {e}", pool.amm)))?,
                base_vault: parse_pubkey(&pool.base_vault)
                    .map_err(|e| LiveError::BadOrder(format!("bad base_vault '{e}")))?,
                quote_vault: parse_pubkey(&pool.quote_vault)
                    .map_err(|e| LiveError::BadOrder(format!("bad quote_vault '{e}")))?,
                amount_tokens: position.amount_tokens,
                slippage_bps: self.effective_slippage_bps(&position.mint, now_secs()),
                tip_lamports: req.tip_lamports,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                return Err(LiveError::Unconfirmed(format!(
                    "pumpswap sell dispatch of {} timed out after {:?}; still holding",
                    position.mint, LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE pumpswap sell dispatched");
        self.note_sell_bundle(&position.mint, &receipt.bundle_id);
        self.confirm_pool_sell(&position.mint, &receipt, "pumpswap sell").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_ceiling_covers_fee_inclusive_quotes() {
        assert_eq!(apply_slippage_ceiling(1_668_970, 100), Some(1_685_660));
        assert_eq!(apply_slippage_ceiling(10_001, 0), Some(10_001));
        assert_eq!(apply_slippage_ceiling(10_000, 1), Some(10_001));
        assert_eq!(apply_slippage_ceiling(0, 200), None);
        assert_eq!(apply_slippage_ceiling(1_000, 9_999), Some(2_000));
        assert_eq!(apply_slippage_ceiling(1_000, 10_000), None);
        assert_eq!(apply_slippage_ceiling(1_000, 99_999), None);
        assert_eq!(apply_slippage_ceiling(u64::MAX, 10_000), None);
        assert_eq!(apply_slippage_ceiling(u64::MAX, 0), Some(u64::MAX));
    }

    #[test]
    fn tip_escalates_bounded_by_attempts() {
        assert_eq!(escalated_tip(100_000, 0), 100_000);
        assert_eq!(escalated_tip(100_000, 1), 200_000);
        assert_eq!(escalated_tip(100_000, 2), 300_000);
        assert_eq!(escalated_tip(100_000, 3), 400_000);
        // Capped: a stuck bag cannot burn the wallet.
        assert_eq!(escalated_tip(100_000, 4), 400_000);
        assert_eq!(escalated_tip(100_000, u32::MAX), 400_000);
        // Zero base stays zero; saturates instead of wrapping.
        assert_eq!(escalated_tip(0, 9), 0);
        assert_eq!(escalated_tip(u64::MAX, 9), u64::MAX);
    }

    #[tokio::test]
    async fn effective_tip_without_oracle_matches_escalation() {
        // No bundle client: floor is 0, configured tip rules.
        let trader = cost_trader(100_000, 0, 0);
        assert_eq!(trader.effective_tip(0).await, 100_000);
        assert_eq!(trader.effective_tip(2).await, 300_000);
        // Explicit tipless stays tipless (floor must not lift it).
        let bare = cost_trader(0, 0, 0);
        assert_eq!(bare.effective_tip(9).await, 0);
    }

    #[test]
    fn buy_plan_math() {
        let plan = plan_buy(10.0, 0.000_004_2, 150.0, 200, 30_000.0, 125.0).expect("plan");
        assert_eq!(plan.amount_tokens, 2_350_783_728_484);
        assert_eq!(plan.max_sol_cost, 68_850_000);
    }

    #[test]
    fn buy_plan_custom_fee_sizes_and_funds_at_tier() {
        let std = plan_buy(10.0, 0.000_004_2, 150.0, 200, 30_000.0, 125.0).expect("std");
        let cus = plan_buy(10.0, 0.000_004_2, 150.0, 200, 30_000.0, 345.0).expect("custom");
        assert!(cus.amount_tokens < std.amount_tokens);
        assert!(cus.max_sol_cost > std.max_sol_cost);
        use simulator::{PaperPortfolio, Side};
        let mut paper = PaperPortfolio::new(1_000.0, 0.0);
        paper.set_venue_fee_for_mint("MINT", 345.0);
        let id = paper
            .open_position(
                "MINT", Side::Long, 0.000_004_2, 10.0, 30_000.0, 75.0, -8.0, false, 0.0,
                0, 0.0, 1_000,
            )
            .expect("open");
        let qty = paper.position(id).expect("pos").quantity;
        assert_eq!(cus.amount_tokens, (qty * 1e6).floor() as u64);
    }

    #[test]
    fn buy_plan_rejects_degenerate() {
        assert_eq!(plan_buy(0.0, 1.0, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_buy(10.0, 0.0, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_buy(10.0, 1.0, 0.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_buy(f64::NAN, 1.0, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_buy(1e-12, 1.0, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_buy(10.0, 1.0, 150.0, 200, 30_000.0, f64::NAN), None);
        assert_eq!(plan_buy(10.0, 1.0, 150.0, 200, 30_000.0, -1.0), None);
    }

    #[test]
    fn sell_floor_math() {
        let floor = plan_sell_floor(1_000_000, 2.0, 150.0, 200, 30_000.0, 125.0).expect("floor");
        assert_eq!(floor, 12_902_461);
    }

    #[test]
    fn sell_floor_custom_fee_floors_lower() {
        let std = plan_sell_floor(1_000_000, 2.0, 150.0, 200, 30_000.0, 125.0).expect("std");
        let cus = plan_sell_floor(1_000_000, 2.0, 150.0, 200, 30_000.0, 345.0).expect("custom");
        assert!(cus < std, "custom {cus} vs std {std}");
    }

    #[test]
    fn u64_boundary_fails_closed() {
        assert_eq!(u64_from_f64_floor(2f64.powi(64)), None);
        assert_eq!(u64_from_f64_floor(f64::NAN), None);
        assert_eq!(u64_from_f64_floor(f64::INFINITY), None);
        assert_eq!(u64_from_f64_floor(0.0), None);
        assert_eq!(u64_from_f64_floor(-1.0), None);
        assert_eq!(u64_from_f64_floor(1e10), Some(10_000_000_000));
        assert_eq!(u64_from_f64_floor(1.9), Some(1));
    }

    #[test]
    fn buy_plan_rejects_overflow() {
        assert_eq!(plan_buy(1e20, 1e-10, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_buy(1e13, 1e6, 150.0, 200, 30_000.0, 125.0), None);
    }

    #[test]
    fn sell_floor_rejects_zero_floor() {
        assert_eq!(plan_sell_floor(1_000_000, 2.0, 150.0, 10_000, 30_000.0, 125.0), None);
        assert_eq!(plan_sell_floor(1_000_000, 2.0, 150.0, 10_001, 30_000.0, 125.0), None);
        assert_eq!(plan_sell_floor(1, 1e-9, 1e9, 200, 30_000.0, 125.0), None);
    }

    #[test]
    fn sell_floor_uses_integer_math_at_1_lamport_boundary() {
        assert_eq!(plan_sell_floor(1, 0.2, 150.0, 0, f64::NAN, 125.0), Some(1));
        assert_eq!(plan_sell_floor(1, 0.2, 150.0, 1, f64::NAN, 125.0), None);
        let floored = (10_001u64 as u128 * (10_000 - 1) as u128 / 10_000) as u64;
        assert_eq!(floored, 9_999);
    }

    #[test]
    fn sell_floor_rejects_degenerate() {
        assert_eq!(plan_sell_floor(0, 2.0, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_sell_floor(100, 0.0, 150.0, 200, 30_000.0, 125.0), None);
        assert_eq!(plan_sell_floor(u64::MAX, 1e12, 1e-6, 0, 30_000.0, 125.0), None);
        assert_eq!(plan_sell_floor(100, 2.0, 150.0, 200, 30_000.0, f64::NAN), None);
        assert_eq!(plan_sell_floor(100, 2.0, 150.0, 200, 30_000.0, -1.0), None);
    }

    #[test]
    fn live_sizes_match_paper_ledger() {
        // Parity lock: same inputs size the same on both legs.
        use simulator::{PaperPortfolio, Side};
        let mut paper = PaperPortfolio::new(1_000.0, 0.0);
        let id = paper
            .open_position(
                "MINT", Side::Long, 0.000_004_2, 10.0, 30_000.0, 75.0, -8.0, false, 0.0,
                0, 0.0, 1_000,
            )
            .expect("open");
        let qty = paper.position(id).expect("pos").quantity;
        let plan = plan_buy(10.0, 0.000_004_2, 150.0, 200, 30_000.0, 125.0).expect("plan");
        assert_eq!(plan.amount_tokens, (qty * 1e6).floor() as u64);
        let floor = plan_sell_floor(1_000_000, 3.0, 150.0, 200, 30_000.0, 125.0).expect("floor");
        let gross = 1_000_000_f64 / 1e6 * 3.0 / 150.0 * LAMPORTS_PER_SOL;
        assert!((floor as f64) < gross, "floor {floor} vs gross {gross}");
        assert!((floor as f64) > gross * 0.9, "floor {floor} vs gross {gross}");
    }

    #[test]
    fn wal_replay_folds_pending_settled_positions_sells() {
        let lines = [
            r#"{"t":"pending","mint":"MINT","curve":"CURVE","assoc":"ASSOC","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}"#,
            r#"{"t":"pending","mint":"DEAD","curve":"CURVE","assoc":"ASSOC","amount":500,"creator":null,"sig":"SIG2","bundle":"B2","ts":2}"#,
            r#"{"t":"settled","bundle":"B2","ts":3}"#,
            r#"{"t":"position","mint":"HELD","curve":"CURVE","assoc":"ASSOC","amount":700,"creator":null,"bundle":"B3","ts":4}"#,
            r#"{"t":"sell","mint":"HELD","amount":200,"bundle":"B4","ts":5}"#,
            r#"{"t":"manual","mint":"STUCK","curve":"CURVE","assoc":"ASSOC","amount":300,"creator":null,"ts":6}"#,
            "not json at all",
            r#"{"t":"pending","mint":"BROKEN","ts":7}"#,
        ];
        let replay = wal::replay_lines(lines.into_iter());
        assert_eq!(replay.pending.len(), 1);
        assert_eq!(replay.pending[0].bundle_id, "B1");
        assert_eq!(replay.pending[0].signature, "SIG1");
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].mint, "HELD");
        assert_eq!(replay.positions[0].amount_tokens, 500);
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.mint, "STUCK");
        assert_eq!(replay.manual[0].note, "");
        assert_eq!(replay.skipped, 2);
    }

    #[test]
    fn wal_replay_settled_retires_pending_beside_manual() {
        let retired = [
            r#"{"t":"pending","mint":"MINT","curve":"CURVE","assoc":"ASSOC","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}"#,
            r#"{"t":"manual","mint":"MINT","curve":"CURVE","assoc":"ASSOC","amount":1000,"creator":null,"note":"dispatch-unknown","ts":2}"#,
            r#"{"t":"settled","bundle":"B1","ts":3}"#,
        ];
        let replay = wal::replay_lines(retired.into_iter());
        assert!(replay.pending.is_empty(), "tombstone retires the poll");
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.mint, "MINT");
    }

    fn cost_trader(tip_lamports: u64, unit_limit: u32, unit_price: u64) -> LiveTrader {
        use executor::WalletGuard;
        use solana_commitment_config::CommitmentConfig;
        use solana_keypair::Keypair;
        let keypair = Keypair::new();
        let wallet =
            WalletGuard::from_base58(&bs58::encode(keypair.to_bytes()).into_string())
                .expect("wallet");
        let exec = SwapExecutor::new(
            listener::types::DEFAULT_HTTP_RPC_URL,
            CommitmentConfig::confirmed(),
            wallet,
        )
        .expect("executor");
        LiveTrader::new(exec, 200, tip_lamports).with_dispatch_fees(unit_limit, unit_price)
    }

    #[test]
    fn adaptive_slippage_off_by_default() {
        // Defaults: bit-identical to static slippage (200).
        let trader = cost_trader(10_000, 0, 0);
        trader.note_mark("M", 1.0, 1000);
        trader.note_mark("M", 2.0, 1001);
        assert_eq!(trader.effective_slippage_bps("M", 1001), 200);
        assert_eq!(trader.effective_slippage_bps("UNKNOWN", 1001), 200);
    }

    #[test]
    fn adaptive_slippage_tracks_window_range_capped() {
        let trader = cost_trader(10_000, 0, 0).with_adaptive_slippage(60, 150);
        // Thin history: base.
        trader.note_mark("M", 1.0, 1000);
        assert_eq!(trader.effective_slippage_bps("M", 1000), 200);
        // 10% range in window: +1000bps raw, capped at +150.
        trader.note_mark("M", 1.1, 1010);
        assert_eq!(trader.effective_slippage_bps("M", 1010), 350);
        // Small range: exact add-on (1% -> +100bps).
        let calm = cost_trader(10_000, 0, 0).with_adaptive_slippage(60, 150);
        calm.note_mark("M", 1.0, 1000);
        calm.note_mark("M", 1.01, 1010);
        assert_eq!(calm.effective_slippage_bps("M", 1010), 300);
        // Window expiry: stale marks fall out, back to base.
        assert_eq!(trader.effective_slippage_bps("M", 2000), 200);
        // Degenerate marks never poison the range.
        trader.note_mark("M", 0.0, 2001);
        trader.note_mark("M", f64::NAN, 2001);
        assert_eq!(trader.effective_slippage_bps("M", 2001), 200);
    }

    #[test]
    fn dispatch_cost_includes_base_fee() {
        let trader = cost_trader(10_000, 200_000, 1_000);
        assert_eq!(
            trader.dispatch_cost_lamports(),
            10_000 + 200 + BASE_TX_FEE_LAMPORTS
        );
        let bare = cost_trader(0, 0, 0);
        assert_eq!(bare.dispatch_cost_lamports(), BASE_TX_FEE_LAMPORTS);
    }

    #[test]
    fn first_touch_adds_ata_rent() {
        let trader = cost_trader(10_000, 200_000, 1_000);
        assert_eq!(
            trader.first_touch_cost_lamports(),
            trader.dispatch_cost_lamports() + ATA_RENT_LAMPORTS
        );
        let bare = cost_trader(0, 0, 0);
        assert_eq!(
            bare.first_touch_cost_lamports(),
            BASE_TX_FEE_LAMPORTS + ATA_RENT_LAMPORTS
        );
    }
}
