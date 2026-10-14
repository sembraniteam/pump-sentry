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

use crate::instructions::{PumpSwapBuilder, user_ata_with_program};
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
    /// Slippage guard: max lamports to spend. The on-chain `max_sol_cost`
    /// (TRADE_V3.md: "most quote the buyer will pay") is buyer protection:
    /// the fee-inclusive recompute below can only LOWER it, never raise
    /// it — a stale guard fails closed at simulation (free), while an
    /// exceeded one would overpay for real.
    pub max_sol_cost: u64,
    /// Jito tip budget in lamports, paid as an in-bundle system transfer
    /// on `buy_jito`/`sell_jito` (plain-RPC `buy`/`sell` ignore it).
    pub tip_lamports: u64,
    /// Token creator (required): selects the v3 curve shape and is
    /// cross-checked against curve state (mismatch fails closed).
    /// `None` fails closed — legacy v1 creates are unsupported.
    pub creator: Option<Pubkey>,
    /// Slippage guard in basis points for the fee-inclusive guard
    /// recompute (see `quote_buy_guard`): the engine quotes the amount
    /// with protocol+creator fees, then ceilings + this margin.
    pub slippage_bps: u64,
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
    /// Token creator (required): cross-checked against curve state and
    /// selects the v3 sell shape. `None` fails closed — legacy sells are
    /// rejected on v3 curves, and no legacy dispatch remains.
    pub creator: Option<Pubkey>,
}

/// Bonding-curve trade state decoded from chain bytes (offsets in
/// [`ids`], verified live 2026-10-14 against a legacy-18-buy curve and a
/// `buy_v2` curve): creator @49..81 (vault-PDA cross-checked on both),
/// mayhem flag @81 (1 on the reserved-fee curve, 0 on the normal-fee
/// curve), cashback flag @82, quote mint @83..115 (zero on both =
/// SOL-paired → WSOL rule), complete @48 (1 on both migrated curves).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveState {
    /// Curve creator (vault-PDA source; cross-checked against the
    /// launch-time parse — mismatch fails closed, never guessed).
    pub creator: Pubkey,
    /// Trade quote mint (curve-zero resolved to WSOL).
    pub quote_mint: Pubkey,
    /// Mayhem-mode coin (reserved fee list; v3 supported).
    pub is_mayhem: bool,
    /// Cashback coin (v3 refused on-chain with 6094; v2 dispatch
    /// retired — cashback curves fail closed to manual exit).
    pub is_cashback: bool,
    /// Curve complete (reserves drained/migrated — curve trades refused).
    pub complete: bool,
    /// Virtual token reserves (constant-product quote input).
    pub vt: u64,
    /// Virtual quote reserves, lamports (constant-product quote input).
    pub vq: u64,
    /// Real (unbought) token reserves (hard cap for quotes).
    pub real_tokens: u64,
    /// Stored total supply (fee-tier market-cap input on mayhem).
    pub token_total_supply: u64,
    /// Coin's own creator rate in bps (0 = unconfigured → schedule).
    pub coin_creator_fee_bps: u64,
}

/// Decode [`CurveState`] from a curve account's owner + data. Pure:
/// `None` on wrong owner, short data, or undecodable slots — callers
/// fall back (buys) or fail closed (shape selection), never guess.
/// The zero-quote → WSOL rule is the official BUY.md convention
/// (`resolve_quote_mint` in the reference SDK).
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

/// Mint supply for fee-tier market caps on non-mayhem curves (SPL Mint
/// layout `supply @36..44`, shared by classic + Token-2022 bases).
/// `None` on any transport/shape failure — callers fall back to the
/// order guard instead of quoting blind.
async fn mint_supply(rpc: &RpcClient, mint: &Pubkey) -> Option<u64> {
    use crate::instructions::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};
    let account =
        match tokio::time::timeout(Duration::from_secs(10), rpc.get_account(mint)).await {
            Ok(Ok(account)) => account,
            _ => return None,
        };
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

