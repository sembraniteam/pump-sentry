//! Paper-trading simulator: virtual balances, active positions, PnL.
//!
//! [`PaperPortfolio`] is the entry point: fund it once (e.g. $100),
//! open/close [`Position`]s at quoted prices, and read realized /
//! unrealized PnL at any time. [`FillModel`] reproduces bonding-curve
//! slippage so fills are worse than the quote; [`check_exit`] evaluates
//! TP / SL / trailing-stop / timeout triggers.

pub mod market;
pub mod portfolio;
pub mod types;

pub use market::{FillModel, check_exit};
pub use portfolio::{Mark, OpenPositionParams, PaperPortfolio, PortfolioError, PortfolioSnapshot};
pub use types::{ClosedTrade, ExitReason, Position, PositionId, Side};
