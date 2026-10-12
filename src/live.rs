//! Live execution sink: USD sizing math plus Jito-routed dispatch.
//!
//! [`LiveTrader`] wraps [`SwapExecutor`](executor::SwapExecutor) with the
//! pipeline's USD-denominated sizing. [`plan_buy`] / [`plan_sell`] are pure
//! (unit-tested); only [`LiveTrader::buy`] / [`sell`](LiveTrader::sell)
//! touch the network.

use executor::{BuyOrder, ConfirmOutcome, ExecutorError, JitoReceipt, SellOrder, SwapExecutor, parse_pubkey};
use std::time::Duration;
use thiserror::Error;

/// Pump.fun mint decimals (base-unit scale). The confirm path verifies
/// every mint reads 6 before sizing (see `ReserveFeed::token_decimals`
/// and the decimals gate in `confirm_open`): non-6 mints are dropped
/// rather than mispriced, so this constant is a checked invariant, not
/// an assumption.
pub const TOKEN_DECIMALS: u32 = 6;
/// Lamports per SOL.
pub const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Live-trade failures: bad inputs or dispatch errors.
#[derive(Error, Debug)]
pub enum LiveError {
    /// Address or amount cannot be built from market data: {0}
    #[error("bad live order: {0}")]
    BadOrder(String),
    /// Dispatch failed: {0}
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] ExecutorError),
    /// Bundle accepted but never confirmed (still holding / never filled).
    #[error("unconfirmed live fill: {0}")]
    Unconfirmed(String),
}

/// Fill-confirmation deadline per dispatch (bundles land in seconds or
/// not at all; the gate loop must not stall on one token).
pub const LIVE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// Dispatch deadline per Jito send: only `confirm_signature` used to be
/// bounded, so a hung RPC/Jito call blocked the caller (and its locks)
/// forever. A timed-out dispatch has no receipt, so the fill is reported
/// unknown for manual review rather than assumed either way.
pub const LIVE_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum spacing between live-sell retries for one mint.
pub const SELL_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
/// Failed live sells before tracking is dropped for manual exit.
pub const SELL_MAX_ATTEMPTS: u32 = 5;
/// Recheck rounds for an unconfirmed buy before giving up to manual review.
pub const PENDING_MAX_ATTEMPTS: u32 = 5;

/// Tracked live fill (amounts in token base units).
#[derive(Debug, Clone)]
pub struct LivePosition {
    /// Token mint (base58).
    pub mint: String,
    /// Bonding-curve address (base58).
    pub curve: String,
    /// Curve ATA address (base58).
    pub assoc_curve: String,
    /// Bought base units (for full-size exits).
    pub amount_tokens: u64,
    /// Token creator (base58), when known: selects v2 execution shapes
    /// (buys use the extended mayhem shape; sells use the 26-account
    /// `sell_v2` shape with reference-resolved routing). `None` keeps
    /// the legacy core (classic curves; rejected on mayhem curves).
    pub creator: Option<String>,
}

/// Buy request: sizing inputs plus the optional creator, bundled so
/// live-dispatch context cannot grow call arity again.
#[derive(Debug, Clone, Copy)]
pub struct LiveBuyParams<'a> {
    /// Token mint (base58).
    pub mint: &'a str,
    /// Bonding-curve address (base58).
    pub curve: &'a str,
    /// Curve ATA address (base58).
    pub assoc_curve: &'a str,
    /// Exposure in USD.
    pub notional_usd: f64,
    /// Execution price in USD per token.
    pub price_usd: f64,
    /// SOL price in USD (lamport conversion).
    pub sol_usd: f64,
    /// Token creator (base58), when the launch carried one.
    pub creator: Option<&'a str>,
}

/// Planned buy: integer amounts ready for [`BuyOrder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBuyPlan {
    /// Token base units to buy.
    pub amount_tokens: u64,
    /// Slippage guard in lamports.
    pub max_sol_cost: u64,
}