/// Global fee inputs for quoting: the creator-fee gate plus the full
/// tier schedule. Owners verified (Global under pump, FeeConfig under
/// the fee program); the schedule tolerates trailing zero room.
/// `None` on any failure — callers keep the order guard.
async fn fetch_fee_inputs(rpc: &RpcClient) -> Option<(bool, crate::quote::FeeSchedule)> {
    use crate::instructions::{FEE_CONFIG_ACCOUNT, FEE_CONFIG_PROGRAM, GLOBAL_ACCOUNT};
    use crate::quote::{parse_fee_gate, parse_fee_schedule};
    let (global, schedule) = tokio::join!(
        async {
            let account = match tokio::time::timeout(
                Duration::from_secs(10),
                rpc.get_account(&GLOBAL_ACCOUNT),
            )
            .await
            {
                Ok(Ok(account)) => account,
                _ => return None,
            };
            if account.owner != crate::instructions::PUMP_PROGRAM_ID {
                return None;
            }
            parse_fee_gate(&account.data).ok()
        },
        async {
            let account = match tokio::time::timeout(
                Duration::from_secs(10),
                rpc.get_account(&FEE_CONFIG_ACCOUNT),
            )
            .await
            {
                Ok(Ok(account)) => account,
                _ => return None,
            };
            if account.owner != FEE_CONFIG_PROGRAM {
                return None;
            }
            parse_fee_schedule(&account.data).ok()
        }
    );
    Some((global?, schedule?))
}

/// Resolve the trade quote mint from curve state: the zero key legacy
/// curves store means SOL-paired → wrapped SOL (official BUY.md rule,
/// `resolve_quote_mint` in the reference SDK). Pure and unit-tested.
pub fn resolve_quote_mint(curve_quote: &Pubkey) -> Pubkey {
    if *curve_quote == Pubkey::default() {
        crate::instructions::WSOL_MINT
    } else {
        *curve_quote
    }
}

/// Resolve the v3 buyback account: the listed wallet itself on
/// SOL-paired quotes, its canonical quote ATA on token-paired quotes
/// (official TRADE_V3.md — the program never creates it, so token
/// quotes must pre-create it idempotently). Pure and unit-tested.
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
    /// Strict preflight: transport/timeout simulation failures reject
    /// the dispatch instead of proceeding with a warning. Default off
    /// (today's behavior); enable for live funds via `SIM_STRICT=true`.
    strict_sim: bool,
}

/// Read-only live funding snapshot (see
/// [`SwapExecutor::live_funding_report`]). `None` balances mean the read
/// failed (transport/shape) — callers warn and let per-trade checks
/// decide, never treat unknown as funded or as broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveFundingReport {
    /// Trading wallet address.
    pub payer: Pubkey,
    /// Native SOL balance in lamports (`None` = unreadable).
    pub native_balance: Option<u64>,
    /// Native lamports required (effective tip + 0.05 SOL reserve).
    pub native_required: u64,
    /// Wallet WSOL associated token account (funds every v3 buy).
    pub wsol_ata: Pubkey,
    /// WSOL balance in base units (`None` = missing/unreadable ATA).
    pub wsol_balance: Option<u64>,
}

impl LiveFundingReport {
    /// Known-funded natively (`None` when the balance is unreadable).
    pub fn native_ok(&self) -> Option<bool> {
        self.native_balance.map(|b| b >= self.native_required)
    }

    /// Known to hold spendable WSOL (`None` when unreadable).
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

    /// Strict preflight: simulation transport/timeout failures reject the
    /// dispatch (`SimFailed`) instead of proceeding unproven. Off by
    /// default (an unknown sim result preserves today's behavior rather
    /// than blocking valid trades); turn on for live funds.
    pub fn with_strict_sim(mut self, strict: bool) -> Self {
        self.strict_sim = strict;
        self
    }

