//! Swap dispatch: solvency check, then build, sim, send.
//! RPC (`buy`/`sell`) or Jito bundle (`*_jito`).

use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::{str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tracing::{info, warn};

use crate::instructions::{PumpSwapBuilder, user_ata_with_program};
use crate::jito::{
    JitoClient, JitoError, encode_bundle, nanos_seed, select_tip_account, tip_instruction,
};
use crate::wallet::{WalletGuard, WalletGuardError};

/// Executor failures.
#[derive(Error, Debug)]
pub enum ExecutorError {
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    #[error("wallet guard: {0}")]
    Wallet(#[from] WalletGuardError),
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
    #[error("jito error: {0}")]
    Jito(#[from] JitoError),
    /// Jito route with no bundle client.
    #[error("jito client not configured; call with_jito first")]
    NoJito,
    /// Not valid base58.
    #[error("bad signature '{value}': {msg}")]
    BadSignature {
        value: String,
        msg: String,
    },
    /// No signatures after signing.
    #[error("unsigned transaction: refusing to dispatch")]
    UnsignedTransaction,
    /// Sim proved failure; fees saved.
    #[error("simulateTransaction failed: {0}")]
    SimFailed(String),
    /// Degenerate amounts.
    #[error("bad order: {0}")]
    BadOrder(String),
    #[error("rpc call timed out: {0}")]
    Timeout(&'static str),
}

/// Live buy params (base units and lamports, no floats).
#[derive(Debug, Clone)]
pub struct BuyOrder {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    /// Token base units to buy.
    pub amount_tokens: u64,
    /// Max lamports to spend. Recompute only lowers it, never raises it.
    pub max_sol_cost: u64,
    /// Jito tip (bundles only; plain sends ignore it).
    pub tip_lamports: u64,
    /// Required: selects v3, checked vs curve state. `None` fails closed.
    pub creator: Option<Pubkey>,
    /// Slippage bps for the fee-inclusive recompute.
    pub slippage_bps: u64,
}

/// Live sell params.
#[derive(Debug, Clone)]
pub struct SellOrder {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    /// Token base units to sell.
    pub amount_tokens: u64,
    /// Min lamports to accept.
    pub min_sol_output: u64,
    /// Jito tip (see [`BuyOrder::tip_lamports`]).
    pub tip_lamports: u64,
    /// Required: checked vs curve state. `None` fails closed.
    pub creator: Option<Pubkey>,
}

/// Post-migration Raydium sell: token -> WSOL on a validated V4 pool.
/// Floor prices from live vault reserves in preflight.
pub struct SellRaydiumOrder {
    pub mint: Pubkey,
    /// Raydium AMM pool.
    pub amm: Pubkey,
    /// Coin-side vault (WSOL on migrations).
    pub base_vault: Pubkey,
    /// Pc-side vault (token on migrations).
    pub quote_vault: Pubkey,
    /// Token base units to sell.
    pub amount_tokens: u64,
    /// Slippage bps over the reserve quote (`>= 10000` refuses).
    pub slippage_bps: u64,
    /// Jito tip (see [`BuyOrder::tip_lamports`]).
    pub tip_lamports: u64,
}

/// Curve trade state (offsets in [`ids`]).
/// Zero quote = SOL-paired -> WSOL. Complete = migrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveState {
    /// Creator (mismatch fails closed).
    pub creator: Pubkey,
    /// Quote mint (curve-zero resolved to WSOL).
    pub quote_mint: Pubkey,
    /// Mayhem coin (reserved fee list; v3 ok).
    pub is_mayhem: bool,
    /// Cashback coin (v3 refused on-chain with 6094).
    pub is_cashback: bool,
    /// Complete (curve trades refused).
    pub complete: bool,
    /// Virtual token reserves.
    pub vt: u64,
    /// Virtual quote reserves (lamports).
    pub vq: u64,
    /// Real (unbought) tokens (quote cap).
    pub real_tokens: u64,
    /// Stored supply (fee-tier mcap input on mayhem).
    pub token_total_supply: u64,
    /// Coin creator rate in bps (0 = unconfigured).
    pub coin_creator_fee_bps: u64,
}

/// Decode [`CurveState`]. Pure. `None` on wrong owner or short data.
pub fn parse_curve_state(owner: &Pubkey, data: &[u8]) -> Option<CurveState> {
    use crate::instructions::{
        CURVE_CASHBACK_FLAG_OFFSET, CURVE_COMPLETE_OFFSET, CURVE_CREATOR_END,
        CURVE_CREATOR_OFFSET, CURVE_MAYHEM_FLAG_OFFSET, CURVE_MIN_LEN,
        CURVE_QUOTE_MINT_END, CURVE_QUOTE_MINT_OFFSET,
    };
    if *owner != crate::instructions::PUMP_PROGRAM_ID || data.len() < CURVE_MIN_LEN {
        return None;
    }
    let creator = Pubkey::new_from_array(
        data.get(CURVE_CREATOR_OFFSET..CURVE_CREATOR_END)?.try_into().ok()?,
    );
    let quote_raw = Pubkey::new_from_array(
        data.get(CURVE_QUOTE_MINT_OFFSET..CURVE_QUOTE_MINT_END)?.try_into().ok()?,
    );
    let u64at = |off: usize| -> Option<u64> {
        data.get(off..off + 8)?.try_into().ok().map(u64::from_le_bytes)
    };
    Some(CurveState {
        creator,
        quote_mint: resolve_quote_mint(&quote_raw),
        is_mayhem: data.get(CURVE_MAYHEM_FLAG_OFFSET).copied().unwrap_or(0) != 0,
        is_cashback: data.get(CURVE_CASHBACK_FLAG_OFFSET).copied().unwrap_or(0) != 0,
        complete: data.get(CURVE_COMPLETE_OFFSET).copied().unwrap_or(0) != 0,
        vt: u64at(8)?,
        vq: u64at(16)?,
        real_tokens: u64at(24)?,
        token_total_supply: u64at(40)?,
        coin_creator_fee_bps: u64at(115)?,
    })
}

/// Deadline for single read RPCs on the hot path.
pub const RPC_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One deadline-bound RPC read. `None` on timeout or failure.
async fn rpc_timeout<T>(
    timeout: Duration,
    fut: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Option<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Some(v),
        _ => None,
    }
}

/// Mint supply for fee tiers (SPL Mint `supply @36..44`).
/// `None` on failure; caller keeps the order guard.
async fn mint_supply(rpc: &RpcClient, mint: &Pubkey) -> Option<u64> {
    use crate::instructions::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};
    let account = rpc_timeout(RPC_READ_TIMEOUT, rpc.get_account(mint)).await?;
    if account.owner != TOKEN_PROGRAM_ID && account.owner != TOKEN_2022_PROGRAM_ID {
        return None;
    }
    account
        .data
        .get(36..44)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}

/// Fee inputs: creator gate + tier schedule. `None` on failure.
async fn fetch_fee_inputs(rpc: &RpcClient) -> Option<(bool, crate::quote::FeeSchedule)> {
    use crate::instructions::{FEE_CONFIG_ACCOUNT, FEE_CONFIG_PROGRAM, GLOBAL_ACCOUNT};
    use crate::quote::{parse_fee_gate, parse_fee_schedule};
    let (global, schedule) = tokio::join!(
        async {
            let account =
                rpc_timeout(RPC_READ_TIMEOUT, rpc.get_account(&GLOBAL_ACCOUNT)).await?;
            if account.owner != crate::instructions::PUMP_PROGRAM_ID {
                return None;
            }
            parse_fee_gate(&account.data).ok()
        },
        async {
            let account =
                rpc_timeout(RPC_READ_TIMEOUT, rpc.get_account(&FEE_CONFIG_ACCOUNT)).await?;
            if account.owner != FEE_CONFIG_PROGRAM {
                return None;
            }
            parse_fee_schedule(&account.data).ok()
        }
    );
    Some((global?, schedule?))
}

/// Zero quote key means SOL-paired -> WSOL. Pure.
pub fn resolve_quote_mint(curve_quote: &Pubkey) -> Pubkey {
    if *curve_quote == Pubkey::default() {
        crate::instructions::WSOL_MINT
    } else {
        *curve_quote
    }
}

/// v3 buyback account: wallet itself on SOL quotes, else its quote ATA. Pure.
pub fn buyback_account(
    buyback_wallet: &Pubkey,
    quote_mint: &Pubkey,
    quote_token_program: &Pubkey,
) -> Pubkey {
    use crate::instructions::{WSOL_MINT, user_ata_with_program};
    if *quote_mint == WSOL_MINT {
        *buyback_wallet
    } else {
        user_ata_with_program(buyback_wallet, quote_mint, quote_token_program)
    }
}

/// Fill outcome for a dispatched signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    Confirmed,
    FailedOnChain,
    Timeout,
}

