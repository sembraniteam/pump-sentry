//! Swap dispatch: solvency-guarded build, sign, and confirm.
//!
//! Every order runs [`WalletGuard::assert_solvency`] first — a broke or
//! underfunded wallet fails *before* any instruction is built. Sends go
//! through standard RPC confirmation; Jito bundles and priority fees are
//! explicit TODOs (see `README.md`).

use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::sync::Arc;
use thiserror::Error;
use tracing::{info, warn};

use crate::instructions::PumpSwapBuilder;
use crate::instructions::user_ata;
use crate::wallet::{WalletGuard, WalletGuardError};

/// Executor failures: config, guard, build, and RPC errors.
#[derive(Error, Debug)]
pub enum ExecutorError {
    /// RPC endpoint URL is invalid.
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    /// Wallet guard rejected the order.
    #[error("wallet guard: {0}")]
    Wallet(#[from] WalletGuardError),
    /// RPC failure (blockhash, send, confirm).
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
}

/// Live buy parameters (base units and lamports — no floats).
#[derive(Debug, Clone)]
pub struct BuyOrder {
    /// Token mint.
    pub mint: Pubkey,
    /// Bonding-curve PDA.
    pub bonding_curve: Pubkey,
    /// Curve's associated token account.
    pub associated_bonding_curve: Pubkey,
    /// Token base units to buy.
    pub amount_tokens: u64,
    /// Slippage guard: max lamports to spend.
    pub max_sol_cost: u64,
    /// Reserved tip budget in lamports (see README: currently reserved,
    /// not routed, until Jito support lands).
    pub tip_lamports: u64,
}

/// Live sell parameters.
#[derive(Debug, Clone)]
pub struct SellOrder {
    /// Token mint.
    pub mint: Pubkey,
    /// Bonding-curve PDA.
    pub bonding_curve: Pubkey,
    /// Curve's associated token account.
    pub associated_bonding_curve: Pubkey,
    /// Token base units to sell.
    pub amount_tokens: u64,
    /// Slippage guard: min lamports to accept.
    pub min_sol_output: u64,
    /// Reserved tip budget in lamports (see [`BuyOrder::tip_lamports`]).
    pub tip_lamports: u64,
}

/// Confirmed swap receipt.
#[derive(Debug, Clone)]
pub struct SwapReceipt {
    /// Confirmed transaction signature (base58).
    pub signature: String,
}

/// Live swap executor. Cloneable; the RPC client is shared.
#[derive(Clone)]
pub struct SwapExecutor {
    rpc: Arc<RpcClient>,
    wallet: Arc<WalletGuard>,
}

impl SwapExecutor {
    /// Build against an HTTP RPC URL with explicit commitment.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        wallet: WalletGuard,
    ) -> Result<Self, ExecutorError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(ExecutorError::BadUrl(http_url.to_string())),
        }
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            wallet: Arc::new(wallet),
        })
    }

    /// Trading address.
    pub fn pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    /// Buy `amount_tokens` for at most `max_sol_cost` lamports.
    pub async fn buy(&self, order: &BuyOrder) -> Result<SwapReceipt, ExecutorError> {
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, order.max_sol_cost, order.tip_lamports)
            .await?;
        info!(mint = %order.mint, balance, max_cost = order.max_sol_cost, "buy pre-flight ok");
        let ata = user_ata(&self.wallet.pubkey(), &order.mint);
        let ix = PumpSwapBuilder::build_buy_ix(
            &self.wallet.pubkey(),
            &order.mint,
            &order.bonding_curve,
            &order.associated_bonding_curve,
            &ata,
            order.amount_tokens,
            order.max_sol_cost,
        );
        self.send(&[ix], "buy").await
    }

    /// Sell `amount_tokens` for at least `min_sol_output` lamports.
    pub async fn sell(&self, order: &SellOrder) -> Result<SwapReceipt, ExecutorError> {
        // Sells spend no SOL beyond fees; still enforce reserve + tip.
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, order.tip_lamports)
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "sell pre-flight ok");
        let ata = user_ata(&self.wallet.pubkey(), &order.mint);
        let ix = PumpSwapBuilder::build_sell_ix(
            &self.wallet.pubkey(),
            &order.mint,
            &order.bonding_curve,
            &order.associated_bonding_curve,
            &ata,
            order.amount_tokens,
            order.min_sol_output,
        );
        self.send(&[ix], "sell").await
    }

    /// Sign with the guarded keypair and confirm. Single-flight per call.
    async fn send(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<SwapReceipt, ExecutorError> {
        let blockhash = self.rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&self.wallet.pubkey()),
            &[self.wallet.keypair()],
            blockhash,
        );
        match self.rpc.send_and_confirm_transaction(&tx).await {
            Ok(signature) => {
                info!(%label, %signature, "swap confirmed");
                Ok(SwapReceipt {
                    signature: signature.to_string(),
                })
            }
            Err(e) => {
                warn!(%label, error = %e, "swap failed");
                Err(ExecutorError::Rpc(e))
            }
        }
    }
}
