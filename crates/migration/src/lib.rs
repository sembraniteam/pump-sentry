//! Graduation and migration detector.
//!
//! Pure parsing: no I/O, no panics.

pub mod detector;
pub mod pool;
pub mod pumpswap;
pub mod vault;

pub use detector::{
    GRADUATION_REAL_SOL_LAMPORTS, MigrationDestination, MigrationError, MigrationMonitor,
    PUMP_AMM_PROGRAM_ID, PUMP_MIGRATE_DISCRIMINATOR, PUMP_MIGRATE_V2_LOG, RAYDIUM_V4_PROGRAM_ID,
};
pub use pool::{RaydiumV4Pool, WSOL_MINT};
pub use pumpswap::{PumpSwapPool, virtual_reserves};
pub use vault::{PoolTick, VaultTicker};

/// Parsed migration pool, either venue. Vault/mint accessors speak
/// the WSOL-side-first orientation: BASE is WSOL, QUOTE the token
/// (PumpSwap stores them the other way round).
#[derive(Debug, Clone)]
pub enum MigratedPool {
    /// Raydium AMM V4 pool (legacy venue).
    RaydiumV4(RaydiumV4Pool),
    /// PumpSwap pool (live venue).
    PumpSwap(PumpSwapPool),
}

impl MigratedPool {
    /// WSOL-side vault (price numerator).
    pub fn base_vault(&self) -> solana_pubkey::Pubkey {
        match self {
            Self::RaydiumV4(p) => p.base_vault,
            Self::PumpSwap(p) => p.quote_vault,
        }
    }

    /// Token-side vault (price denominator).
    pub fn quote_vault(&self) -> solana_pubkey::Pubkey {
        match self {
            Self::RaydiumV4(p) => p.quote_vault,
            Self::PumpSwap(p) => p.base_vault,
        }
    }

    /// WSOL-side mint.
    pub fn base_mint(&self) -> solana_pubkey::Pubkey {
        match self {
            Self::RaydiumV4(p) => p.base_mint,
            Self::PumpSwap(p) => p.quote_mint,
        }
    }

    /// Token-side mint.
    pub fn quote_mint(&self) -> solana_pubkey::Pubkey {
        match self {
            Self::RaydiumV4(p) => p.quote_mint,
            Self::PumpSwap(p) => p.base_mint,
        }
    }
}
