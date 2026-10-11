//! Swap dispatch: solvency-guarded build, sign, simulate, and confirm.
//!
//! Every order runs [`WalletGuard::assert_solvency`] first — a broke or
//! underfunded wallet fails *before* any instruction is built — then a
//! `simulateTransaction` preflight rejects definitive failures before
//! fees burn (transport trouble proceeds with a log). Sends go
//! through standard RPC confirmation (`buy`/`sell`) or Jito bundles
//! (`buy_jito`/`sell_jito` with an in-bundle tip). Priority fees attach
//! via `with_compute_budget`.

use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::{str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tracing::{info, warn};

use crate::instructions::{PumpSwapBuilder, SwapKind, user_ata_with_program};
use crate::jito::{
    JitoClient, JitoError, encode_bundle, nanos_seed, select_tip_account, tip_instruction,
};
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
    /// Jito dispatch failure.
    #[error("jito error: {0}")]
    Jito(#[from] JitoError),
    /// Jito route requested but no bundle client configured.
    #[error("jito client not configured; call with_jito first")]
    NoJito,
    /// Signature is not valid base58: {value} ({msg}).
    #[error("bad signature '{value}': {msg}")]
    BadSignature {
        /// Offending signature string.
        value: String,
        /// Parse failure message.
        msg: String,
    },
    /// Transaction has no signatures after signing (refusing to dispatch).
    #[error("unsigned transaction: refusing to dispatch")]
    UnsignedTransaction,
    /// Simulation proved the transaction would fail (refusing to burn fees).
    #[error("simulateTransaction failed: {0}")]
    SimFailed(String),
    /// Order amounts are degenerate (refusing a doomed transaction).
    #[error("bad order: {0}")]
    BadOrder(String),
    /// RPC call exceeded its deadline.
    #[error("rpc call timed out: {0}")]
    Timeout(&'static str),
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
    /// Jito tip budget in lamports, paid as an in-bundle system transfer
    /// on `buy_jito`/`sell_jito` (plain-RPC `buy`/`sell` ignore it).
    pub tip_lamports: u64,
    /// Token creator, when known: selects the extended mayhem swap shape
    /// (creator vault + volume/fee routing). `None` keeps the legacy core.
    pub creator: Option<Pubkey>,
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
    /// Jito tip budget in lamports (see [`BuyOrder::tip_lamports`]).
    pub tip_lamports: u64,
    /// Token creator, when known: selects the extended mayhem swap shape.
    pub creator: Option<Pubkey>,
}

/// Fill landing outcome for a dispatched signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Landed and succeeded on-chain.
    Confirmed,
    /// Landed but the transaction errored (no fill).
    FailedOnChain,
    /// Still unknown after the deadline.
    Timeout,
}

/// Confirmed swap receipt.
#[derive(Debug, Clone)]
pub struct SwapReceipt {
    /// Confirmed transaction signature (base58).
    pub signature: String,
}

/// Accepted Jito bundle receipt (acceptance ≠ landing: the bundle may
/// still miss its slot; track the signature separately).
#[derive(Debug, Clone)]
pub struct JitoReceipt {
    /// Block-engine bundle UUID.
    pub bundle_id: String,
    /// First transaction signature (base58) for tracking.
    pub signature: String,
}

/// Live swap executor. Cloneable; clients and wallet are shared.
#[derive(Clone)]
pub struct SwapExecutor {
    rpc: Arc<RpcClient>,
    wallet: Arc<WalletGuard>,
    jito: Option<JitoClient>,
    compute_unit_limit: u32,
    compute_unit_price: u64,
    /// Explicit fee-recipient override (`None` = resolve from Global at
    /// preflight, canonical constant as fallback; the program rotates
    /// recipients so hardcoding fails closed into fee misdirection).
    fee_recipient_override: Option<Pubkey>,
}

/// Global account layout: 8-byte Anchor discriminator, then
/// `initialized`, `authority`, `fee_recipient` — the recipient slot is
/// bytes 41..73. (An older client-SDK doc quoted 33..65, i.e. the same
/// struct WITHOUT the discriminator prefix; reading there yields a
/// garbage span that the program rejects with `NotAuthorized`. Verified
/// live: 41..73 currently holds a rotated recipient seen the same day
/// in real sells.)
pub const GLOBAL_FEE_RECIPIENT_OFFSET: usize = 41;
/// End offset (exclusive) of the fee-recipient slot in Global data.
pub const GLOBAL_FEE_RECIPIENT_END: usize = 73;

/// Read the current fee recipient from the Global account. `None` on any
/// failure (transport, short data): callers fall back to the canonical
/// constant. Deadline-bound.
pub async fn fetch_global_fee_recipient(rpc: &RpcClient) -> Option<Pubkey> {
    let account = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        rpc.get_account(&crate::instructions::GLOBAL_ACCOUNT),
    )
    .await
    {
        Ok(Ok(account)) => account,
        _ => return None,
    };
    let bytes: [u8; 32] = account
        .data
        .get(GLOBAL_FEE_RECIPIENT_OFFSET..GLOBAL_FEE_RECIPIENT_END)?
        .try_into()
        .ok()?;
    Some(Pubkey::new_from_array(bytes))
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
            jito: None,
            compute_unit_limit: 0,
            compute_unit_price: 0,
            fee_recipient_override: None,
        })
    }

    /// Attach a Jito bundle client for protected dispatch.
    pub fn with_jito(mut self, jito: JitoClient) -> Self {
        self.jito = Some(jito);
        self
    }

    /// Override the fee-recipient account used in swap instructions.
    /// Without this, the recipient resolves from the Global account at
    /// preflight time (canonical constant as fallback). Set explicitly
    /// only to pin a recipient (e.g. tests, emergencies).
    pub fn with_fee_recipient(mut self, fee_recipient: Pubkey) -> Self {
        self.fee_recipient_override = Some(fee_recipient);
        self
    }

    /// Resolve the fee recipient: explicit override, else the Global
    /// account's current recipient, else the canonical constant.
    /// Deadline-bound; never fails (worst case is the constant).
    async fn fee_recipient(&self) -> Pubkey {
        if let Some(pinned) = self.fee_recipient_override {
            return pinned;
        }
        if let Some(live) = fetch_global_fee_recipient(&self.rpc).await {
            return live;
        }
        crate::instructions::FEE_RECIPIENT
    }

    /// Resolve the token program owning `mint` (classic SPL vs
    /// Token-2022): v2/mayhem mints live under Token-2022 and a swap
    /// naming the wrong program fails on-chain. Fail-closed (`None`) when
    /// the mint account is unreadable — building with a guessed program
    /// burns fees for certain.
    async fn token_program(&self, mint: &Pubkey) -> Option<Pubkey> {
        let account = match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_account(mint),
        )
        .await
        {
            Ok(Ok(account)) => account,
            _ => return None,
        };
        Some(account.owner)
    }

    /// Attach a compute budget: `SetComputeUnitLimit` + `SetComputeUnitPrice`
    /// are prepended to every built transaction (limit first, then price,
    /// per runtime convention). Zero values disable that instruction, so
    /// the default executor builds byte-identical transactions to before.
    pub fn with_compute_budget(mut self, unit_limit: u32, unit_price_micro_lamports: u64) -> Self {
        self.compute_unit_limit = unit_limit;
        self.compute_unit_price = unit_price_micro_lamports;
        self
    }

    /// Compute-budget prefix for the next transaction (empty by default).
    fn budget_prefix(&self) -> Vec<solana_instruction::Instruction> {
        crate::instructions::compute_budget_ixs(self.compute_unit_limit, self.compute_unit_price)
    }
}

