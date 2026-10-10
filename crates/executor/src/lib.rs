//! Live Pump.fun buy/sell executor.
//!
//! [`wallet::WalletGuard`] loads the trading keypair from
//! `SOLANA_PRIVATE_KEY` and enforces the 0.05 SOL reserve (skill
//! `solana-wallet-guard`). [`instructions::PumpSwapBuilder`] builds the
//! exact buy/sell instructions (skill `solana-pumpfun-swap`).
//! [`executor::SwapExecutor`] signs and confirms them.
//!
//! Live funds move here — this crate is deliberately *not* wired into the
//! paper binary. See `README.md` before funding a wallet.

pub mod executor;
pub mod instructions;
pub mod wallet;

pub use executor::{BuyOrder, ExecutorError, SellOrder, SwapExecutor, SwapReceipt};
pub use instructions::{PumpBuyArgs, PumpSellArgs, PumpSwapBuilder, user_ata};
pub use wallet::{MIN_SOL_RESERVE_LAMPORTS, WalletGuard, WalletGuardError};
