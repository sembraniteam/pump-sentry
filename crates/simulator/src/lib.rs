//! Paper-trading simulator.

pub mod market;
pub mod portfolio;
pub mod types;

pub use market::{
    CurveReserves, EntryFill, EntryFillError, ExitProceeds, FillModel, FlipPreview, Mark,
    STANDARD_VENUE_FEE_BPS, check_exit, check_exit_realizable,
};
pub use portfolio::{
    OpenPositionParams, PaperPortfolio, PortfolioError, PortfolioSnapshot, RUNNER_BACKSTOP_SECS,
    slice_quantity,
};
pub use types::{ClosedTrade, ExitReason, Position, PositionId, Side};