impl SwapExecutor {
    /// Trading address.
    pub fn pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    /// Poll a dispatched signature until it lands or `timeout` elapses.
    /// `Some(Ok(()))` (at the client commitment) is a fill;
    /// `Some(Err(_))` landed-but-errored; `None` is still unknown.
    /// Never panics; transport errors propagate.
    pub async fn confirm_signature(
        &self,
        signature_str: &str,
        timeout: Duration,
    ) -> Result<ConfirmOutcome, ExecutorError> {
        use std::time::Instant;
        let signature = solana_signature::Signature::from_str(signature_str).map_err(|e| {
            ExecutorError::BadSignature {
                value: signature_str.to_string(),
                msg: e.to_string(),
            }
        })?;
        let start = Instant::now();
        loop {
            // Per-poll deadline: one hung RPC must stall only this round,
            // never the whole confirmation loop (the outer `timeout` is the
            // total budget, this is the liveness guard per attempt).
            let status = match tokio::time::timeout(
                Duration::from_secs(15),
                self.rpc.get_signature_status(&signature),
            )
            .await
            {
                Ok(Ok(status)) => status,
                // Transport error: fail fast (caller decides retry/fallback).
                Ok(Err(e)) => return Err(e.into()),
                // Poll hung: treat as still-unknown and keep waiting only
                // while the total budget allows.
                Err(_) => None,
            };
            match status {
                Some(Ok(())) => return Ok(ConfirmOutcome::Confirmed),
                Some(Err(_)) => return Ok(ConfirmOutcome::FailedOnChain),
                None => {}
            }
            if start.elapsed() >= timeout {
                return Ok(ConfirmOutcome::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }

    /// Buy `amount_tokens` for at most `max_sol_cost` lamports.
    pub async fn buy(&self, order: &BuyOrder) -> Result<SwapReceipt, ExecutorError> {
        let ixs = self.preflight_buy(order).await?;
        self.send(&ixs, "buy").await
    }

    /// Sell `amount_tokens` for at least `min_sol_output` lamports.
    pub async fn sell(&self, order: &SellOrder) -> Result<SwapReceipt, ExecutorError> {
        let ixs = self.preflight_sell(order).await?;
        self.send(&ixs, "sell").await
    }

    /// Buy via Jito bundle: swap ix plus the order's tip in one atomic
    /// transaction. Requires [`SwapExecutor::with_jito`].
    pub async fn buy_jito(&self, order: &BuyOrder) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_buy(order).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "buy-jito").await
    }

    /// Sell via Jito bundle (see [`SwapExecutor::buy_jito`]).
    pub async fn sell_jito(&self, order: &SellOrder) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_sell(order).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "sell-jito").await
    }

    /// Shared buy path: solvency preflight plus swap instruction.
    async fn preflight_buy(
        &self,
        order: &BuyOrder,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        // Degenerate orders can never fill: reject before spending fees.
        // (`LiveTrader::plan_buy` already filters these; this guards
        // direct `SwapExecutor` callers.)
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("buy amount_tokens is 0".to_string()));
        }
        if order.max_sol_cost == 0 {
            return Err(ExecutorError::BadOrder("buy max_sol_cost is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, order.max_sol_cost, order.tip_lamports)
            .await?;
        info!(mint = %order.mint, balance, max_cost = order.max_sol_cost, "buy pre-flight ok");
        let payer = self.wallet.pubkey();
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // Program-bound ATA: Token-2022 mints derive a DIFFERENT address
        // than the classic helper (verified live).
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        let mut ixs = self
            .maybe_create_ata_ix(&payer, &order.mint, &ata, &token_program)
            .await;
        let fee_recipient = self.fee_recipient().await;
        ixs.push(PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount: order.amount_tokens, max_sol_cost: order.max_sol_cost, track_volume: true },
            &crate::instructions::SwapAccounts {
                payer: &payer,
                mint: &order.mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                user_ata: &ata,
                fee_recipient: &fee_recipient,
                token_program: &token_program,
                creator: order.creator.as_ref(),
            },
        ));
        Ok(ixs)
    }

    /// Shared sell path: solvency preflight plus swap instruction.
    /// Sells spend no SOL beyond fees; still enforce reserve + tip.
    async fn preflight_sell(
        &self,
        order: &SellOrder,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("sell amount_tokens is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, order.tip_lamports)
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "sell pre-flight ok");
        let payer = self.wallet.pubkey();
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // Program-bound ATA (see buy path): the balance check below must
        // read the account the swap will actually spend from.
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        // The wallet must actually hold the tokens: an underfunded ATA
        // fails on-chain after burning fees, so check the balance first.
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_token_account_balance(&ata),
        )
        .await
        {
            Ok(Ok(balance)) => {
                let held: u64 = balance.amount.parse().unwrap_or(0);
                if held < order.amount_tokens {
                    return Err(ExecutorError::BadOrder(format!(
                        "sell amount {} exceeds ATA balance {held}",
                        order.amount_tokens
                    )));
                }
            }
            Ok(Err(_)) => {
                // ATA missing/unreadable: the sell cannot fill; fail here
                // instead of after fees.
                return Err(ExecutorError::BadOrder(
                    "sell ATA missing or unreadable".to_string(),
                ));
            }
            Err(_) => return Err(ExecutorError::Timeout("sell balance read")),
        }
        let fee_recipient = self.fee_recipient().await;
        Ok(vec![PumpSwapBuilder::build_swap_ix(
            SwapKind::Sell { amount: order.amount_tokens, min_sol_output: order.min_sol_output },
            &crate::instructions::SwapAccounts {
                payer: &payer,
                mint: &order.mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                user_ata: &ata,
                fee_recipient: &fee_recipient,
                token_program: &token_program,
                creator: order.creator.as_ref(),
            },
        )])
    }

    /// Create-ATA prefix when the user's token account for `mint` is not
    /// known to exist (fresh wallet or first touch of the mint). The
    /// idempotent variant makes the prefix safe on all paths: an existing
    /// ATA succeeds silently, and a race between the existence check and
    /// execution cannot fail the bundle.
    async fn maybe_create_ata_ix(
        &self,
        payer: &Pubkey,
        mint: &Pubkey,
        ata: &Pubkey,
        token_program: &Pubkey,
    ) -> Vec<solana_instruction::Instruction> {
        match tokio::time::timeout(Duration::from_secs(10), self.rpc.get_account(ata)).await {
            Ok(Ok(_)) => Vec::new(),
            _ => {
                use crate::instructions::build_create_ata_ix;
                vec![build_create_ata_ix(payer, payer, mint, ata, token_program)]
            }
        }
    }

    /// Append the tip transfer when the budget is non-zero. Uses the live
    /// tip set when a bundle client is attached (hourly refresh, silent
    /// fallback to the static rotation); plain-RPC sends use the static
    /// rotation directly (no engine to ask).
    async fn push_tip(&self, ixs: &mut Vec<solana_instruction::Instruction>, tip_lamports: u64) {
        if tip_lamports == 0 {
            return;
        }
        let payer = self.wallet.pubkey();
        let tip = match &self.jito {
            Some(jito) => jito.tip_account(nanos_seed()).await,
            None => select_tip_account(nanos_seed()),
        };
        ixs.push(tip_instruction(&payer, &tip, tip_lamports));
    }

    /// Sign with the guarded keypair and confirm. Single-flight per call.
    /// Deadline-bound on top of the client's own confirmation loop.
    async fn send(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<SwapReceipt, ExecutorError> {
        let tx = self.build_tx(ixs).await?;
        self.simulate_tx(&tx, label).await?;
        match tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.rpc.send_and_confirm_transaction(&tx),
        )
        .await
        {
            Ok(Ok(signature)) => {
                info!(%label, %signature, "swap confirmed");
                Ok(SwapReceipt {
                    signature: signature.to_string(),
                })
            }
            Ok(Err(e)) => {
                warn!(%label, error = %e, "swap failed");
                Err(ExecutorError::Rpc(e))
            }
            Err(_) => {
                warn!(%label, "swap confirmation timed out");
                Err(ExecutorError::Timeout("send_and_confirm"))
            }
        }
    }

    /// Sign and dispatch as a single-transaction Jito bundle.
    async fn send_bundle(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<JitoReceipt, ExecutorError> {
        let jito = self.jito.clone().ok_or(ExecutorError::NoJito)?;
        let tx = self.build_tx(ixs).await?;
        self.simulate_tx(&tx, label).await?;
        let signature = tx
            .signatures
            .first()
            .map(ToString::to_string)
            .filter(|s| !s.is_empty())
            .ok_or(ExecutorError::UnsignedTransaction)?;
        let bundle = encode_bundle(std::slice::from_ref(&tx))?;
        match jito.send_bundle(bundle).await {
            Ok(bundle_id) if !bundle_id.is_empty() => {
                info!(%label, %bundle_id, %signature, "bundle accepted");
                Ok(JitoReceipt {
                    bundle_id,
                    signature,
                })
            }
            Ok(_) => {
                warn!(%label, "bundle engine returned empty id");
                Err(ExecutorError::Jito(JitoError::Rpc(
                    "empty bundle id".to_string(),
                )))
            }
            Err(e) => {
                warn!(%label, error = %e, "bundle rejected");
                Err(ExecutorError::Jito(e))
            }
        }
    }

    /// Fetch a fresh blockhash and sign every instruction with the wallet.
    /// Deadline-bound: a hung blockhash stalls the whole dispatch.
    async fn build_tx(
        &self,
        ixs: &[solana_instruction::Instruction],
    ) -> Result<Transaction, ExecutorError> {
        let blockhash = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.rpc.get_latest_blockhash(),
        )
        .await
        .map_err(|_| ExecutorError::Timeout("get_latest_blockhash"))??;
        // Compute-budget prefix first (limit, price), then the swap/ATA/tip
        // instructions; empty by default (byte-identical to before).
        let mut all = self.budget_prefix();
        all.extend_from_slice(ixs);
        Ok(Transaction::new_signed_with_payer(
            &all,
            Some(&self.wallet.pubkey()),
            &[self.wallet.keypair()],
            blockhash,
        ))
    }

    /// `simulateTransaction` preflight on the signed transaction.
    /// A definitive simulation failure rejects the dispatch *before* any
    /// fee is spent (layout bugs surface here, not on-chain). Transport
    /// failures proceed with a warning — an unknown sim result preserves
    /// today's behavior rather than blocking valid trades.
    async fn simulate_tx(&self, tx: &Transaction, label: &str) -> Result<(), ExecutorError> {
        use solana_client::rpc_config::RpcSimulateTransactionConfig;
        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            ..RpcSimulateTransactionConfig::default()
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.rpc.simulate_transaction_with_config(tx, config),
        )
        .await;
        match outcome {
            Ok(Ok(response)) if response.value.err.is_some() => {
                let err = format!("{:?}", response.value.err);
                warn!(%label, %err, "simulation failed; refusing to dispatch");
                Err(ExecutorError::SimFailed(err))
            }
            Ok(Ok(_)) => {
                info!(%label, "simulation ok");
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(%label, error = %e, "simulation RPC failed; proceeding without preflight");
                Ok(())
            }
            Err(_) => {
                warn!(%label, "simulation timed out; proceeding without preflight");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::WalletGuard;
    use solana_keypair::Keypair;

    fn executor() -> SwapExecutor {
        let keypair = Keypair::new();
        let wallet = WalletGuard::from_base58(&bs58::encode(keypair.to_bytes()).into_string())
            .expect("wallet");
        SwapExecutor::new(
            "https://api.mainnet-beta.solana.com",
            CommitmentConfig::confirmed(),
            wallet,
        )
        .expect("executor")
    }

    #[tokio::test]
    async fn confirm_rejects_garbage_signature_without_network() {
        let err = executor()
            .confirm_signature("not-a-signature", Duration::from_secs(1))
            .await
            .expect_err("must fail");
        assert!(matches!(err, ExecutorError::BadSignature { .. }));
    }
}