/// Size a buy from USD notional at spot: tokens from `notional/price`,
/// spend cap from `notional/sol` plus `slippage_bps` (clamped to 10_000 =
/// 100% like the sell floor, so a misconfigured `SLIPPAGE_BPS` cannot
/// authorize unbounded overspend). `None` on non-positive inputs, zero
/// quantity, or amounts that do not fit `u64` (`as`-casts saturate
/// silently past the max — a garbage print must fail closed, never size
/// a jumbo order).
pub fn plan_buy(
    notional_usd: f64,
    price_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
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
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    let amount_f = (notional_usd / price_usd * scale).floor();
    if !amount_f.is_finite() || amount_f <= 0.0 || amount_f > u64::MAX as f64 {
        return None;
    }
    let amount_tokens = amount_f as u64;
    if amount_tokens == 0 {
        return None;
    }
    let lamports = notional_usd / sol_usd * LAMPORTS_PER_SOL;
    if !lamports.is_finite() || lamports <= 0.0 {
        return None;
    }
    let max_f =
        (lamports * (10_000.0 + slippage_bps.min(10_000) as f64) / 10_000.0).ceil();
    if !max_f.is_finite() || max_f <= 0.0 || max_f > u64::MAX as f64 {
        return None;
    }
    Some(LiveBuyPlan {
        amount_tokens,
        max_sol_cost: max_f as u64,
    })
}

/// Floor for a sell: expected proceeds minus `slippage_bps`. `None` on
/// degenerate inputs — and on a zero floor: `min_sol_output = 0`
/// authorizes the slot leader to fill at any price (sandwich drain), so
/// full-slippage configs and sub-lamport dust fail closed instead of
/// dispatching unprotected.
pub fn plan_sell_floor(
    amount_tokens: u64,
    price_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
) -> Option<u64> {
    if amount_tokens == 0 || !price_usd.is_finite() || price_usd <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if slippage_bps >= 10_000 {
        return None;
    }
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    let lamports = amount_tokens as f64 / scale * price_usd / sol_usd * LAMPORTS_PER_SOL;
    if !lamports.is_finite() || lamports <= 0.0 {
        return None;
    }
    let floor = (lamports * (10_000.0 - slippage_bps as f64) / 10_000.0).floor() as u64;
    if floor == 0 {
        return None;
    }
    Some(floor)
}

/// Jito-routed live trader over a configured [`SwapExecutor`].
#[derive(Clone)]
pub struct LiveTrader {
    executor: SwapExecutor,
    slippage_bps: u64,
    tip_lamports: u64,
    /// Buys accepted by the engine but unconfirmed when the deadline hit.
    /// A timed-out bundle may still land late; dropping it here would leave
    /// real tokens untracked (no exit automation). Entries are re-polled
    /// via [`LiveTrader::recheck_pending`] until confirmed, failed, or
    /// out of attempts (then manual review). Short critical sections only.
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
    /// At most one [`LiveTrader::recheck_pending`] round runs at a time:
    /// ticks fire every second while a round can take up to the confirm
    /// timeout, so overlapping rounds would stack identical confirm RPCs
    /// and race the attempt counters. Late callers skip (empty vec).
    recheck_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Buy awaiting confirmation: reserved for the recheck loop, never treated
/// as a fill until the chain says so. Carries BOTH identifiers: the
/// base58 `signature` (what `confirm_signature` polls — a bundle UUID
/// never parses as one) and the engine `bundle_id` (for logs/cards).
#[derive(Debug, Clone)]
struct PendingBuy {
    position: LivePosition,
    signature: String,
    bundle_id: String,
    attempts: u32,
}

/// Outcome of one [`LiveTrader::recheck_pending`] round per mint.
#[derive(Debug, Clone)]
pub struct PendingRecheck {
    /// Expected position (valid only when `outcome` is `Confirmed`).
    pub position: LivePosition,
    /// Engine bundle UUID.
    pub bundle_id: String,
    /// What the re-poll found.
    pub outcome: RecheckOutcome,
}

/// Recheck verdict for one pending buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecheckOutcome {
    /// Landed after all: promote to tracked and automate exits.
    Confirmed,
    /// Errored on-chain: nothing held, drop silently-ish (log).
    FailedOnChain,
    /// Still unknown: keep pending for the next round.
    StillUnknown,
    /// Out of attempts: drop and escalate to manual review.
    GiveUp,
}

