//! Pump.fun graduation and Raydium migration detector (skill
//! `solana-raydium-migration`).
//!
//! [`detector::MigrationMonitor`] spots graduation (bonding-curve
//! `complete` flag) and classifies migration transactions by destination.
//! [`pool::RaydiumV4Pool`] parses fresh Raydium AMM V4 pool accounts into
//! swap-ready addresses. [`vault::VaultTicker`] polls pool vault balances
//! for post-migration USD pricing. Pure parsing: no I/O, no panics.

pub mod detector;
pub mod pool;
pub mod vault;

pub use detector::{
    GRADUATION_REAL_SOL_LAMPORTS, MigrationDestination, MigrationError, MigrationMonitor,
    PUMP_MIGRATE_DISCRIMINATOR, PUMP_MIGRATE_V2_LOG, RAYDIUM_V4_PROGRAM_ID,
};
pub use pool::{RaydiumV4Pool, WSOL_MINT};
pub use vault::{PoolTick, VaultTicker};
