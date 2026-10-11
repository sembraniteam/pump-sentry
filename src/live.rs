//! Live execution sink: USD sizing math plus Jito-routed dispatch.
//!
//! [`LiveTrader`] wraps [`SwapExecutor`](executor::SwapExecutor) with the
//! pipeline's USD-denominated sizing. [`plan_buy`] / [`plan_sell`] are pure
//! (unit-tested); only [`LiveTrader::buy`] / [`sell`](LiveTrader::sell)
//! touch the network.

use executor::{BuyOrder, ExecutorError, SellOrder, SwapExecutor, parse_pubkey};
use thiserror::Error;

/// Pump.fun mint decimals (base-unit scale).
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
}

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
/// spend cap from `notional/sol` plus `slippage_bps`. `None` on
/// non-positive inputs or zero quantity.
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
    let amount_tokens = (notional_usd / price_usd * scale).floor() as u64;
    if amount_tokens == 0 {
        return None;
    }
    let lamports = notional_usd / sol_usd * LAMPORTS_PER_SOL;
    let max_sol_cost = (lamports * (10_000.0 + slippage_bps as f64) / 10_000.0).ceil() as u64;
    Some(LiveBuyPlan {
        amount_tokens,
        max_sol_cost,
    })
}

/// Floor for a sell: expected proceeds minus `slippage_bps`. `None` on
/// degenerate inputs.
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
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    let lamports = amount_tokens as f64 / scale * price_usd / sol_usd * LAMPORTS_PER_SOL;
    let floor = (lamports * (10_000.0 - slippage_bps.min(10_000) as f64) / 10_000.0).floor() as u64;
    Some(floor)
}

/// Jito-routed live trader over a configured [`SwapExecutor`].
#[derive(Clone)]
pub struct LiveTrader {
    executor: SwapExecutor,
    slippage_bps: u64,
    tip_lamports: u64,
}

impl LiveTrader {
    /// Wrap an executor (already Jito-attached) with sizing policy.
    pub fn new(executor: SwapExecutor, slippage_bps: u64, tip_lamports: u64) -> Self {
        Self {
            executor,
            slippage_bps,
            tip_lamports,
        }
    }

    /// Buy `notional_usd` at `price_usd` via Jito bundle.
    /// Returns the tracked position plus the bundle UUID.
    pub async fn buy(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        notional_usd: f64,
        price_usd: f64,
        sol_usd: f64,
    ) -> Result<(LivePosition, String), LiveError> {
        let plan =
            plan_buy(notional_usd, price_usd, sol_usd, self.slippage_bps).ok_or_else(|| {
                LiveError::BadOrder(format!("cannot size buy of {mint} at {price_usd}"))
            })?;
        let receipt = self
            .executor
            .buy_jito(&BuyOrder {
                mint: parse_pubkey(mint).map_err(|e| LiveError::BadOrder(e.to_string()))?,
                bonding_curve: parse_pubkey(curve)
                    .map_err(|e| LiveError::BadOrder(e.to_string()))?,
                associated_bonding_curve: parse_pubkey(assoc_curve)
                    .map_err(|e| LiveError::BadOrder(e.to_string()))?,
                amount_tokens: plan.amount_tokens,
                max_sol_cost: plan.max_sol_cost,
                tip_lamports: self.tip_lamports,
            })
            .await?;
        tracing::info!(%mint, bundle = %receipt.bundle_id, amount = plan.amount_tokens, "LIVE buy dispatched");
        Ok((
            LivePosition {
                mint: mint.to_string(),
                curve: curve.to_string(),
                assoc_curve: assoc_curve.to_string(),
                amount_tokens: plan.amount_tokens,
            },
            receipt.bundle_id,
        ))
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
        let receipt = self
            .executor
            .sell_jito(&SellOrder {
                mint: parse_pubkey(&position.mint)
                    .map_err(|e| LiveError::BadOrder(e.to_string()))?,
                bonding_curve: parse_pubkey(&position.curve)
                    .map_err(|e| LiveError::BadOrder(e.to_string()))?,
                associated_bonding_curve: parse_pubkey(&position.assoc_curve)
                    .map_err(|e| LiveError::BadOrder(e.to_string()))?,
                amount_tokens: position.amount_tokens,
                min_sol_output: floor,
                tip_lamports: self.tip_lamports,
            })
            .await?;
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE sell dispatched");
        Ok(receipt.bundle_id)
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
    fn sell_floor_rejects_degenerate() {
        assert_eq!(plan_sell_floor(0, 2.0, 150.0, 200), None);
        assert_eq!(plan_sell_floor(100, 0.0, 150.0, 200), None);
    }
}
