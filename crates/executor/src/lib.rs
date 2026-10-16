//! Live Pump.fun executor (v3 only).
//!
//! [`WalletGuard`] holds the keypair and the 0.05 SOL reserve.
//! [`PumpSwapBuilder`] builds; [`SwapExecutor`] signs and confirms.
//! See `README.md` before funding a wallet.

pub mod executor;
pub mod instructions;
pub mod quote;
pub mod jito;
pub mod raydium;
pub mod wallet;

pub use executor::{
    BuyOrder, ConfirmOutcome, ExecutorError, JitoReceipt,
    LiveFundingReport, SellOrder, SellPumpSwapOrder, SellRaydiumOrder, SwapExecutor,
};
pub use instructions::{BuyV3Accounts, PumpAmmSellV2Accounts, PumpBuyV3Args, PumpSellV3Args, PumpBuyExactQuoteInV3Args, PumpSwapBuilder, SellV3Accounts, parse_pubkey, user_ata_with_program};
pub use quote::{CurveView, FeeSchedule, FeesBps, QuoteError, QuoteKind};
pub use jito::{BundleStatus, JITO_TIP_ACCOUNTS, JitoClient, JitoError, apply_tip_floor, bundle_payload, tip_floor_parse_failures};
pub use raydium::{
    AMM_V4_FEE_BPS, SWAP_BASE_IN_V2_TAG, build_swap_base_in_v2_ix, quote_base_in,
    quote_base_in_std,
};
pub use wallet::{MIN_SOL_RESERVE_LAMPORTS, WalletGuard, WalletGuardError};
