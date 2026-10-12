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
pub mod jito;
pub mod raydium;
pub mod wallet;

pub use executor::{
    BuyOrder, ConfirmOutcome, ExecutorError, JitoReceipt, SellOrder, SellV2Reference,
    SwapExecutor, SwapReceipt, select_sell_v2_reference,
};
pub use instructions::{PumpBuyArgs, PumpSellArgs, PumpSellV2Args, PumpSwapBuilder, SellV2Accounts, SwapAccounts, SwapKind, parse_pubkey, user_ata, user_ata_with_program};
pub use jito::{JITO_TIP_ACCOUNTS, JitoClient, JitoError, bundle_payload};
pub use raydium::{AMM_V4_FEE_BPS, SWAP_BASE_IN_V2_TAG, build_swap_base_in_v2_ix, quote_base_in, quote_base_in_std};
pub use wallet::{MIN_SOL_RESERVE_LAMPORTS, WalletGuard, WalletGuardError};