    /// Tip budget that actually leaves the wallet on this path: the
    /// order's tip for Jito bundles, zero for plain-RPC sends (which
    /// never attach it). Callers pass whether the dispatch is bundled —
    /// demanding tip+reserve natively for a tipless send blocks valid
    /// trades when the balance covers the reserve but not a phantom tip.
    fn effective_tip(&self, tip_lamports: u64, bundled: bool) -> u64 {
        if bundled && self.jito.is_some() {
            tip_lamports
        } else {
            0
        }
    }

    /// Read-only live funding snapshot: native SOL (fees, tips, and the
    /// trade guard on SOL-paired quotes) plus the wallet WSOL balance
    /// (funds token-paired quotes only). SOL-paired v3 buys transfer
    /// native SOL (BUY.md; TRADE_V3.md [10] unused) — a WSOL float is
    /// NOT required for them. No funds move here; callers gate on the
    /// result before the first signal (native shortfall fails closed;
    /// zero WSOL merely informs, since per-trade preflight enforces the
    /// exact guard anyway).
    pub async fn live_funding_report(&self, tip_lamports: u64) -> LiveFundingReport {
        use crate::instructions::{WSOL_MINT, user_ata_with_program};
        let required = self
            .effective_tip(tip_lamports, true)
            .saturating_add(crate::wallet::MIN_SOL_RESERVE_LAMPORTS);
        let native_balance = match tokio::time::timeout(
            Duration::from_secs(15),
            self.rpc.get_balance(&self.wallet.pubkey()),
        )
        .await
        {
            Ok(Ok(balance)) => Some(balance),
            _ => None,
        };
        // WSOL is always classic SPL, but resolve the program off-chain
        // like every dispatch path instead of assuming it.
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

    /// Resolve the token program owning `mint` (classic SPL vs
    /// Token-2022): a swap naming the wrong program fails on-chain.
    /// Fail-closed (`None`) when the mint account is unreadable —
    /// building with a guessed program burns fees for certain.
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
        // Allowlist like `mint_supply` below: a garbage/closed mint owner
        // must fail closed here, not surface as a confusing sim failure
        // after extra RPCs.
        use crate::instructions::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};
        if account.owner != TOKEN_PROGRAM_ID && account.owner != TOKEN_2022_PROGRAM_ID {
            return None;
        }
        Some(account.owner)
    }

    /// Fee-inclusive spend guard for `amount` base units: fresh quote
    /// (protocol+creator fees included for the curve's tier) plus the
    /// slippage ceiling. Returns the order guard unchanged when
    /// anything is unreadable or unquotable — simulation still refuses
    /// a truly short guard (6002 class), so fallback is safe with a
    /// loud log. Cashback/complete curves skip the engine (their paths
    /// fail closed or fall back before reaching a guard).
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
                    // Clamp to the order ceiling: the recompute exists to
                    // price out 6002s with fresh fees, but it must never
                    // authorize MORE than the caller stated — a price move
                    // since sizing fails closed at simulation (free) instead
                    // of buying the spike (real loss).
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

    /// Quote-token balance of `ata` (`None` on transport/shape failure —
    /// callers proceed to simulation instead of refusing on unknowns).
    async fn quote_balance(&self, ata: &Pubkey) -> Option<u64> {
        match tokio::time::timeout(Duration::from_secs(10), self.rpc.get_token_account_balance(ata))
            .await
        {
            Ok(Ok(balance)) => balance.amount.parse().ok(),
            _ => None,
        }
    }

    /// Fetch and decode the curve's trade state for shape routing
    /// ([`CurveState`]: creator cross-check, cashback/complete flags,
    /// quote rule). One RPC, 10s deadline. `None` on any
    /// transport/shape failure — buys fall back to the launch-time
    /// parse plus the v2 reference path; sells fall back to the v2
    /// reference path. Never panics, never guesses.
    async fn read_curve_state(&self, curve: &Pubkey) -> Option<CurveState> {
        let account = match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_account(curve),
        )
        .await
        {
            Ok(Ok(account)) => account,
            _ => return None,
        };
        parse_curve_state(&account.owner, &account.data)
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
        let ixs = self.preflight_buy(order, false).await?;
        self.send(&ixs, "buy").await
    }

    /// Sell `amount_tokens` for at least `min_sol_output` lamports.
    pub async fn sell(&self, order: &SellOrder) -> Result<SwapReceipt, ExecutorError> {
        let ixs = self.preflight_sell(order, false).await?;
        self.send(&ixs, "sell").await
    }

    /// Buy via Jito bundle: swap ix plus the order's tip in one atomic
    /// transaction. Requires [`SwapExecutor::with_jito`].
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

    /// Shared buy path: solvency preflight plus swap instruction.
    /// `bundled` selects the solvency tip (Jito sends attach it, plain
    /// sends never do).
    async fn preflight_buy(
        &self,
        order: &BuyOrder,
        bundled: bool,
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
        // Route-aware solvency is decided below (after the guard
        // recompute): SOL-paired buys transfer NATIVE SOL (BUY.md;
        // TRADE_V3.md [10] is unused and need not exist), so the trade
        // guard must sit natively. Token-paired quotes debit the payer
        // quote ATA via TransferChecked, so natively only tip + reserve
        // must sit (quote-side cover is enforced in `preflight_buy_v3`
        // only where the quote engine prices it).
        // Nothing is spent before those checks.
        let payer = self.wallet.pubkey();
        // Curve state (creator cross-check, cashback/complete routing)
        // loads concurrently with the base token-program read: the two
        // RPCs are independent.
        let (base_program, curve_state) = tokio::join!(
            self.token_program(&order.mint),
            self.read_curve_state(&order.bonding_curve)
        );
        let Some(token_program) = base_program else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // v3-only routing FIRST (no RPC beyond the two above): legacy v1
        // creates, creator mismatches, complete and cashback-era curves
        // all fail closed before the quote/solvency round-trips burn
        // budget and widen the quote-to-land slot gap. New launches are
        // `create_v2` v3 curves; anything else is either ancient
        // (complete) or unprovable — loud diverged/manual upstream,
        // never an unproven dispatch.
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
            // Runbook if a NEW launch ever sets `is_cashback_enabled`
            // (create_v2 carries the flag): do NOT resurrect the deleted
            // v2 builders on theory. Required evidence, in order: (1) a
            // frozen buy_v2 + sell_v2 fill pair on the NEW curve, (2) a
            // byte-exact builder + sim GREEN probe like
            // `mainnet_v3_17_simulates`, (3) the same opaque-routing
            // provenance the old code demanded (reference, never guess).
            // Until then the diverged mark upstream is the whole story.
            return Err(ExecutorError::BadOrder(
                "cashback-era curve: v3 refused on-chain (6094) and v2 dispatch retired; refusing (fail-closed)".to_string(),
            ));
        }
        // Program-bound ATA: Token-2022 mints derive a DIFFERENT address
        // than the classic helper (verified live). The base-ATA existence
        // read and the fee-inclusive guard recompute are independent —
        // batch them instead of serializing on the sniper hot path. The
        // guard falls back to the order value with a loud log when the
        // quote is unreadable; simulation refuses a truly short guard.
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
        // Route-aware funding (v3-only): SOL-paired trades pay native
        // SOL, so the guard is asserted natively (trade + tip + reserve);
        // token-paired trades pay from the quote ATA, so natively only
        // tip + reserve must sit. No native-trade arm remains beyond
        // this: legacy/mayhem-18/v2 dispatch was retired (new launches
        // are v3 curves; anything else fails closed below).
        let is_sol_quote = curve_state
            .as_ref()
            .is_some_and(|s| s.quote_mint == crate::instructions::WSOL_MINT);
        let trade_lamports = if is_sol_quote { final_max } else { 0 };
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, trade_lamports, self.effective_tip(order.tip_lamports, bundled))
            .await?;
        info!(mint = %order.mint, balance, max_cost = final_max, "buy pre-flight ok");
        // Routing already decided above; re-bind for the call (the state
        // cannot have vanished, but guessing is worse than re-checking).
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

    /// v3 buy path: the 17-account shape with fully derived addresses
    /// plus the listed buyback pick — no same-curve reference needed
    /// (fees stay on the curve). The spend guard is the order's own
    /// `max_sol_cost`, never a copied value. Pre-creates: the payer
    /// quote ATA ([10]) and the buyback ATA on token-paired quotes
    /// only (the trade debits them via TransferChecked and the program
    /// never creates accounts). On SOL-paired quotes NO quote ATA is
    /// created: the trade transfers native SOL (BUY.md; TRADE_V3.md
    /// [10] is unused and need not exist) — the derived address is
    /// still passed for the seed constraint. The base-ATA prefix (if
    /// needed) is already in `ixs`.
    /// (Arity documented, not shrunk: the seven inputs are the full
    /// v3-buy context — order, chain accounts, guard, and routing state —
    /// and the sole caller passes all of them. Splitting the struct would
    /// scatter one shape's inputs across two types.)
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
        // Quote-side funding (TRADE_V3.md + BUY.md): on SOL-paired quotes
        // the trade transfers native SOL — [10] is unused and need not
        // exist, so no quote ATA is pre-created and no WSOL balance is
        // demanded (native cover was asserted in `preflight_buy`). An
        // earlier revision demanded a WSOL float here from a misread
        // live probe; the official docs refute it (a nonexistent account
        // cannot be debited). Token-paired quotes debit the payer quote
        // ATA via TransferChecked: pre-create it (idempotent) and let
        // simulation decide sufficiency.
        let user_quote =
            user_ata_with_program(payer, &quote_mint, &quote_program);
        if quote_mint == crate::instructions::WSOL_MINT {
            info!(mint = %order.mint, cost = max_sol_cost, "sol-pair buy pays native SOL; no quote ATA needed");
        } else {
            // The two existence reads are independent: batch them instead
            // of serializing on the sniper hot path.
            let (user_prefix, buyback_prefix) = tokio::join!(
                self.maybe_create_ata_ix(payer, payer, &quote_mint, &user_quote, &quote_program),
                // Anyone may fund the rent (official docs); the recipient owns it.
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

    /// Shared sell path: solvency preflight plus swap instruction.
    /// Sells spend no SOL beyond fees; still enforce reserve + tip.
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
                // Canonical ATA missing/unreadable: our own holdings are
                // always created via the idempotent ATA prefix, so this
                // is a transient RPC read (or a fund-drain outside the
                // bot) — not an aux-account layout. Third-party aux
                // holdings only mattered for the retired sell_v2 parser;
                // v3 fills are ours, canonical by construction.
                return Err(ExecutorError::BadOrder(
                    "sell ATA missing or unreadable".to_string(),
                ));
            }
            Err(_) => return Err(ExecutorError::Timeout("sell balance read")),
        }
        // v3-only routing (mirrors buys): creatorless orders, complete
        // curves, cashback-era curves, and unreadable curve state all
        // fail closed — every tracked bag is a v3 fill, so anything else
        // is unprovable (loud manual exit upstream, never a blind sell).
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

    /// v3 sell path: the 17-account shape, mirrored from
    /// [`SwapExecutor::preflight_buy_v3`] minus the base-ATA prefix —
    /// the caller's balance check already proved the base ATA holds the
    /// funds (and the reference SDK creates no base ATA on sells).
    /// Quote-ATA creates only on token-paired quotes. The floor is the
    /// order's own `min_sol_output`, never a copied value.
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
        // Payer quote ATA ([10]) receives the proceeds and must exist
        // (the program never creates accounts — same rule as the buy
        // path). Idempotent prefix: safe whether or not it exists.
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

    /// Create-ATA prefix when `owner`'s token account for `mint` is not
    /// known to exist (fresh wallet or first touch of the mint). `payer`
    /// funds the rent (anyone may pay — the v3 buyback ATA relies on
    /// this); `owner` holds the account. The idempotent variant makes
    /// the prefix safe on all paths: an existing ATA succeeds silently,
    /// and a race between the existence check and execution cannot fail
    /// the bundle. An existing account with a WRONG owner (address
    /// squat — practically impossible on derived ATAs) is NOT checked
    /// here: it fails closed at simulation instead (accepted: checking
    /// would force every caller fallible for a non-case).
    async fn maybe_create_ata_ix(
        &self,
        payer: &Pubkey,
        owner: &Pubkey,
        mint: &Pubkey,
        ata: &Pubkey,
        token_program: &Pubkey,
    ) -> Vec<solana_instruction::Instruction> {
        match tokio::time::timeout(Duration::from_secs(10), self.rpc.get_account(ata)).await {
            Ok(Ok(_)) => Vec::new(),
            _ => {
                use crate::instructions::build_create_ata_ix;
                vec![build_create_ata_ix(payer, owner, mint, ata, token_program)]
            }
        }
    }

    /// Append the tip transfer when the budget is non-zero. Uses the live
    /// tip set when a bundle client is attached (15-min refresh, silent
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
    /// Deadline-bound on top of the client's own confirmation loop. One
    /// fresh-blockhash resample on `BlockhashNotFound`, like `send_bundle`.
    /// No resend AFTER a send error by design: an errored send may
    /// already have landed, and re-sending the same spend under a fresh
    /// hash risks a double fill. Ambiguous sends surface as timeouts for
    /// the pipeline retry budget (sells) or diverged/manual review
    /// (buys) instead.
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

    /// Sign and dispatch as a single-transaction Jito bundle. One fresh-
    /// blockhash resample on `BlockhashNotFound`: under load the hash can
    /// expire between build and sim, and a retry is cheaper than a miss.
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

    /// Fetch a fresh blockhash and sign every instruction with the wallet.
    /// Deadline-bound: a hung blockhash stalls the whole dispatch.
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
    /// today's behavior rather than blocking valid trades — unless
    /// strict preflight is on ([`SwapExecutor::with_strict_sim`]), in
    /// which case they reject too (see [`sim_transport_outcome`]).
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

