//! Pump.fun bonding-curve reserve reader and price ticker.
//!
//! [`layout`] parses the on-chain `BondingCurve` account
//! (discriminator + 5×u64 reserves + `complete` flag) into prices and
//! liquidity. [`CurveTicker`] polls watched curves over HTTP RPC and
//! emits [`PriceTick`]s for the exit engine.

pub mod layout;
pub mod ticker;

pub use layout::{BondingCurveReserves, CurveParseError, BONDING_CURVE_MIN_LEN, COMPLETE_FLAG_OFFSET, PUMP_TOKEN_DECIMALS, TOKEN_SUPPLY_LEN, TOKEN_SUPPLY_OFFSET};
pub use ticker::{CurveError, CurveTicker, PriceTick};
