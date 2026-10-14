//! Paper-trading simulator.

pub mod market;
pub mod portfolio;
pub mod types;

pub use market::{FillModel, STANDARD_VENUE_FEE_BPS, check_exit, check_exit_realizable};
pub use portfolio::{Mark, OpenPositionParams, PaperPortfolio, PortfolioError, PortfolioSnapshot, RUNNER_BACKSTOP_SECS};
pub use types::{ClosedTrade, ExitReason, Position, PositionId, Side};
