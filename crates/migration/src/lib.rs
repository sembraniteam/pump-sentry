//! Graduation and migration detector.
//!
//! Pure parsing: no I/O, no panics.

pub mod detector;
pub mod pool;
pub mod vault;

pub use detector::{
    GRADUATION_REAL_SOL_LAMPORTS, MigrationDestination, MigrationError, MigrationMonitor,
    PUMP_MIGRATE_DISCRIMINATOR, PUMP_MIGRATE_V2_LOG, RAYDIUM_V4_PROGRAM_ID,
};
pub use pool::{RaydiumV4Pool, WSOL_MINT};
pub use vault::{PoolTick, VaultTicker};
