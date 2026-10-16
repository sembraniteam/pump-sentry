//! Bonding-curve reader and price ticker.

pub mod backoff;
pub mod fee;
pub mod layout;
pub mod sol_feed;
pub mod ticker;

pub use backoff::{RoundBackoff, warn_throttled};
pub use fee::{
    FEE_CACHE_TTL, FEE_TIMEOUT, FeeInputsCache, FeeSchedule, FeeTierRow, FeesBps, SupplyCache,
    market_cap,
};

pub use layout::{
    BONDING_CURVE_DISCRIMINATOR, BONDING_CURVE_MIN_LEN, BondingCurveReserves, COMPLETE_FLAG_OFFSET,
    CurveParseError, PUMP_TOKEN_DECIMALS, TOKEN_SUPPLY_LEN, TOKEN_SUPPLY_OFFSET,
};
pub use sol_feed::{DEFAULT_JUP_URL, SOL_MINT, SolUsdFeed};
pub use ticker::{CurveError, CurveTicker, PriceTick};
