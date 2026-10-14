//! Live Pump.fun buy/sell executor.
//!
//! [`WalletGuard`] loads the trading keypair from
//! `SOLANA_PRIVATE_KEY` and enforces the 0.05 SOL reserve (skill
//! `solana-wallet-guard`). [`PumpSwapBuilder`] builds the
//! exact buy/sell instructions (skill `solana-pumpfun-swap`).
//! [`SwapExecutor`] signs and confirms them.
//!
//! Live funds move here — this crate is deliberately *not* wired into the
//! paper binary. See `README.md` before funding a wallet.

pub mod executor;
pub mod instructions;
pub mod quote;
pub mod jito;
pub mod raydium;
pub mod wallet;

pub use executor::{
    BuyOrder, ConfirmOutcome, ExecutorError, JitoReceipt,
    LiveFundingReport, SellOrder, SellRaydiumOrder, SwapExecutor, SwapReceipt,
};
pub use instructions::{BuyV3Accounts, PumpBuyV3Args, PumpSellV3Args, PumpBuyExactQuoteInV3Args, PumpSwapBuilder, SellV3Accounts, parse_pubkey, user_ata, user_ata_with_program};
pub use quote::{CurveView, FeeSchedule, FeesBps, QuoteError, QuoteKind};
pub use jito::{JITO_TIP_ACCOUNTS, JitoClient, JitoError, bundle_payload};
pub use raydium::{
    AMM_V4_FEE_BPS, SWAP_BASE_IN_V2_TAG, build_swap_base_in_v2_ix, quote_base_in,
    quote_base_in_std,
};
pub use wallet::{MIN_SOL_RESERVE_LAMPORTS, WalletGuard, WalletGuardError};