/// Confirmed swap receipt.
#[derive(Debug, Clone)]
pub struct SwapReceipt {
    pub signature: String,
}

/// Accepted Jito bundle receipt (acceptance != landing).
#[derive(Debug, Clone)]
pub struct JitoReceipt {
    /// Block-engine bundle UUID.
    pub bundle_id: String,
    /// First tx signature (base58) for tracking.
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
    /// Strict preflight: sim transport failure rejects. Off by default.
    strict_sim: bool,
}

/// Read-only funding snapshot. `None` = unreadable, never funded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveFundingReport {
    /// Trading wallet.
    pub payer: Pubkey,
    /// Native SOL in lamports (`None` = unreadable).
    pub native_balance: Option<u64>,
    /// Native lamports required (tip + 0.05 SOL reserve).
    pub native_required: u64,
    /// Wallet WSOL ATA.
    pub wsol_ata: Pubkey,
    /// WSOL balance (`None` = missing/unreadable).
    pub wsol_balance: Option<u64>,
}

impl LiveFundingReport {
    /// Natively funded (`None` when unreadable).
    pub fn native_ok(&self) -> Option<bool> {
        self.native_balance.map(|b| b >= self.native_required)
    }

    /// Holds spendable WSOL (`None` when unreadable).
    pub fn wsol_funded(&self) -> Option<bool> {
        self.wsol_balance.map(|b| b > 0)
    }
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
            strict_sim: false,
        })
    }

    /// Attach a Jito bundle client for protected dispatch.
    pub fn with_jito(mut self, jito: JitoClient) -> Self {
        self.jito = Some(jito);
        self
    }

    /// Strict preflight: sim transport/timeout failures reject (`SimFailed`).
    /// Off by default; turn on for live funds.
    pub fn with_strict_sim(mut self, strict: bool) -> Self {
        self.strict_sim = strict;
        self
    }

    /// Tip that leaves the wallet: order tip on bundles, zero on plain sends.
    fn effective_tip(&self, tip_lamports: u64, bundled: bool) -> u64 {
        if bundled && self.jito.is_some() {
            tip_lamports
        } else {
            0
        }
    }

    /// Read-only funding snapshot. Moves no funds.
    pub async fn live_funding_report(&self, tip_lamports: u64) -> LiveFundingReport {
        use crate::instructions::{WSOL_MINT, user_ata_with_program};
        let required = self
            .effective_tip(tip_lamports, true)
            .saturating_add(crate::wallet::MIN_SOL_RESERVE_LAMPORTS);
        let native_balance = rpc_timeout(
            Duration::from_secs(15),
            self.rpc.get_balance(&self.wallet.pubkey()),
        )
        .await;
        // Same program resolution as every dispatch path.
        let wsol_ata = match self.token_program(&WSOL_MINT).await {
            Some(program) => {
                user_ata_with_program(&self.wallet.pubkey(), &WSOL_MINT, &program)
            }
            None => user_ata_with_program(
                &self.wallet.pubkey(),
                &WSOL_MINT,
                &crate::instructions::TOKEN_PROGRAM_ID,
            ),
        };
        let wsol_balance = self.quote_balance(&wsol_ata).await;
        LiveFundingReport {
            payer: self.wallet.pubkey(),
            native_balance,
            native_required: required,
            wsol_ata,
            wsol_balance,
        }
    }

    /// Token program owning `mint`. `None` when unreadable (fail closed).
    async fn token_program(&self, mint: &Pubkey) -> Option<Pubkey> {
        let account = rpc_timeout(RPC_READ_TIMEOUT, self.rpc.get_account(mint)).await?;
        use crate::instructions::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};
        if account.owner != TOKEN_PROGRAM_ID && account.owner != TOKEN_2022_PROGRAM_ID {
            return None;
        }
        Some(account.owner)
    }

    /// Fee-inclusive spend guard. Falls back to the order guard when
    /// unreadable. Never exceeds the order ceiling.
    async fn quote_buy_guard(
        &self,
        mint: &Pubkey,
        state: Option<&CurveState>,
        amount: u64,
        fallback_max: u64,
        slippage_bps: u64,
    ) -> (u64, Option<u64>) {
        let Some(state) = state else { return (fallback_max, None) };
        if state.complete || state.is_cashback {
            return (fallback_max, None);
        }
        let (fees, supply) = tokio::join!(
            fetch_fee_inputs(&self.rpc),
            async {
                if state.is_mayhem {
                    Some(state.token_total_supply)
                } else {
                    mint_supply(&self.rpc, mint).await
                }
            }
        );
        let (Some((gate_on, schedule)), Some(supply)) = (fees, supply) else {
            warn!(mint = %mint, "fee inputs unreadable; keeping sized guard (sim decides)");
            return (fallback_max, None);
        };
        let view = crate::quote::CurveView {
            vt: state.vt,
            vq: state.vq,
            real_tokens: state.real_tokens,
            token_total_supply: state.token_total_supply,
            coin_creator_fee_bps: state.coin_creator_fee_bps,
            quote: if state.quote_mint == crate::instructions::WSOL_MINT {
                crate::quote::QuoteKind::SolLike
            } else if state.quote_mint == crate::instructions::USDC_MINT {
                crate::quote::QuoteKind::Usdc
            } else {
                crate::quote::QuoteKind::Other
            },
            is_mayhem: state.is_mayhem,
        };
        match crate::quote::buy_cost_incl_fees(&schedule, gate_on, &view, supply, amount) {
            Ok(cost) => match crate::quote::slippage_ceiling(cost, slippage_bps) {
                Some(guard) => {
                    // Never authorize more than stated.
                    let guard = guard.min(fallback_max);
                    if guard != fallback_max {
                        info!(mint = %mint, quoted = cost, guard, fallback = fallback_max,
                            "fee-inclusive guard recomputed");
                    }
                    (guard, Some(cost))
                }
                None => (fallback_max, None),
            },
            Err(e) => {
                warn!(mint = %mint, error = %e, "quote failed; keeping sized guard (sim decides)");
                (fallback_max, None)
            }
        }
    }

    /// Quote-token balance of `ata` (`None` on failure).
    async fn quote_balance(&self, ata: &Pubkey) -> Option<u64> {
        let balance =
            rpc_timeout(RPC_READ_TIMEOUT, self.rpc.get_token_account_balance(ata)).await?;
        balance.amount.parse().ok()
    }

    /// Fetch and decode curve state. `None` on any failure.
    async fn read_curve_state(&self, curve: &Pubkey) -> Option<CurveState> {
        let account = rpc_timeout(RPC_READ_TIMEOUT, self.rpc.get_account(curve)).await?;
        parse_curve_state(&account.owner, &account.data)
    }

    /// Attach a compute budget. Zeros disable; default builds identical txs.
    pub fn with_compute_budget(mut self, unit_limit: u32, unit_price_micro_lamports: u64) -> Self {
        self.compute_unit_limit = unit_limit;
        self.compute_unit_price = unit_price_micro_lamports;
        self
    }

    /// Compute-budget prefix (empty by default).
    fn budget_prefix(&self) -> Vec<solana_instruction::Instruction> {
        crate::instructions::compute_budget_ixs(self.compute_unit_limit, self.compute_unit_price)
    }
}

