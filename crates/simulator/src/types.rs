//! Core simulator types.

use serde::{Deserialize, Serialize};

/// Position handle.
pub type PositionId = u64;

/// Trade direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Long,
    Short,
}

/// Why a position was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    TrailingStop,
    Timeout,
    Manual,
}

/// An open paper position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: PositionId,
    pub mint: String,
    pub side: Side,
    pub entry_price: f64,
    pub quantity: f64,
    pub entry_notional: f64,
    pub entry_fee: f64,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub trail_percent: f64,
    pub trailing_stop: bool,
    pub best_price: f64,
    pub opened_at: u64,
    pub timeout_seconds: u64,
    pub moonbag_percent: f64,
}

/// A finished round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedTrade {
    pub id: PositionId,
    pub mint: String,
    pub side: Side,
    pub entry_price: f64,
    pub exit_price: f64,
    pub quantity: f64,
    pub gross_pnl: f64,
    pub fees: f64,
    pub net_pnl: f64,
    pub net_pnl_percent: f64,
    pub reason: ExitReason,
    pub fraction_closed: f64,
    pub opened_at: u64,
    pub closed_at: u64,
    pub hold_seconds: u64,
    /// Dispatch bleed (tip/base/rent in USD) attributed to this close.
    /// Zero for pure-paper closes; live legs attribute actual tips here
    /// so headline PnL never hides the round-trip drag.
    #[serde(default)]
    pub dispatch_usd: f64,
}