/// Decide an unproven preflight (simulation transport/timeout): fail
/// closed under strict mode, warn-and-proceed otherwise. Pure so the
/// policy is unit-tested without network.
fn sim_transport_outcome(strict: bool, cause: &str) -> Result<(), ExecutorError> {
    if strict {
        Err(ExecutorError::SimFailed(format!(
            "preflight unavailable ({cause}); strict mode refuses to dispatch unproven"
        )))
    } else {
        Ok(())
    }
}

/// True when a sim failure names an expired blockhash (retryable with one
/// fresh `build_tx`, not a layout refusal). Pure so the resample policy
/// is unit-tested without network.
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
        // Pure gate semantics: known balances decide, unknown reads
        // abstain (callers warn, never treat unknown as funded/broke).
        // Pubkeys are placeholders — only the balance logic is pinned.
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
        // Default (off): unknown sim result preserves today's behavior.
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
        // Synthetic 166-byte curve blob laid out exactly like the two
        // live-verified mainnet curves (creator @49..81, mayhem @81,
        // cashback @82, quote @83..115, complete @48): the parser is a
        // pure offset contract, tested without RPC.
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
        // Zero quote → WSOL rule (both live curves store zero).
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
        // Non-zero quote passes through (token-paired curve).
        let quote = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
            .expect("quote");
        data[CURVE_QUOTE_MINT_OFFSET..CURVE_QUOTE_MINT_OFFSET + 32]
            .copy_from_slice(&quote.to_bytes());
        let state = parse_curve_state(&pump, &data).expect("state");
        assert_eq!(state.quote_mint, quote);
        // Wrong owner / short data fail closed, never guess.
        let stranger = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(parse_curve_state(&stranger, &data).is_none());
        assert!(parse_curve_state(&pump, &data[..114]).is_none());
        // Quote rule + buyback rule, pure.
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
