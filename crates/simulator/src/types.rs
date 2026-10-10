//! Core simulator types: positions, closed trades, exit reasons.

use serde::{Deserialize, Serialize};

/// Unique position handle issued by [`PaperPortfolio`](crate::PaperPortfolio).
pub type PositionId = u64;

/// Trade direction. The snipe pipeline only opens [`Side::Long`];
/// [`Side::Short`] exists so the PnL math stays general.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// Buy first, profit when the price rises.
    Long,
    /// Sell first, profit when the price falls.
    Short,
}

/// Why a position was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    /// Take-profit target hit.
    TakeProfit,
    /// Stop-loss trigger hit.
    StopLoss,
    /// Trailing stop triggered after giving back gains.
    TrailingStop,
    /// Holding timeout elapsed.
    Timeout,
    /// Explicit manual close.
    Manual,
}

/// An open paper position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Handle assigned at open.
    pub id: PositionId,
    /// Token mint (base58).
    pub mint: String,
    /// Direction.
    pub side: Side,
    /// Executed entry price in USD per token (post-slippage, pre-fee).
    pub entry_price: f64,
    /// Token quantity held (signed: negative for shorts).
    pub quantity: f64,
    /// Entry notional in USD (always non-negative).
    pub entry_notional: f64,
    /// Fees paid at entry in USD.
    pub entry_fee: f64,
    /// Take-profit target in percent (e.g. `75.0` = +75% for longs).
    pub tp_percent: f64,
    /// Stop-loss trigger in percent (negative, e.g. `-8.0`).
    pub sl_percent: f64,
    /// Trail distance in percent when `trailing_stop` is set.
    pub trail_percent: f64,
    /// Whether the trailing stop is armed.
    pub trailing_stop: bool,
    /// Best favorable excursion since open (for trailing stops).
    pub best_price: f64,
    /// Open timestamp (caller-provided unix seconds).
    pub opened_at: u64,
    /// Holding timeout in seconds (`0` = no timeout).
    pub timeout_seconds: u64,
}

/// A finished round trip with final accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedTrade {
    /// Position this trade closes.
    pub id: PositionId,
    /// Token mint (base58).
    pub mint: String,
    /// Direction.
    pub side: Side,
    /// Executed entry price in USD per token.
    pub entry_price: f64,
    /// Executed exit price in USD per token.
    pub exit_price: f64,
    /// Token quantity (signed).
    pub quantity: f64,
    /// Gross PnL in USD (before fees).
    pub gross_pnl: f64,
    /// Total fees (entry + exit) in USD.
    pub fees: f64,
    /// Net PnL in USD (`gross_pnl - fees`).
    pub net_pnl: f64,
    /// Net PnL as percent of entry notional.
    pub net_pnl_percent: f64,
    /// What closed the position.
    pub reason: ExitReason,
    /// Open timestamp (unix seconds).
    pub opened_at: u64,
    /// Close timestamp (unix seconds).
    pub closed_at: u64,
    /// Hold duration in seconds.
    pub hold_seconds: u64,
}