impl SwapExecutor {
    /// Trading address.
    pub fn pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    /// Poll a signature until it lands or `timeout` ends.
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
            // One hung RPC stalls only this round.
            let status = match tokio::time::timeout(
                Duration::from_secs(15),
                self.rpc.get_signature_status(&signature),
            )
            .await
            {
                Ok(Ok(status)) => status,
                Ok(Err(e)) => return Err(e.into()),
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
        let ixs = self.preflight_buy(order, false).await?;
        self.send(&ixs, "buy").await
    }

    /// Sell `amount_tokens` for at least `min_sol_output` lamports.
    pub async fn sell(&self, order: &SellOrder) -> Result<SwapReceipt, ExecutorError> {
        let ixs = self.preflight_sell(order, false).await?;
        self.send(&ixs, "sell").await
    }

    /// Buy via Jito bundle (swap + tip, atomic). Requires `with_jito`.
    pub async fn buy_jito(&self, order: &BuyOrder) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_buy(order, true).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "buy-jito").await
    }

    /// Sell via Jito bundle (see [`SwapExecutor::buy_jito`]).
    pub async fn sell_jito(&self, order: &SellOrder) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_sell(order, true).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "sell-jito").await
    }

    /// Post-migration Raydium sell via Jito. Floor from live reserves.
    pub async fn sell_raydium_jito(
        &self,
        order: &SellRaydiumOrder,
    ) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_sell_raydium(order, true).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "sell-raydium-jito").await
    }

    /// Raydium sell checks, then the 8-account `SwapBaseInV2` shape.
    /// Sim gates the bundle, so a stale reserve read dies free.
    async fn preflight_sell_raydium(
        &self,
        order: &SellRaydiumOrder,
        bundled: bool,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder(
                "raydium sell amount_tokens is 0".to_string(),
            ));
        }
        if order.slippage_bps >= 10_000 {
            return Err(ExecutorError::BadOrder(
                "raydium sell slippage >= 100%: refusing unguarded floor".to_string(),
            ));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, self.effective_tip(order.tip_lamports, bundled))
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "raydium sell pre-flight ok");
        let payer = self.wallet.pubkey();
        // Source ATA must be the account the swap spends from.
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        let source_ata = crate::instructions::user_ata_with_program(&payer, &order.mint, &token_program);
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_token_account_balance(&source_ata),
        )
        .await
        {
            Ok(Ok(balance)) => {
                let held: u64 = balance.amount.parse().map_err(|_| {
                    ExecutorError::BadOrder(format!(
                        "raydium sell balance unreadable for {} (RPC shape change?); refusing",
                        order.mint
                    ))
                })?;
                if held < order.amount_tokens {
                    return Err(ExecutorError::BadOrder(format!(
                        "raydium sell amount {} exceeds ATA balance {held}",
                        order.amount_tokens
                    )));
                }
            }
            Ok(Err(_)) => {
                return Err(ExecutorError::BadOrder(
                    "raydium sell ATA missing or unreadable".to_string(),
                ));
            }
            Err(_) => return Err(ExecutorError::Timeout("raydium sell balance read")),
        }
        // Vaults must be classic Token accounts ([0] is spl_token); 2022 fails closed.
        let (base_vault, quote_vault) = tokio::join!(
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&order.base_vault)).await
            },
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&order.quote_vault)).await
            }
        );
        let (base_vault, quote_vault) = match (base_vault, quote_vault) {
            (Ok(Ok(b)), Ok(Ok(q))) => (b, q),
            _ => {
                return Err(ExecutorError::BadOrder(
                    "raydium vaults unreadable; refusing unroutable dispatch (fail-closed)"
                        .to_string(),
                ));
            }
        };
        if base_vault.owner != crate::instructions::TOKEN_PROGRAM_ID
            || quote_vault.owner != crate::instructions::TOKEN_PROGRAM_ID
        {
            warn!(mint = %order.mint, "Token-2022 raydium vaults: unproven path; refusing (fail-closed, manual exit)");
            return Err(ExecutorError::BadOrder(
                "Token-2022 raydium vaults unproven; refusing (fail-closed)".to_string(),
            ));
        }
        let vault_amount = |data: &[u8]| -> Option<u64> {
            data.get(64..72)?.try_into().ok().map(u64::from_le_bytes)
        };
        let (Some(base_reserve), Some(quote_reserve)) = (
            vault_amount(&base_vault.data),
            vault_amount(&quote_vault.data),
        ) else {
            return Err(ExecutorError::BadOrder(
                "raydium vault balances unreadable; refusing".to_string(),
            ));
        };
        // Bags hold the QUOTE-side token (base is WSOL).
        let quoted =
            crate::raydium::quote_base_in_std(order.amount_tokens, quote_reserve, base_reserve)
                .ok_or_else(|| {
                    ExecutorError::BadOrder("raydium quote unroutable at this size".to_string())
                })?;
        let floor = crate::raydium::raydium_sell_floor(quoted, order.slippage_bps).ok_or_else(|| {
            ExecutorError::BadOrder("raydium floor unguardable (dust); refusing".to_string())
        })?;
        // WSOL dest [6] must exist; prefix is idempotent.
        let user_destination = crate::instructions::user_ata_with_program(
            &payer,
            &crate::instructions::WSOL_MINT,
            &crate::instructions::TOKEN_PROGRAM_ID,
        );
        let mut ixs = Vec::new();
        ixs.extend(
            self.maybe_create_ata_ix(
                &payer,
                &payer,
                &crate::instructions::WSOL_MINT,
                &user_destination,
                &crate::instructions::TOKEN_PROGRAM_ID,
            )
            .await,
        );
        ixs.push(
            crate::raydium::build_swap_base_in_v2_ix(
                &order.amm,
                &ids::RAYDIUM_VAULT_AUTHORITY,
                &order.base_vault,
                &order.quote_vault,
                &source_ata,
                &user_destination,
                &payer,
                order.amount_tokens,
                floor,
            )
            .ok_or_else(|| {
                ExecutorError::BadOrder("raydium swap ix refused to build".to_string())
            })?,
        );
        Ok(ixs)
    }

    /// Shared buy path. `bundled` selects the solvency tip.
    async fn preflight_buy(
        &self,
        order: &BuyOrder,
        bundled: bool,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        // Reject before spending fees.
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("buy amount_tokens is 0".to_string()));
        }
        if order.max_sol_cost == 0 {
            return Err(ExecutorError::BadOrder("buy max_sol_cost is 0".to_string()));
        }
        // SOL-paired pays native; token-paired debits the quote ATA.
        let payer = self.wallet.pubkey();
        // Independent RPCs: load together.
        let (base_program, curve_state) = tokio::join!(
            self.token_program(&order.mint),
            self.read_curve_state(&order.bonding_curve)
        );
        let Some(token_program) = base_program else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // v3-only: creatorless, mismatched, complete, cashback fail closed.
        let Some(creator) = order.creator.as_ref() else {
            warn!(mint = %order.mint, "creatorless order: legacy v1 creates are unsupported (v3-only dispatch)");
            return Err(ExecutorError::BadOrder(
                "no creator: legacy v1 creates unsupported; refusing (fail-closed)".to_string(),
            ));
        };
        let Some(state) = curve_state.as_ref() else {
            warn!(mint = %order.mint, "curve state unreadable; refusing unroutable dispatch (fail-closed)");
            return Err(ExecutorError::BadOrder(
                "curve state unreadable; refusing unroutable dispatch (fail-closed)".to_string(),
            ));
        };
        if state.creator != *creator {
            warn!(mint = %order.mint, order_creator = %creator, curve_creator = %state.creator,
                "creator mismatch between launch parse and curve state; refusing (fail-closed)");
            return Err(ExecutorError::BadOrder(
                "creator mismatch; refusing unroutable dispatch (fail-closed)".to_string(),
            ));
        }
        if state.complete {
            return Err(ExecutorError::BadOrder(
                "curve complete; buys route to the pool/migration, not the curve".to_string(),
            ));
        }
        if state.is_cashback {
            warn!(mint = %order.mint, "cashback-era curve: v2 dispatch retired; refusing (fail-closed, manual exit only)");
            // Never re-add v2 without a frozen fill pair + sim GREEN probe.
            return Err(ExecutorError::BadOrder(
                "cashback-era curve: v3 refused on-chain (6094) and v2 dispatch retired; refusing (fail-closed)".to_string(),
            ));
        }
        // Program-bound ATA (differs for 2022 mints). Both reads batched.
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        let (ixs, (final_max, _)) = tokio::join!(
            self.maybe_create_ata_ix(&payer, &payer, &order.mint, &ata, &token_program),
            self.quote_buy_guard(
                &order.mint,
                curve_state.as_ref(),
                order.amount_tokens,
                order.max_sol_cost,
                order.slippage_bps,
            )
        );
        // SOL-paired pays native; token-paired pays from the quote ATA.
        let is_sol_quote = curve_state
            .as_ref()
            .is_some_and(|s| s.quote_mint == crate::instructions::WSOL_MINT);
        let trade_lamports = if is_sol_quote { final_max } else { 0 };
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, trade_lamports, self.effective_tip(order.tip_lamports, bundled))
            .await?;
        info!(mint = %order.mint, balance, max_cost = final_max, "buy pre-flight ok");
        let Some(state) = curve_state.as_ref() else {
            return Err(ExecutorError::BadOrder(
                "curve state unreadable; refusing unroutable dispatch (fail-closed)".to_string(),
            ));
        };
        info!(mint = %order.mint, mayhem = state.is_mayhem,
            "v3 buy via derived addresses + listed buyback (no reference scan)");
        return self
            .preflight_buy_v3(order, &payer, &token_program, ixs, state, final_max)
            .await;
    }

    /// v3 buy: 17 accounts. Guard is `max_sol_cost`. Quote ATAs pre-created
    /// on token-paired quotes only (base-ATA prefix already in `ixs`).
    #[allow(clippy::too_many_arguments)]
    async fn preflight_buy_v3(
        &self,
        order: &BuyOrder,
        payer: &Pubkey,
        token_program: &Pubkey,
        mut ixs: Vec<solana_instruction::Instruction>,
        state: &CurveState,
        max_sol_cost: u64,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        let quote_mint = state.quote_mint;
        let Some(quote_program) = self.token_program(&quote_mint).await else {
            return Err(ExecutorError::BadOrder(
                "quote mint account unreadable; token program unknown".to_string(),
            ));
        };
        let buyback_wallet = crate::instructions::DEFAULT_BUYBACK_FEE_RECIPIENT;
        let buyback = buyback_account(&buyback_wallet, &quote_mint, &quote_program);
        // SOL-paired pays native ([10] unused). Token-paired debits the quote ATA.
        let user_quote =
            user_ata_with_program(payer, &quote_mint, &quote_program);
        if quote_mint == crate::instructions::WSOL_MINT {
            info!(mint = %order.mint, cost = max_sol_cost, "sol-pair buy pays native SOL; no quote ATA needed");
        } else {
            // Independent existence reads: batched.
            let (user_prefix, buyback_prefix) = tokio::join!(
                self.maybe_create_ata_ix(payer, payer, &quote_mint, &user_quote, &quote_program),
                self.maybe_create_ata_ix(
                    payer,
                    &buyback_wallet,
                    &quote_mint,
                    &buyback,
                    &quote_program,
                )
            );
            ixs.extend(user_prefix);
            ixs.extend(buyback_prefix);
        }
        ixs.push(PumpSwapBuilder::build_buy_v3_ix(
            order.amount_tokens,
            max_sol_cost,
            &crate::instructions::BuyV3Accounts {
                payer,
                mint: &order.mint,
                quote_mint: &quote_mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                base_token_program: token_program,
                quote_token_program: &quote_program,
                buyback_recipient: &buyback,
            },
        ));
        Ok(ixs)
    }

    /// Shared sell path.
    async fn preflight_sell(
        &self,
        order: &SellOrder,
        bundled: bool,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("sell amount_tokens is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, self.effective_tip(order.tip_lamports, bundled))
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "sell pre-flight ok");
        let payer = self.wallet.pubkey();
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // Read the account the swap spends from.
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_token_account_balance(&ata),
        )
        .await
        {
            Ok(Ok(balance)) => {
                let held: u64 = balance.amount.parse().map_err(|_| {
                    ExecutorError::BadOrder(format!(
                        "sell balance unreadable for {} (RPC shape change?); refusing",
                        order.mint
                    ))
                })?;
                if held < order.amount_tokens {
                    return Err(ExecutorError::BadOrder(format!(
                        "sell amount {} exceeds ATA balance {held}",
                        order.amount_tokens
                    )));
                }
            }
            Ok(Err(_)) => {
                // Holdings are always canonical, so this is transient, not aux.
                return Err(ExecutorError::BadOrder(
                    "sell ATA missing or unreadable".to_string(),
                ));
            }
            Err(_) => return Err(ExecutorError::Timeout("sell balance read")),
        }
        // v3-only (mirrors buys): creatorless, complete, cashback fail closed.
        let Some(creator) = order.creator.as_ref() else {
            warn!(mint = %order.mint, "creatorless sell order: legacy dispatch retired; refusing (fail-closed, manual exit)");
            return Err(ExecutorError::BadOrder(
                "no creator: legacy sells retired; refusing (fail-closed)".to_string(),
            ));
        };
        let Some(state) = self.read_curve_state(&order.bonding_curve).await else {
            warn!(mint = %order.mint, "curve state unreadable; refusing unroutable sell (fail-closed, manual exit)");
            return Err(ExecutorError::BadOrder(
                "curve state unreadable; refusing unroutable sell (fail-closed)".to_string(),
            ));
        };
        if state.creator != *creator {
            warn!(mint = %order.mint, "creator mismatch between launch parse and curve state; refusing (fail-closed)");
            return Err(ExecutorError::BadOrder(
                "creator mismatch; refusing unroutable dispatch (fail-closed)".to_string(),
            ));
        }
        if state.complete {
            return Err(ExecutorError::BadOrder(
                "curve complete; exits route via migration/manual, not the curve".to_string(),
            ));
        }
        if state.is_cashback {
            warn!(mint = %order.mint, "cashback-era curve: v2 dispatch retired; refusing (fail-closed, manual exit)");
            return Err(ExecutorError::BadOrder(
                "cashback-era curve: v2 dispatch retired; refusing (fail-closed)".to_string(),
            ));
        }
        info!(mint = %order.mint, mayhem = state.is_mayhem,
            "v3 sell via derived addresses + listed buyback (no reference scan)");
        return self
            .preflight_sell_v3(order, &payer, &token_program, &state)
            .await;
    }

    /// v3 sell: 17 accounts, no base-ATA prefix (balance proved funds).
    /// Floor is `min_sol_output`.
    async fn preflight_sell_v3(
        &self,
        order: &SellOrder,
        payer: &Pubkey,
        token_program: &Pubkey,
        state: &CurveState,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        let quote_mint = state.quote_mint;
        let Some(quote_program) = self.token_program(&quote_mint).await else {
            return Err(ExecutorError::BadOrder(
                "quote mint account unreadable; token program unknown".to_string(),
            ));
        };
        let buyback_wallet = crate::instructions::DEFAULT_BUYBACK_FEE_RECIPIENT;
        let buyback = buyback_account(&buyback_wallet, &quote_mint, &quote_program);
        let mut ixs = Vec::new();
        // Quote ATA [10] must exist; prefix is idempotent.
        let user_quote =
            user_ata_with_program(payer, &quote_mint, &quote_program);
        ixs.extend(
            self.maybe_create_ata_ix(payer, payer, &quote_mint, &user_quote, &quote_program)
                .await,
        );
        if quote_mint != crate::instructions::WSOL_MINT {
            ixs.extend(
                self.maybe_create_ata_ix(
                    payer,
                    &buyback_wallet,
                    &quote_mint,
                    &buyback,
                    &quote_program,
                )
                .await,
            );
        }
        ixs.push(PumpSwapBuilder::build_sell_v3_ix(
            order.amount_tokens,
            order.min_sol_output,
            &crate::instructions::SellV3Accounts {
                payer,
                mint: &order.mint,
                quote_mint: &quote_mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                base_token_program: token_program,
                quote_token_program: &quote_program,
                buyback_recipient: &buyback,
            },
        ));
        Ok(ixs)
    }

    /// Create-ATA prefix when `ata` may not exist. Idempotent.
    /// A wrong-owner squat fails closed at sim.
    async fn maybe_create_ata_ix(
        &self,
        payer: &Pubkey,
        owner: &Pubkey,
        mint: &Pubkey,
        ata: &Pubkey,
        token_program: &Pubkey,
    ) -> Vec<solana_instruction::Instruction> {
        match rpc_timeout(RPC_READ_TIMEOUT, self.rpc.get_account(ata)).await {
            Some(_) => Vec::new(),
            None => {
                use crate::instructions::build_create_ata_ix;
                vec![build_create_ata_ix(payer, owner, mint, ata, token_program)]
            }
        }
    }

    /// Append the tip transfer when non-zero.
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

    /// Sign and confirm. One blockhash resample on expiry.
    /// No resend after send error: it may have landed.
    async fn send(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<SwapReceipt, ExecutorError> {
        let mut tx = self.build_tx(ixs).await?;
        match self.simulate_tx(&tx, label).await {
            Ok(()) => {}
            Err(e) if is_blockhash_expired(&e) => {
                warn!(%label, "stale blockhash under load; resampling once");
                tx = self.build_tx(ixs).await?;
                self.simulate_tx(&tx, label).await?;
            }
            Err(e) => return Err(e),
        }
        match tokio::time::timeout(
            Duration::from_secs(90),
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

    /// Sign and dispatch as a single-tx Jito bundle. One blockhash resample.
    async fn send_bundle(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<JitoReceipt, ExecutorError> {
        let jito = self.jito.clone().ok_or(ExecutorError::NoJito)?;
        let mut tx = self.build_tx(ixs).await?;
        match self.simulate_tx(&tx, label).await {
            Ok(()) => {}
            Err(e) if is_blockhash_expired(&e) => {
                warn!(%label, "stale blockhash under load; resampling once");
                tx = self.build_tx(ixs).await?;
                self.simulate_tx(&tx, label).await?;
            }
            Err(e) => return Err(e),
        }
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

    /// Fresh blockhash + wallet signature. Deadline-bound.
    async fn build_tx(
        &self,
        ixs: &[solana_instruction::Instruction],
    ) -> Result<Transaction, ExecutorError> {
        let blockhash = tokio::time::timeout(
            Duration::from_secs(15),
            self.rpc.get_latest_blockhash(),
        )
        .await
        .map_err(|_| ExecutorError::Timeout("get_latest_blockhash"))??;
        // Budget prefix first; empty by default.
        let mut all = self.budget_prefix();
        all.extend_from_slice(ixs);
        Ok(Transaction::new_signed_with_payer(
            &all,
            Some(&self.wallet.pubkey()),
            &[self.wallet.keypair()],
            blockhash,
        ))
    }

    /// Sim preflight. Hard failure rejects; transport trouble warns
    /// unless strict mode rejects too.
    async fn simulate_tx(&self, tx: &Transaction, label: &str) -> Result<(), ExecutorError> {
        use solana_client::rpc_config::RpcSimulateTransactionConfig;
        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            ..RpcSimulateTransactionConfig::default()
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(15),
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
                sim_transport_outcome(self.strict_sim, "RPC failure")
            }
            Err(_) => {
                warn!(%label, "simulation timed out; proceeding without preflight");
                sim_transport_outcome(self.strict_sim, "timeout")
            }
        }
    }
}

/// Unproven preflight: strict fails closed, else warn-and-proceed. Pure.
fn sim_transport_outcome(strict: bool, cause: &str) -> Result<(), ExecutorError> {
    if strict {
        Err(ExecutorError::SimFailed(format!(
            "preflight unavailable ({cause}); strict mode refuses to dispatch unproven"
        )))
    } else {
        Ok(())
    }
}

/// True on expired blockhash (retryable). Pure.
fn is_blockhash_expired(err: &ExecutorError) -> bool {
    match err {
        ExecutorError::SimFailed(msg) => {
            msg.contains("BlockhashNotFound") || msg.contains("Blockhash not found")
        }
        _ => false,
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

    #[test]
    fn funding_report_predicates() {
        // Known balances decide; unknown reads abstain.
        use solana_pubkey::Pubkey;
        let funded = LiveFundingReport {
            payer: Pubkey::default(),
            native_balance: Some(100_000),
            native_required: 60_000,
            wsol_ata: Pubkey::default(),
            wsol_balance: Some(1_000),
        };
        assert_eq!(funded.native_ok(), Some(true));
        assert_eq!(funded.wsol_funded(), Some(true));
        let broke = LiveFundingReport { native_balance: Some(10_000), wsol_balance: Some(0), ..funded };
        assert_eq!(broke.native_ok(), Some(false));
        assert_eq!(broke.wsol_funded(), Some(false));
        let unknown = LiveFundingReport { native_balance: None, wsol_balance: None, ..funded };
        assert_eq!(unknown.native_ok(), None);
        assert_eq!(unknown.wsol_funded(), None);
    }

    #[test]
    fn strict_preflight_rejects_unproven_dispatch() {
        // Default off preserves behavior; strict refuses unproven.
        assert!(sim_transport_outcome(false, "timeout").is_ok());
        // Strict: transport/timeout failures refuse to dispatch unproven.
        for cause in ["timeout", "RPC failure"] {
            match sim_transport_outcome(true, cause) {
                Err(ExecutorError::SimFailed(msg)) => assert!(msg.contains(cause), "{msg}"),
                other => panic!("expected SimFailed, got {other:?}"),
            }
        }
        // Builder plumbing enables the policy.
        assert!(!executor().strict_sim);
        assert!(executor().with_strict_sim(true).strict_sim);
    }

    #[test]
    fn blockhash_expiry_is_retryable_layout_errors_are_not() {
        assert!(is_blockhash_expired(&ExecutorError::SimFailed(
            "Transaction simulation failed: BlockhashNotFound".to_string()
        )));
        assert!(!is_blockhash_expired(&ExecutorError::SimFailed(
            "InstructionError(4, Custom(6002))".to_string()
        )));
        assert!(!is_blockhash_expired(&ExecutorError::Timeout("send_and_confirm")));
    }

    #[test]
    fn parse_curve_state_reads_verified_offsets() {
        // Synthetic 166-byte blob with the documented layout; pure offset contract.
        use crate::instructions::{
            CURVE_CASHBACK_FLAG_OFFSET, CURVE_COMPLETE_OFFSET, CURVE_CREATOR_OFFSET,
            CURVE_MAYHEM_FLAG_OFFSET, CURVE_QUOTE_MINT_OFFSET, WSOL_MINT,
        };
        let pump = crate::instructions::PUMP_PROGRAM_ID;
        let creator = Pubkey::from_str("DC4X9szDJk2wVm2NAGsshGFydKAahBg6sAAjmxvSJZGh")
            .expect("creator");
        let mut data = vec![0u8; 166];
        data[8..16].copy_from_slice(&1_073_000_000_000_000u64.to_le_bytes());
        data[16..24].copy_from_slice(&30_000_000_000u64.to_le_bytes());
        data[24..32].copy_from_slice(&793_100_000_000_000u64.to_le_bytes());
        data[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
        data[115..123].copy_from_slice(&0u64.to_le_bytes());
        data[CURVE_COMPLETE_OFFSET] = 0;
        data[CURVE_CREATOR_OFFSET..CURVE_CREATOR_OFFSET + 32]
            .copy_from_slice(&creator.to_bytes());
        data[CURVE_MAYHEM_FLAG_OFFSET] = 1;
        data[CURVE_CASHBACK_FLAG_OFFSET] = 0;
        // Zero quote → WSOL.
        let state = parse_curve_state(&pump, &data).expect("state");
        assert_eq!(state.creator, creator);
        assert_eq!(state.quote_mint, WSOL_MINT);
        assert!(state.is_mayhem && !state.is_cashback && !state.complete);
        assert_eq!(state.vt, 1_073_000_000_000_000);
        assert_eq!(state.vq, 30_000_000_000);
        assert_eq!(state.real_tokens, 793_100_000_000_000);
        assert_eq!(state.token_total_supply, 1_000_000_000_000_000);
        assert_eq!(state.coin_creator_fee_bps, 0);
        // Cashback + complete variants decode too.
        data[CURVE_CASHBACK_FLAG_OFFSET] = 1;
        data[CURVE_COMPLETE_OFFSET] = 1;
        let state = parse_curve_state(&pump, &data).expect("state");
        assert!(state.is_cashback && state.complete);
        // Non-zero quote passes through.
        let quote = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
            .expect("quote");
        data[CURVE_QUOTE_MINT_OFFSET..CURVE_QUOTE_MINT_OFFSET + 32]
            .copy_from_slice(&quote.to_bytes());
        let state = parse_curve_state(&pump, &data).expect("state");
        assert_eq!(state.quote_mint, quote);
        // Wrong owner / short data fail closed.
        let stranger = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(parse_curve_state(&stranger, &data).is_none());
        assert!(parse_curve_state(&pump, &data[..114]).is_none());
        // Quote rule + buyback rule.
        assert_eq!(resolve_quote_mint(&Pubkey::default()), WSOL_MINT);
        assert_eq!(resolve_quote_mint(&quote), quote);
        let wallet = Pubkey::from_str("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7")
            .expect("wallet");
        assert_eq!(
            buyback_account(&wallet, &WSOL_MINT, &WSOL_MINT),
            wallet,
            "SOL-paired: the wallet itself"
        );
        let token_prog = crate::instructions::TOKEN_PROGRAM_ID;
        assert_eq!(
            buyback_account(&wallet, &quote, &token_prog),
            user_ata_with_program(&wallet, &quote, &token_prog),
            "token-paired: the recipient quote ATA"
        );
    }
}
