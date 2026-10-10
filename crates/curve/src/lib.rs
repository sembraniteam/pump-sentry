//! Pump.fun bonding-curve reserve reader and price ticker.
//!
//! [`layout`] parses the on-chain `BondingCurve` account
//! (discriminator + 5×u64 reserves + `complete` flag) into prices and
//! liquidity. [`CurveTicker`] polls watched curves over HTTP RPC and
//! emits [`PriceTick`]s for the exit engine.

pub mod layout;
pub mod ticker;

pub use layout::{BondingCurveReserves, CurveParseError, PUMP_TOKEN_DECIMALS};
pub use ticker::{CurveError, CurveTicker, PriceTick};