impl LiveTrader {
    /// Wrap an executor (already Jito-attached) with sizing policy.
    pub fn new(executor: SwapExecutor, slippage_bps: u64, tip_lamports: u64) -> Self {
        Self {
            executor,
            slippage_bps,
            tip_lamports,
            pending: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            recheck_busy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// True when at least one buy awaits confirmation.
    pub fn has_pending(&self) -> bool {
        match self.pending.lock() {
            Ok(guard) => !guard.is_empty(),
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned; assuming pending");
                true
            }
        }
    }

    /// Re-poll every pending buy once (one fresh confirmation window each).
    /// Confirmed and failed entries leave the map; unknown ones stay until
    /// [`PENDING_MAX_ATTEMPTS`] and then report [`RecheckOutcome::GiveUp`].
    /// Confirms run concurrently (one slow mint must not hold up the
    /// rest); a round already in flight makes late callers skip.
    pub async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        if self
            .recheck_busy
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Vec::new();
        }
        // Reset on every exit path, including panic unwinds: a stuck flag
        // would silence all future rechecks.
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
        let snapshot: Vec<PendingBuy> = match self.pending.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            // Poisoned after a prior panic: report empty (nothing
            // rechecked) but loudly, never silently.
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned; recheck skipped");
                Vec::new()
            }
        };
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
                        RecheckOutcome::GiveUp
                    } else {
                        match self.pending.lock() {
                            Ok(mut guard) => {
                                if let Some(stored) = guard.get_mut(&entry.position.mint) {
                                    stored.attempts = attempts;
                                }
                            }
                            Err(_) => tracing::warn!(
                                mint = %entry.position.mint,
                                "pending-tracker lock poisoned; attempt counter frozen"
                            ),
                        }
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
                _ => match self.pending.lock() {
                    Ok(mut guard) => {
                        guard.remove(&entry.position.mint);
                    }
                    Err(_) => tracing::warn!(
                        mint = %entry.position.mint,
                        "pending-tracker lock poisoned; settled entry not removed"
                    ),
                },
            }
            out.push(PendingRecheck {
                position: entry.position,
                bundle_id: entry.bundle_id,
                outcome,
            });
        }
        out
    }

    /// Build the tracked [`LivePosition`] for one buy (single
    /// constructor so dispatch, confirm, and pending paths cannot drift
    /// apart on what a fill carries).
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

    /// Buy `notional_usd` at `price_usd` via Jito bundle.
    /// Returns the tracked position plus the bundle UUID.
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
        let plan =
            plan_buy(notional_usd, price_usd, sol_usd, self.slippage_bps).ok_or_else(|| {
                LiveError::BadOrder(format!("cannot size buy of {mint} at {price_usd}"))
            })?;
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
                tip_lamports: self.tip_lamports,
                creator,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                // Dispatch hung: the bundle may or may not have reached
                // the engine, and there is no signature to re-poll — the
                // fill is unknown and needs a human.
                return Err(LiveError::Unconfirmed(format!(
                    "buy dispatch of {mint} timed out after {:?}; fill unknown, MANUAL REVIEW REQUIRED",
                    LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(%mint, bundle = %receipt.bundle_id, amount = plan.amount_tokens, "LIVE buy dispatched");
        match self
            .executor
            .confirm_signature(&receipt.signature, LIVE_CONFIRM_TIMEOUT)
            .await
        {
            Ok(ConfirmOutcome::Confirmed) => Ok((
                Self::pending_position(
                    mint,
                    curve,
                    assoc_curve,
                    plan.amount_tokens,
                    creator.map(|c| c.to_string()),
                ),
                receipt.bundle_id,
            )),
            Ok(ConfirmOutcome::FailedOnChain) => Err(LiveError::Unconfirmed(format!(
                "buy bundle {} errored on-chain; no fill",
                receipt.bundle_id
            ))),
            Ok(ConfirmOutcome::Timeout) => {
                // A timed-out bundle may still land late: track it for the
                // recheck loop instead of assuming no fill (an untracked bag
                // would never get exit automation).
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
            Err(e) => {
                // Transport failure mid-confirm: the fill is just as
                // unknown as a timeout (the bundle may already have
                // landed), so track it identically. Only a proven
                // on-chain failure means no fill.
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
                    "buy bundle {} confirmRPC failed ({e}); tracked as pending, rechecking",
                    receipt.bundle_id
                )))
            }
        }
    }

    /// Record an unconfirmed buy for the recheck loop. Used for timeouts
    /// AND transport errors alike: in both cases the fill is unknown, and
    /// only a proven on-chain failure (`FailedOnChain`) means no fill.
    /// First receipt wins per mint: a second pending buy for the same
    /// mint is logged loudly for manual review instead of silently
    /// dropping the first bag's tracking.
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
            // A poisoned lock means a prior panic already fired: losing
            // the bag's tracking silently would strand real funds without
            // exit automation — escalate loudly instead.
            Err(_) => tracing::warn!(mint = %mint, bundle = %receipt.bundle_id,
                "pending-tracker lock poisoned; buy fill untracked, MANUAL REVIEW REQUIRED"),
        }
    }

    /// Sell the full tracked amount at `price_usd` via Jito bundle.
    /// Returns the bundle UUID.
    pub async fn sell(
        &self,
        position: &LivePosition,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<String, LiveError> {
        let floor = plan_sell_floor(
            position.amount_tokens,
            price_usd,
            sol_usd,
            self.slippage_bps,
        )
        .ok_or_else(|| LiveError::BadOrder(format!("cannot floor sell of {}", position.mint)))?;
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
                tip_lamports: self.tip_lamports,
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
        match self
            .executor
            .confirm_signature(&receipt.signature, LIVE_CONFIRM_TIMEOUT)
            .await
        {
            Ok(ConfirmOutcome::Confirmed) => Ok(receipt.bundle_id),
            Ok(ConfirmOutcome::FailedOnChain) => Err(LiveError::Unconfirmed(format!(
                "sell bundle {} errored on-chain; still holding",
                receipt.bundle_id
            ))),
            Ok(ConfirmOutcome::Timeout) => Err(LiveError::Unconfirmed(format!(
                "sell bundle {} unconfirmed after {:?}; still holding",
                receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
            ))),
            // Transport failure mid-confirm: the bundle may already have
            // landed, so report it as unconfirmed-but-holding (same retry
            // path as a timeout) rather than a dispatch error.
            Err(e) => Err(LiveError::Unconfirmed(format!(
                "sell bundle {} confirm RPC failed ({e}); still holding",
                receipt.bundle_id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_plan_math() {
        // $10 at $0.0000042 ≈ 2.38M tokens → 2_380_952_380_952 base units.
        let plan = plan_buy(10.0, 0.000_004_2, 150.0, 200).expect("plan");
        assert_eq!(plan.amount_tokens, 2_380_952_380_952);
        // $10 ≈ 66.67M lamports + 2% slippage guard.
        assert_eq!(plan.max_sol_cost, 68_000_000);
    }

    #[test]
    fn buy_plan_rejects_degenerate() {
        assert_eq!(plan_buy(0.0, 1.0, 150.0, 200), None);
        assert_eq!(plan_buy(10.0, 0.0, 150.0, 200), None);
        assert_eq!(plan_buy(10.0, 1.0, 0.0, 200), None);
        assert_eq!(plan_buy(f64::NAN, 1.0, 150.0, 200), None);
        // Dust that rounds to zero units.
        assert_eq!(plan_buy(1e-12, 1.0, 150.0, 200), None);
    }

    #[test]
    fn sell_floor_math() {
        // 1e6 units (1 token) at $2, SOL $150 → 13_333_333 lamports − 2%.
        let floor = plan_sell_floor(1_000_000, 2.0, 150.0, 200).expect("floor");
        assert_eq!(floor, 13_066_666);
    }

    #[test]
    fn buy_plan_rejects_overflow() {
        // 1e36 base units does not fit u64: fail closed, never saturate.
        assert_eq!(plan_buy(1e20, 1e-10, 150.0, 200), None);
        // Oversized spend cap (amount fits, cap does not): same treatment.
        assert_eq!(plan_buy(1e13, 1e6, 150.0, 200), None);
    }

    #[test]
    fn sell_floor_rejects_zero_floor() {
        // 100% slippage authorizes any fill price: rejected outright.
        assert_eq!(plan_sell_floor(1_000_000, 2.0, 150.0, 10_000), None);
        assert_eq!(plan_sell_floor(1_000_000, 2.0, 150.0, 10_001), None);
        // Sub-lamport dust floors to 0: no unprotected dispatch.
        assert_eq!(plan_sell_floor(1, 1e-9, 1e9, 200), None);
    }

    #[test]
    fn sell_floor_rejects_degenerate() {
        assert_eq!(plan_sell_floor(0, 2.0, 150.0, 200), None);
        assert_eq!(plan_sell_floor(100, 0.0, 150.0, 200), None);
    }
}
