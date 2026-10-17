//! Swap dispatch: solvency check, then build, sim, send.
//! RPC (`buy`/`sell`) or Jito bundle (`*_jito`).

use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient, rpc_config::RpcTransactionConfig,
};
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use solana_transaction_status::{
    EncodedTransaction, TransactionConfirmationStatus, TransactionStatus, UiMessage,
    UiTransactionEncoding, option_serializer::OptionSerializer,
};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tracing::{info, warn};

use crate::instructions::{PumpSwapBuilder, user_ata_with_program};
use crate::jito::{
    BundleStatus, JitoClient, JitoError, encode_bundle, nanos_seed, select_tip_account,
    tip_instruction,
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
    /// Slippage tolerance (bps) applied to the re-quoted fresh floor.
    /// Mirrors the sizing floor: an exact-net fresh leg fails on
    /// 1-lamport drift and burns the retry budget for nothing.
    pub slippage_bps: u64,
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

/// Post-migration PumpSwap sell: token -> WSOL on a validated pool.
/// Floor quotes from live reserves plus the fee schedule in preflight.
pub struct SellPumpSwapOrder {
    /// Token mint (pool base).
    pub mint: Pubkey,
    /// PumpSwap pool.
    pub pool: Pubkey,
    /// Token-side vault (pool base vault).
    pub base_vault: Pubkey,
    /// WSOL-side vault (pool quote vault).
    pub quote_vault: Pubkey,
    /// Token base units to sell.
    pub amount_tokens: u64,
    /// Slippage bps over the fee-aware quote (`>= 10000` refuses).
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
    /// Coin creator rate in bps (0 = unconfigured).
    pub coin_creator_fee_bps: u64,
}

/// Decode [`CurveState`]. Pure. `None` on wrong owner, wrong
/// discriminator, or short data: a pump-owned non-curve account must
/// refuse here, not produce garbage quotes for the sim to catch.
pub fn parse_curve_state(owner: &Pubkey, data: &[u8]) -> Option<CurveState> {
    use crate::instructions::{
        CURVE_CASHBACK_FLAG_OFFSET, CURVE_COIN_CREATOR_FEE_END, CURVE_COMPLETE_OFFSET,
        CURVE_CREATOR_END, CURVE_CREATOR_OFFSET, CURVE_MAYHEM_FLAG_OFFSET, CURVE_QUOTE_MINT_END,
        CURVE_QUOTE_MINT_OFFSET,
    };
    if *owner != crate::instructions::PUMP_PROGRAM_ID
        || !data.starts_with(&ids::BONDING_CURVE_DISCRIMINATOR)
        || data.len() < CURVE_COIN_CREATOR_FEE_END
    {
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
        coin_creator_fee_bps: u64at(115)?,
    })
}

/// [`crate::quote::CurveView`] from dispatch state. One constructor
/// so the buy guard and the sell floor quote the same curve.
fn curve_view_from_state(state: &CurveState) -> crate::quote::CurveView {
    crate::quote::CurveView {
        vt: state.vt,
        vq: state.vq,
        real_tokens: state.real_tokens,
        coin_creator_fee_bps: state.coin_creator_fee_bps,
        creator_is_default: state.creator == Pubkey::default(),
        quote: if state.quote_mint == crate::instructions::WSOL_MINT {
            crate::quote::QuoteKind::SolLike
        } else if state.quote_mint == crate::instructions::USDC_MINT {
            crate::quote::QuoteKind::Usdc
        } else {
            crate::quote::QuoteKind::Other
        },
    }
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

/// SPL token account amount: base layout `mint(32) owner(32)
/// amount(u64)` (Token-2022 extensions follow the base fields). `None`
/// when the data is too short — callers fail closed, never read 0.
fn token_account_amount(data: &[u8]) -> Option<u64> {
    data.get(64..72)?.try_into().ok().map(u64::from_le_bytes)
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

/// Chain status for a dispatched signature (RPC `getSignatureStatuses`
/// semantics). `Timeout` is NOT a failure: the transaction may still
/// land, so callers reconcile by signature before any replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Seen in a processed block (`confirmationStatus: processed`):
    /// not voted yet; a fork can still drop it. Never a fill verdict.
    Processed,
    /// Voted by a supermajority (`confirmed`): the fill verdict.
    Confirmed,
    /// Rooted (`finalized`): irreversible.
    Finalized,
    /// Landed and errored on-chain: fees burned, no state change.
    FailedOnChain,
    /// Not seen within the poll budget: unknown, MAY STILL LAND.
    Timeout,
}

impl ConfirmOutcome {
    /// True once the chain has the transaction in a block (any vote
    /// level): `Processed` can still vanish; `Confirmed`/`Finalized` are
    /// fill verdicts. `Timeout` and `FailedOnChain` never landed.
    pub fn landed(self) -> bool {
        matches!(self, Self::Processed | Self::Confirmed | Self::Finalized)
    }
}

/// Why a dispatch is provably dead: no fill, safe to rebuild or resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    /// Landed on-chain with a program error: no state change.
    FailedOnChain,
    /// Engine verdict: the bundle was never forwarded, it can never land.
    NeverForwarded,
    /// Blockhash no longer valid and the signature was never seen: the
    /// transaction can never land.
    Expired,
}

impl DeadReason {
    /// Short label for logs and caller reasons.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FailedOnChain => "errored on-chain",
            Self::NeverForwarded => "engine never forwarded the bundle",
            Self::Expired => "blockhash expired, never landed",
        }
    }
}

impl std::fmt::Display for DeadReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Post-dispatch verdict every caller decision derives from (opening,
/// tracking, releasing). `Unknown` must never be treated as failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchVerdict {
    /// Chain-verified landing (`confirmed`/`finalized`, or engine
    /// `Landed`). The only verdict that may open or retire a position.
    Landed,
    /// Provably dead: no fill, safe to rebuild or resend.
    Dead(DeadReason),
    /// Unresolved: MAY still land. Keep tracking; never resend blind.
    Unknown(&'static str),
}

/// Expiry evidence for an unseen signature (pure input to the verdict
/// table). Only `Expired` may close a dispatch as dead: `Unknown` is
/// every other outcome, including a missing hash (pre-upgrade WAL line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnseenExpiry {
    /// Blockhash answered invalid: cannot be included anymore.
    Expired,
    /// Blockhash still includable: the dispatch may still land.
    Live,
    /// No blockhash recorded, or the check could not run.
    Unknown,
}

/// Accepted Jito bundle receipt (acceptance != landing).
#[derive(Debug, Clone)]
pub struct JitoReceipt {
    /// Block-engine bundle UUID.
    pub bundle_id: String,
    /// First tx signature (base58) for tracking.
    pub signature: String,
    /// Dispatch blockhash: once invalid, an unseen dispatch can never
    /// land (the only proof of expiry).
    pub blockhash: Hash,
}

/// Live swap executor. Cloneable; clients and wallet are shared.
#[derive(Clone)]
pub struct SwapExecutor {
    rpc: Arc<RpcClient>,
    wallet: Arc<WalletGuard>,
    jito: Option<JitoClient>,
    compute_unit_limit: u32,
    compute_unit_price: u64,
    /// Strict preflight: sim transport failure rejects. On by default;
    /// paper paths opt out explicitly via [`Self::with_strict_sim`](false).
    strict_sim: bool,
    /// Curve fee-input cache: (gate_on, schedule, fetched_at).
    fee_cache: Arc<tokio::sync::Mutex<Option<(bool, crate::quote::FeeSchedule, std::time::Instant)>>>,
    /// Mint supply cache: (supply, fetched_at).
    supply_cache: Arc<tokio::sync::Mutex<HashMap<Pubkey, (u64, std::time::Instant)>>>,
}

/// Fee-input cache TTL. Same value as the paper reader twin
/// (`curve::fee::FEE_CACHE_TTL`): a schedule change must reach
/// dispatch within minutes — stale tiers quote sell floors loose,
/// so the window stays short (two small account reads per window).
pub const FEE_INPUTS_TTL: Duration = Duration::from_secs(300);

/// Read-only funding snapshot. `None` = unreadable, never funded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveFundingReport {
    /// Trading wallet.
    pub payer: Pubkey,
    /// Native SOL in lamports (`None` = unreadable).
    pub native_balance: Option<u64>,
    /// Native lamports required, scaled to a full book: every
    /// position pays buy + sell dispatches (tip + priority + base
    /// each), one ATA rent per new mint, plus the 0.05 SOL reserve.
    pub native_required: u64,
    /// Wallet WSOL ATA.
    pub wsol_ata: Pubkey,
    /// WSOL balance (`None` = missing/unreadable).
    pub wsol_balance: Option<u64>,
}

/// Trade principal for the funding gate (SOL-pair buys pay native).
#[derive(Debug, Clone, Copy)]
pub struct FundingPrincipal {
    /// Paper notional per trade, USD.
    pub notional_usd: f64,
    /// Slippage guard, bps (principal sized past it).
    pub slippage_bps: u64,
    /// SOL/USD for conversion.
    pub sol_usd: f64,
}

impl FundingPrincipal {
    /// Principal lamports per position, ceiled past slippage. `None`
    /// on garbage inputs (fail-closed: caller skips principal loudly,
    /// never boots a fiction).
    fn per_position_lamports(&self) -> Option<u128> {
        if !self.notional_usd.is_finite()
            || self.notional_usd <= 0.0
            || !self.sol_usd.is_finite()
            || self.sol_usd <= 0.0
        {
            return None;
        }
        let slipped = self.notional_usd * (10_000.0 + self.slippage_bps as f64) / 10_000.0;
        if !slipped.is_finite() || slipped <= 0.0 {
            return None;
        }
        let lamports = slipped / self.sol_usd * ids::LAMPORTS_PER_SOL as f64;
        if !lamports.is_finite() || lamports <= 0.0 {
            return None;
        }
        Some(lamports.ceil() as u128)
    }
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

/// Observed buy fill in chain units (entry-gap calibration input).
/// Priced by the caller (it owns `sol_usd` + mint decimals); this type
/// carries no USD so the executor stays chain-pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedBuyFill {
    /// Base tokens credited across the buyer's accounts for the mint.
    pub tokens_base_units: u64,
    /// Lamports that went into the swap leg: buyer SOL delta minus
    /// tip, priority, base fee, and created-account rent.
    pub swap_sol_lamports: u64,
}

/// Fill fetch deadline. One attempt, no retry: gap data is
/// calibration, not custody — a wedged RPC must not stall exits.
const FILL_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Buy-fill math from observed balance deltas. Pure (unit-pinned):
/// `None` on zero tokens, a non-positive swap leg, or any overflow —
/// calibration skips instead of fabricating a price. `created_atas`
/// counts buyer+mint token accounts with no pre balance (each paid one
/// rent-exempt reserve).
fn buy_fill_from_deltas(
    buyer_sol_spent_lamports: u64,
    tokens_base_units: u64,
    created_atas: u64,
    tip_lamports: u64,
    priority_lamports: u64,
) -> Option<ObservedBuyFill> {
    if tokens_base_units == 0 {
        return None;
    }
    let rent = created_atas.checked_mul(ids::ATA_RENT_LAMPORTS)?;
    let overhead = tip_lamports
        .checked_add(priority_lamports)?
        .checked_add(ids::BASE_TX_FEE_LAMPORTS)?
        .checked_add(rent)?;
    let swap_sol_lamports = buyer_sol_spent_lamports.checked_sub(overhead)?;
    if swap_sol_lamports == 0 {
        return None;
    }
    Some(ObservedBuyFill {
        tokens_base_units,
        swap_sol_lamports,
    })
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
        Ok(Self::with_rpc(
            Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            wallet,
        ))
    }

    /// Build around a shared RPC client (one connection pool per process).
    pub fn with_rpc(rpc: Arc<RpcClient>, wallet: WalletGuard) -> Self {
        Self {
            rpc,
            wallet: Arc::new(wallet),
            jito: None,
            compute_unit_limit: 0,
            compute_unit_price: 0,
            strict_sim: true,
            fee_cache: Arc::new(tokio::sync::Mutex::new(None)),
            supply_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Attach a Jito bundle client for protected dispatch.
    pub fn with_jito(mut self, jito: JitoClient) -> Self {
        self.jito = Some(jito);
        self
    }

    /// Strict preflight: sim transport/timeout failures reject (`SimFailed`).
    /// On by default; pass `false` explicitly for paper paths that may
    /// proceed unproven.
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

    /// Priority fee per dispatch, ceiled (`limit * price / 1e6`).
    /// Delegates to the shared [`crate::quote::priority_fee_lamports`].
    fn priority_fee_lamports(&self) -> u64 {
        crate::quote::priority_fee_lamports(self.compute_unit_limit, self.compute_unit_price)
    }

    /// Read-only funding snapshot. Moves no funds. Scaled to a full
    /// book — every position pays buy + sell dispatches, one ATA rent
    /// per new mint (assumes all mints new: conservative), trade
    /// principal per position, plus the reserve — so a green report
    /// means the wallet can actually run `max_open_positions`, not
    /// just cover the first touch. `None` principal (price unreadable)
    /// skips principal loudly, never boots a fiction.
    pub async fn live_funding_report(
        &self,
        tip_lamports: u64,
        max_open_positions: usize,
        principal: Option<FundingPrincipal>,
    ) -> LiveFundingReport {
        use crate::instructions::{WSOL_MINT, user_ata_with_program};
        let positions = max_open_positions.max(1) as u128;
        let per_dispatch = u128::from(self.effective_tip(tip_lamports, true))
            .saturating_add(u128::from(self.priority_fee_lamports()))
            .saturating_add(u128::from(ids::BASE_TX_FEE_LAMPORTS));
        let mut required_128 = per_dispatch
            .saturating_mul(positions)
            .saturating_mul(2)
            .saturating_add(u128::from(ids::ATA_RENT_LAMPORTS).saturating_mul(positions))
            .saturating_add(u128::from(crate::wallet::MIN_SOL_RESERVE_LAMPORTS));
        if let Some(p) = principal
            && let Some(each) = p.per_position_lamports()
        {
            required_128 = required_128.saturating_add(each.saturating_mul(positions));
        }
        // Overflow can only overstate (fail-closed boot), never understate.
        let required = u64::try_from(required_128).unwrap_or(u64::MAX);
        let native_balance = rpc_timeout(
            Duration::from_secs(15),
            self.rpc.get_balance(&self.wallet.pubkey()),
        )
        .await;
        // Same program resolution as every dispatch path. Report-only
        // fallback: dispatch paths fail closed when the mint is unreadable
        // (`token_program` -> `BadOrder`), while this snapshot still reports
        // a classic-ATA address so funding stays visible. Never use this
        // fallback to build a swap ix.
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

    /// Raw ATA balance of the trading wallet for `mint`, in base units.
    /// `Ok(None)` when the ATA does not exist yet (nothing held);
    /// `Err` when unreadable (mint/token program unknown, RPC doubt) —
    /// callers must fail closed and never read `Err` as zero.
    pub async fn observe_token_balance(&self, mint: &Pubkey) -> Result<Option<u64>, ExecutorError> {
        let Some(token_program) = self.token_program(mint).await else {
            return Err(ExecutorError::BadOrder(
                "mint account unreadable; token program unknown".to_string(),
            ));
        };
        let ata =
            crate::instructions::user_ata_with_program(&self.wallet.pubkey(), mint, &token_program);
        match tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&ata)).await {
            Ok(Ok(account)) => token_account_amount(&account.data)
                .map(Some)
                .ok_or_else(|| {
                    ExecutorError::BadOrder(format!(
                        "token account data too short for {mint} (layout change?); refusing"
                    ))
                }),
            Ok(Err(e)) if e.to_string().contains("AccountNotFound") => Ok(None),
            Ok(Err(e)) => Err(ExecutorError::Rpc(e)),
            Err(_) => Err(ExecutorError::Timeout("observe_token_balance")),
        }
    }

    /// Fee-aware sell floor. Re-quotes the net from live state and
    /// takes the tighter (higher) of the sized floor and the fresh net
    /// with slippage applied: a stale floor in a rising market offers
    /// no sandwich tightening, and an exact-net fresh leg fails on
    /// 1-lamport drift. Refuses on quote failure (exits retry next
    /// tick; a loose floor is real money, a missed sell is not).
    async fn quote_sell_floor(
        &self,
        mint: &Pubkey,
        state: &CurveState,
        amount: u64,
        fallback_min: u64,
        slippage_bps: u64,
    ) -> Result<u64, ExecutorError> {
        let refuse = |why: &str| {
            warn!(mint = %mint, "sell re-quote failed ({why}); refusing loose floor (fail-closed)");
            ExecutorError::BadOrder(format!("sell re-quote failed ({why}); refusing"))
        };
        let (fees, supply) = tokio::join!(
            self.fee_inputs(),
            self.supply_cached(mint),
        );
        let (Some((gate_on, schedule)), Some(supply)) = (fees, supply) else {
            return Err(refuse("fee inputs unreadable"));
        };
        let view = curve_view_from_state(state);
        match crate::quote::sell_out_after_fees(&schedule, gate_on, &view, supply, amount) {
            Ok(net) => {
                let fresh = crate::quote::curve_sell_floor(net, slippage_bps)
                    .ok_or_else(|| refuse("fresh floor unguardable"))?;
                let floor = fresh.max(fallback_min);
                if floor != fallback_min {
                    info!(mint = %mint, quoted = net, floor, fallback = fallback_min,
                        "fee-aware sell floor recomputed");
                }
                Ok(floor)
            }
            Err(e) => Err(refuse(&format!("quote failed ({e})"))),
        }
    }

    /// Curve fee inputs behind the TTL cache. `None` on failure.
    async fn fee_inputs(&self) -> Option<(bool, crate::quote::FeeSchedule)> {
        if let Some((gate, schedule, at)) = self.fee_cache.lock().await.clone()
            && at.elapsed() < FEE_INPUTS_TTL
        {
            return Some((gate, schedule));
        }
        let fresh = fetch_fee_inputs(&self.rpc).await?;
        *self.fee_cache.lock().await =
            Some((fresh.0, fresh.1.clone(), std::time::Instant::now()));
        Some((fresh.0, fresh.1))
    }

    /// Mint supply behind the TTL cache. `None` on failure.
    async fn supply_cached(&self, mint: &Pubkey) -> Option<u64> {
        if let Some((supply, at)) = self.supply_cache.lock().await.get(mint).copied()
            && at.elapsed() < FEE_INPUTS_TTL
        {
            return Some(supply);
        }
        let supply = mint_supply(&self.rpc, mint).await?;
        self.supply_cache
            .lock()
            .await
            .insert(*mint, (supply, std::time::Instant::now()));
        Some(supply)
    }

    /// Fee-inclusive spend guard. Falls back to the order guard when
    /// unreadable. Never exceeds the order ceiling.
    async fn quote_buy_guard(        &self,
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
            self.fee_inputs(),
            self.supply_cached(mint),
        );
        let (Some((gate_on, schedule)), Some(supply)) = (fees, supply) else {
            warn!(mint = %mint, "fee inputs unreadable; keeping sized guard (sim decides)");
            return (fallback_max, None);
        };
        let view = curve_view_from_state(state);
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

    /// Observe a landed buy fill from its confirmed transaction
    /// (entry-gap calibration). Sums token credits across ALL buyer+mint
    /// accounts (canonical or aux — no ATA derivation needed) and nets
    /// the buyer SOL delta against the known overhead (tip, priority,
    /// base fee, created-account rent). `None` on any failure or
    /// ambiguity: single attempt under `FILL_FETCH_TIMEOUT`, no retry —
    /// calibration skips, custody never waits on it. Call only for
    /// confirmed fills; a failed tx has no meaningful fill.
    pub async fn observe_buy_fill(
        &self,
        signature_str: &str,
        mint_str: &str,
        tip_lamports: u64,
    ) -> Option<ObservedBuyFill> {
        let signature = solana_signature::Signature::from_str(signature_str).ok()?;
        let fetched = tokio::time::timeout(
            FILL_FETCH_TIMEOUT,
            self.rpc.get_transaction_with_config(
                &signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    commitment: None,
                    max_supported_transaction_version: Some(1),
                },
            ),
        )
        .await
        .ok()?
        .ok()?;
        let meta = fetched.transaction.meta.as_ref()?;
        if meta.err.is_some() {
            return None;
        }
        let EncodedTransaction::Json(ui_tx) = &fetched.transaction.transaction else {
            return None;
        };
        let UiMessage::Raw(raw) = &ui_tx.message else {
            return None;
        };
        let buyer = self.wallet.pubkey().to_string();
        let buyer_idx = raw.account_keys.iter().position(|k| k == &buyer)?;
        let pre_sol = meta.pre_balances.get(buyer_idx)?;
        let post_sol = meta.post_balances.get(buyer_idx)?;
        let buyer_sol_spent = pre_sol.checked_sub(*post_sol)?;
        let pre_by_account: std::collections::HashMap<u8, u64> = match &meta.pre_token_balances {
            OptionSerializer::Some(list) => list
                .iter()
                .filter_map(|b| {
                    (b.mint == mint_str
                        && matches!(&b.owner, OptionSerializer::Some(o) if o == &buyer))
                    .then(|| {
                        b.ui_token_amount.amount.parse::<u64>().ok().map(|a| (b.account_index, a))
                    })?
                })
                .collect(),
            _ => Default::default(),
        };
        let mut tokens_base_units = 0u64;
        let mut created_atas = 0u64;
        let mut matched = false;
        if let OptionSerializer::Some(list) = &meta.post_token_balances {
            for b in list {
                if b.mint != mint_str
                    || !matches!(&b.owner, OptionSerializer::Some(o) if o == &buyer)
                {
                    continue;
                }
                matched = true;
                let post: u64 = b.ui_token_amount.amount.parse().ok()?;
                match pre_by_account.get(&b.account_index) {
                    Some(pre) => {
                        tokens_base_units =
                            tokens_base_units.checked_add(post.checked_sub(*pre)?)?;
                    }
                    None => {
                        created_atas = created_atas.checked_add(1)?;
                        tokens_base_units = tokens_base_units.checked_add(post)?;
                    }
                }
            }
        }
        if !matched {
            return None;
        }
        buy_fill_from_deltas(
            buyer_sol_spent,
            tokens_base_units,
            created_atas,
            tip_lamports,
            self.priority_fee_lamports(),
        )
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

    /// Poll a signature until it lands or `timeout` ends. Classifies per
    /// RPC semantics: an unseen signature is `Timeout` (unknown, never
    /// failure) and a seen-but-unvoted one reports `Processed`, so
    /// callers never open a position on it.
    pub async fn confirm_signature(
        &self,
        signature_str: &str,
        timeout: Duration,
    ) -> Result<ConfirmOutcome, ExecutorError> {
        use std::time::Instant;
        let signature = parse_signature(signature_str)?;
        let start = Instant::now();
        let mut processed = false;
        loop {
            // One hung RPC stalls only this round.
            let status = match tokio::time::timeout(
                Duration::from_secs(15),
                self.rpc.get_signature_statuses(&[signature]),
            )
            .await
            {
                Ok(Ok(res)) => res.value.into_iter().next().flatten(),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => None,
            };
            if let Some(status) = status {
                match classify_status(&status) {
                    ConfirmOutcome::Processed => processed = true,
                    verdict => return Ok(verdict),
                }
            }
            if start.elapsed() >= timeout {
                // Seen but unvoted is not "unseen": keep them distinct.
                return Ok(if processed {
                    ConfirmOutcome::Processed
                } else {
                    ConfirmOutcome::Timeout
                });
            }
            // Deterministic stagger (no rand dep): concurrent confirms
            // for different mints desync instead of bursting the RPC in
            // lockstep every second.
            let round = (start.elapsed().as_secs() % 4) * 125;
            tokio::time::sleep(Duration::from_millis(1000 + round)).await;
        }
    }

    /// One-shot full-history reconciliation by signature (recent cache
    /// plus rooted ledger history). `Ok(None)` = never seen; `Err` =
    /// unverifiable (callers fail closed and keep holding). Never
    /// `Timeout`: a seen status always carries a chain state.
    pub async fn reconcile_signature(
        &self,
        signature_str: &str,
    ) -> Result<Option<ConfirmOutcome>, ExecutorError> {
        let signature = parse_signature(signature_str)?;
        let res = tokio::time::timeout(
            Duration::from_secs(15),
            self.rpc.get_signature_statuses_with_history(&[signature]),
        )
        .await
        .map_err(|_| ExecutorError::Timeout("get_signature_statuses_with_history"))??;
        Ok(res
            .value
            .into_iter()
            .next()
            .flatten()
            .map(|status| classify_status(&status)))
    }

    /// Expiry evidence for a possibly-dispatch blockhash. Only an
    /// explicit "invalid" answer from `isBlockhashValid` is `Expired`;
    /// every doubt (missing/unparsable hash, RPC trouble) stays
    /// `Unknown` and never releases a dispatch. The blockhash dimension
    /// is what makes expiry precise: wall-clock age is not a proof
    /// (validity is 150 slots, which a degraded cluster can stretch).
    async fn unseen_expiry(&self, blockhash: Option<&str>) -> UnseenExpiry {
        let Some(hash) = blockhash.and_then(|b| Hash::from_str(b).ok()) else {
            return UnseenExpiry::Unknown;
        };
        match tokio::time::timeout(
            Duration::from_secs(15),
            self.rpc
                .is_blockhash_valid(&hash, CommitmentConfig::confirmed()),
        )
        .await
        {
            Ok(Ok(false)) => UnseenExpiry::Expired,
            Ok(Ok(true)) => UnseenExpiry::Live,
            _ => UnseenExpiry::Unknown,
        }
    }

    /// Engine-side bundle state. `Unknown` without a bundle client or
    /// on any status trouble: callers keep their existing behavior.
    pub async fn bundle_status(&self, bundle_id: &str) -> BundleStatus {
        match &self.jito {
            Some(jito) => jito.inflight_status(bundle_id).await,
            None => BundleStatus::Unknown,
        }
    }

    /// Resolve a dispatched bundle+signature into one verdict. Chain
    /// beats engine: a landed signature decides; a timeout asks the
    /// engine — whose terminal verdicts (Landed, Failed) are taken as
    /// given — then reconciles by signature over full history before
    /// any terminal release. Only an unseen signature with a provably
    /// dead blockhash may close as expired; unverifiable reconciles and
    /// unprovable expiry stay `Unknown`, which never authorizes a
    /// resend (the caller's own bound releases those, loudly).
    pub async fn resolve_dispatch(
        &self,
        bundle_id: &str,
        signature: &str,
        blockhash: Option<&str>,
        timeout: Duration,
    ) -> DispatchVerdict {
        match self.confirm_signature(signature, timeout).await {
            Ok(ConfirmOutcome::Timeout) | Err(_) => {}
            Ok(outcome) => return verdict_from_chain(outcome),
        }
        let engine = self.bundle_status(bundle_id).await;
        let (reconciled, expiry) = match engine {
            // Engine terminal states need no chain second-opinion: the
            // poll above already checked the chain for this signature.
            BundleStatus::Landed | BundleStatus::Failed | BundleStatus::Pending => {
                (None, UnseenExpiry::Unknown)
            }
            // Forgot (5-minute lookback) or unreadable: no testimony at
            // all. The signature is the reconciliation key, and only
            // "unseen" opens the expiry question.
            BundleStatus::Invalid | BundleStatus::Unknown => {
                match self.reconcile_signature(signature).await {
                    Ok(seen) => {
                        let expiry = if seen.is_none() {
                            self.unseen_expiry(blockhash).await
                        } else {
                            UnseenExpiry::Unknown
                        };
                        (seen, expiry)
                    }
                    // Unverifiable: proof of nothing; keep it unknown.
                    Err(_) => return DispatchVerdict::Unknown("signature reconcile unavailable"),
                }
            }
        };
        timeout_verdict(engine, reconciled, expiry)
    }

    /// Landed-tip floor in lamports. 0 without a bundle client or on
    /// any oracle trouble: callers use the configured tip.
    pub async fn tip_floor_lamports(&self) -> u64 {
        match &self.jito {
            Some(jito) => jito.tip_floor_lamports().await,
            None => 0,
        }
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
            .assert_solvency(&self.rpc, 0, self.effective_tip(order.tip_lamports, bundled), self.priority_fee_lamports())
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "raydium sell pre-flight ok");
        let payer = self.wallet.pubkey();
        // Source ATA must be the account the swap spends from. Classic
        // only: upstream requires `[0] == spl_token::id()`, so a
        // Token-2022 mint fails here (not after burning sim budget).
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        if token_program != crate::instructions::TOKEN_PROGRAM_ID {
            warn!(mint = %order.mint, "Token-2022 mint on a Raydium V4 pool: unroutable by program construction; refusing (fail-closed, manual exit)");
            return Err(ExecutorError::BadOrder(
                "Token-2022 mint unroutable via Raydium V4; refusing (fail-closed)".to_string(),
            ));
        }
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
        // Reserves read raw: accrued PnL (`need_take_pnl_*`) is not
        // subtracted (accepted ~0.03% tolerance; the sim gate decides).
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
        let (Some(base_reserve), Some(quote_reserve)) = (
            token_account_amount(&base_vault.data),
            token_account_amount(&quote_vault.data),
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

    /// Post-migration PumpSwap sell via Jito. Floor quotes from live
    /// reserves plus the fee schedule in preflight.
    pub async fn sell_pumpswap_jito(
        &self,
        order: &SellPumpSwapOrder,
    ) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_sell_pumpswap(order, true).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "sell-pumpswap-jito").await
    }

    /// PumpSwap sell checks, then the 17-account `sell_v2` shape.
    /// Sim gates the bundle, so a stale reserve read dies free.
    /// No fee-input cache: exits are rare and retry-bounded, and every
    /// leg re-reads live reserves anyway.
    async fn preflight_sell_pumpswap(
        &self,
        order: &SellPumpSwapOrder,
        bundled: bool,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        use crate::instructions::{
            DEFAULT_BUYBACK_FEE_RECIPIENT, PUMP_AMM_FEE_CONFIG, PUMP_AMM_PROGRAM_ID, TOKEN_PROGRAM_ID,
            WSOL_MINT, user_ata_with_program,
        };
        use crate::quote::{
            market_cap, parse_amm_creator_gate, parse_fee_schedule, pumpswap_fee_bps,
            pumpswap_sell_floor, pumpswap_sell_out,
        };
        use migration::pumpswap::PumpSwapPool;
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder(
                "pumpswap sell amount_tokens is 0".to_string(),
            ));
        }
        if order.slippage_bps >= 10_000 {
            return Err(ExecutorError::BadOrder(
                "pumpswap sell slippage >= 100%: refusing unguarded floor".to_string(),
            ));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, self.effective_tip(order.tip_lamports, bundled), self.priority_fee_lamports())
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "pumpswap sell pre-flight ok");
        let payer = self.wallet.pubkey();
        // PumpSwap supports both token programs: the vault owners decide.
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        let source_ata = user_ata_with_program(&payer, &order.mint, &token_program);
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_token_account_balance(&source_ata),
        )
        .await
        {
            Ok(Ok(balance)) => {
                let held: u64 = balance.amount.parse().map_err(|_| {
                    ExecutorError::BadOrder(format!(
                        "pumpswap sell balance unreadable for {} (RPC shape change?); refusing",
                        order.mint
                    ))
                })?;
                if held < order.amount_tokens {
                    return Err(ExecutorError::BadOrder(format!(
                        "pumpswap sell amount {} exceeds ATA balance {held}",
                        order.amount_tokens
                    )));
                }
            }
            Ok(Err(_)) => {
                return Err(ExecutorError::BadOrder(
                    "pumpswap sell ATA missing or unreadable".to_string(),
                ));
            }
            Err(_) => return Err(ExecutorError::Timeout("pumpswap sell balance read")),
        }
        // Pool, vaults, fee inputs, supply: one joined round.
        let (pool, base_vault, quote_vault, global, schedule, supply) = tokio::join!(
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&order.pool)).await
            },
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&order.base_vault)).await
            },
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&order.quote_vault)).await
            },
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&ids::PUMP_AMM_GLOBAL_CONFIG)).await
            },
            async {
                tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&PUMP_AMM_FEE_CONFIG)).await
            },
            async { self.supply_cached(&order.mint).await }
        );
        let (Ok(Ok(pool)), Ok(Ok(base_vault)), Ok(Ok(quote_vault))) = (pool, base_vault, quote_vault)
        else {
            return Err(ExecutorError::BadOrder(
                "pumpswap pool or vaults unreadable; refusing unroutable dispatch (fail-closed)"
                    .to_string(),
            ));
        };
        // Trust gate: AMM owner, our mint at base, WSOL quote.
        let Some(parsed) = PumpSwapPool::parse_validated(
            &order.pool,
            &pool.data,
            &pool.owner,
            &order.mint,
        ) else {
            return Err(ExecutorError::BadOrder(
                "pumpswap pool failed trust gate (owner/mint/quote); refusing (fail-closed)"
                    .to_string(),
            ));
        };
        if parsed.is_cashback {
            warn!(mint = %order.mint, "cashback pool: sell_v2 refuses on-chain (6079); v1 fallback is manual-only");
            return Err(ExecutorError::BadOrder(
                "cashback pool: sell_v2 unsupported; refusing (fail-closed)".to_string(),
            ));
        }
        // Order vaults must be the pool's own (wiring guard).
        if order.base_vault != parsed.base_vault || order.quote_vault != parsed.quote_vault {
            return Err(ExecutorError::BadOrder(
                "pumpswap order vaults differ from pool vaults; refusing".to_string(),
            ));
        }
        // Vault programs decide the ix programs (classic or 2022).
        for (vault, role) in [(&base_vault, "base"), (&quote_vault, "quote")] {
            use crate::instructions::TOKEN_2022_PROGRAM_ID;
            if vault.owner != TOKEN_PROGRAM_ID && vault.owner != TOKEN_2022_PROGRAM_ID {
                return Err(ExecutorError::BadOrder(format!(
                    "pumpswap {role} vault owner is not a token program; refusing"
                )));
            }
        }
        let (Some(base_reserve), Some(quote_reserve)) = (
            token_account_amount(&base_vault.data),
            token_account_amount(&quote_vault.data),
        ) else {
            return Err(ExecutorError::BadOrder(
                "pumpswap vault balances unreadable; refusing".to_string(),
            ));
        };
        // Fee inputs: global gates + schedule, mint supply for tiers.
        let (Ok(Ok(global)), Ok(Ok(fee_config))) = (global, schedule) else {
            return Err(ExecutorError::BadOrder(
                "pumpswap fee inputs unreadable; refusing unpriced dispatch (fail-closed)"
                    .to_string(),
            ));
        };
        if global.owner != PUMP_AMM_PROGRAM_ID {
            return Err(ExecutorError::BadOrder(
                "pumpswap global config owner mismatch; refusing".to_string(),
            ));
        }
        if fee_config.owner != crate::instructions::FEE_CONFIG_PROGRAM {
            return Err(ExecutorError::BadOrder(
                "pumpswap fee config owner mismatch; refusing".to_string(),
            ));
        }
        let gate_on =
            parse_amm_creator_gate(&global.data).map_err(|_| {
                ExecutorError::BadOrder("pumpswap global gate unparsable; refusing".to_string())
            })?;
        if global
            .data
            .get(ids::PUMP_AMM_GLOBAL_DISABLE_FLAGS_OFFSET)
            .copied()
            .unwrap_or(1)
            != 0
        {
            warn!(mint = %order.mint, "pumpswap disable_flags set: admin-halted instructions; refusing (fail-closed)");
            return Err(ExecutorError::BadOrder(
                "pumpswap instructions disabled by admin; refusing".to_string(),
            ));
        }
        let schedule =
            parse_fee_schedule(&fee_config.data).map_err(|_| {
                ExecutorError::BadOrder("pumpswap fee schedule unparsable; refusing".to_string())
            })?;
        // Effective quote: vault plus signed virtual reserves.
        let eff_quote = (i128::from(quote_reserve))
            .checked_add(parsed.virtual_quote_reserves)
            .and_then(|v| u64::try_from(v).ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| {
                ExecutorError::BadOrder("pumpswap effective reserves non-positive; refusing".to_string())
            })?;
        let real_cap = u64::try_from(parsed.fee_buckets)
            .ok()
            .and_then(|buckets| quote_reserve.checked_sub(buckets))
            .ok_or_else(|| {
                ExecutorError::BadOrder("pumpswap fee buckets exceed vault; refusing".to_string())
            })?;
        let circulating = if parsed.is_mayhem {
            ids::PUMP_AMM_MAYHEM_SUPPLY
        } else {
            supply.ok_or_else(|| {
                ExecutorError::BadOrder("pumpswap mint supply unreadable; refusing unpriced dispatch".to_string())
            })?
        };
        let mcap = market_cap(eff_quote, base_reserve, circulating).map_err(|_| {
            ExecutorError::BadOrder("pumpswap market cap uncomputable; refusing".to_string())
        })?;
        let fees = pumpswap_fee_bps(
            &schedule,
            gate_on,
            parsed.creator_fee_bps,
            parsed.coin_creator == Pubkey::default(),
            mcap,
        )
        .map_err(|_| {
            ExecutorError::BadOrder("pumpswap fee tier unresolvable; refusing".to_string())
        })?;
        let net = pumpswap_sell_out(
            &crate::quote::PoolView {
                base_reserve,
                eff_quote_reserve: eff_quote,
                real_quote_cap: real_cap,
                lp_bps: fees.lp,
                proto_bps: fees.protocol,
                creator_bps: fees.creator,
            },
            order.amount_tokens,
        )
        .map_err(|_| {
            ExecutorError::BadOrder("pumpswap quote unroutable at this size".to_string())
        })?;
        let floor = pumpswap_sell_floor(net, order.slippage_bps).ok_or_else(|| {
            ExecutorError::BadOrder("pumpswap floor unguardable (dust); refusing".to_string())
        })?;
        // Quote-side WSOL ATA [6] must exist; prefix is idempotent.
        let quote_program = &quote_vault.owner;
        let user_quote = user_ata_with_program(&payer, &WSOL_MINT, quote_program);
        let mut ixs = Vec::new();
        ixs.extend(
            self.maybe_create_ata_ix(&payer, &payer, &WSOL_MINT, &user_quote, quote_program)
                .await,
        );
        // Buyback recipient ATA must exist; never funded by the trade.
        let buyback = user_ata_with_program(
            &DEFAULT_BUYBACK_FEE_RECIPIENT,
            &WSOL_MINT,
            quote_program,
        );
        match tokio::time::timeout(RPC_READ_TIMEOUT, self.rpc.get_account(&buyback)).await {
            Ok(Ok(_)) => {}
            _ => {
                return Err(ExecutorError::BadOrder(
                    "pumpswap buyback recipient ATA missing; refusing (never funded by the trade)"
                        .to_string(),
                ));
            }
        }
        ixs.push(crate::instructions::PumpSwapBuilder::build_sell_pumpswap_v2_ix(
            order.amount_tokens,
            floor,
            &crate::instructions::PumpAmmSellV2Accounts {
                pool: &order.pool,
                user: &payer,
                base_mint: &order.mint,
                quote_mint: &WSOL_MINT,
                user_base_token_account: &source_ata,
                user_quote_token_account: &user_quote,
                pool_base_token_account: &order.base_vault,
                pool_quote_token_account: &order.quote_vault,
                base_token_program: &token_program,
                quote_token_program: quote_program,
                buyback_fee_recipient: &buyback,
            },
        ));
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
        if order.slippage_bps >= 10_000 {
            return Err(ExecutorError::BadOrder(
                "buy slippage >= 100%: refusing unguarded ceiling".to_string(),
            ));
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
            .assert_solvency(&self.rpc, trade_lamports, self.effective_tip(order.tip_lamports, bundled), self.priority_fee_lamports())
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
            // Fail fast on a short quote balance (sim would catch it,
            // but a sim outage in non-strict mode must not reach chain).
            match self.quote_balance(&user_quote).await {
                Some(held) if held >= max_sol_cost => {}
                _ => {
                    return Err(ExecutorError::BadOrder(
                        "token-pair quote balance below max cost; refusing".to_string(),
                    ));
                }
            }
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
        // Zero floor authorizes any fill price (sandwich drain): fail closed,
        // mirroring the buy-side max_sol_cost == 0 refusal above.
        if order.min_sol_output == 0 {
            return Err(ExecutorError::BadOrder("sell min_sol_output is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, self.effective_tip(order.tip_lamports, bundled), self.priority_fee_lamports())
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
        // Tighten the floor against live reserves (see `quote_sell_floor`).
        let floor = self
            .quote_sell_floor(
                &order.mint,
                state,
                order.amount_tokens,
                order.min_sol_output,
                order.slippage_bps,
            )
            .await?;
        let mut ixs = Vec::new();
        // Quote ATA [10] must exist; prefix is idempotent. SOL pairs
        // skip it: the account is unused and needs neither read nor rent.
        if quote_mint != crate::instructions::WSOL_MINT {
            let user_quote =
                user_ata_with_program(payer, &quote_mint, &quote_program);
            ixs.extend(
                self.maybe_create_ata_ix(payer, payer, &quote_mint, &user_quote, &quote_program)
                    .await,
            );
        }
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
            floor,
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
                // Unreadable counts as missing (idempotent create decides
                // at sim): transport trouble, not proof of absence.
                tracing::debug!(%ata, "ATA unreadable; prefixing idempotent create");
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

    /// Sign and dispatch as a single-tx Jito bundle. One blockhash resample.
    /// This is the ONLY dispatch path: sniper flow never touches the
    /// public mempool (sandwich exposure). Do NOT add a mempool send
    /// without a written MEV analysis.
    async fn send_bundle(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<JitoReceipt, ExecutorError> {
        let jito = self.jito.clone().ok_or(ExecutorError::NoJito)?;
        let (mut tx, mut blockhash) = self.build_tx(ixs).await?;
        match self.simulate_tx(&tx, label).await {
            Ok(()) => {}
            Err(e) if is_blockhash_expired(&e) => {
                warn!(%label, "stale blockhash under load; resampling once");
                (tx, blockhash) = self.build_tx(ixs).await?;
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
                    blockhash,
                })
            }
            Ok(_) => {
                warn!(%label, "bundle engine returned empty id");
                Err(ExecutorError::Jito(JitoError::Rpc(
                    "empty bundle id".to_string(),
                )))
            }
            Err(e) => {
                // Transport failure is not a refusal: a timed-out POST
                // may still land, so callers park instead of resending.
                warn!(%label, error = %e, "bundle send failed (no resend: a timeout may still land)");
                Err(ExecutorError::Jito(e))
            }
        }
    }

    /// Fresh blockhash + wallet signature. Deadline-bound.
    async fn build_tx(
        &self,
        ixs: &[solana_instruction::Instruction],
    ) -> Result<(Transaction, Hash), ExecutorError> {
        let blockhash = tokio::time::timeout(
            Duration::from_secs(15),
            self.rpc.get_latest_blockhash(),
        )
        .await
        .map_err(|_| ExecutorError::Timeout("get_latest_blockhash"))??;
        // Budget prefix first; empty by default.
        let mut all = self.budget_prefix();
        all.extend_from_slice(ixs);
        Ok((
            Transaction::new_signed_with_payer(
                &all,
                Some(&self.wallet.pubkey()),
                &[self.wallet.keypair()],
                blockhash,
            ),
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
                warn!(%label, error = %ids::strip_error_url(&e.to_string()), "UNPROVEN-DISPATCH simulation RPC failed; strict refuses, non-strict proceeds without preflight");
                sim_transport_outcome(self.strict_sim, "RPC failure")
            }
            Err(_) => {
                warn!(%label, "UNPROVEN-DISPATCH simulation timed out; strict refuses, non-strict proceeds without preflight");
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

/// Parse a base58 signature without touching the network. Pure.
fn parse_signature(value: &str) -> Result<solana_signature::Signature, ExecutorError> {
    solana_signature::Signature::from_str(value).map_err(|e| ExecutorError::BadSignature {
        value: value.to_string(),
        msg: e.to_string(),
    })
}

/// Map one RPC signature status onto the chain-status vocabulary
/// (`confirmation_status()` carries the legacy fallback). Pure.
fn classify_status(status: &TransactionStatus) -> ConfirmOutcome {
    if status.err.is_some() {
        return ConfirmOutcome::FailedOnChain;
    }
    match status.confirmation_status() {
        TransactionConfirmationStatus::Processed => ConfirmOutcome::Processed,
        TransactionConfirmationStatus::Confirmed => ConfirmOutcome::Confirmed,
        TransactionConfirmationStatus::Finalized => ConfirmOutcome::Finalized,
    }
}

/// Map a chain status onto a dispatch verdict. Pure.
fn verdict_from_chain(outcome: ConfirmOutcome) -> DispatchVerdict {
    match outcome {
        ConfirmOutcome::Confirmed | ConfirmOutcome::Finalized => DispatchVerdict::Landed,
        ConfirmOutcome::FailedOnChain => DispatchVerdict::Dead(DeadReason::FailedOnChain),
        // Seen but unvoted: may still vanish on a fork, never a fill verdict.
        ConfirmOutcome::Processed => DispatchVerdict::Unknown("seen processed; not voted yet"),
        // Unseen: needs the engine/reconcile/expiry chain to resolve.
        ConfirmOutcome::Timeout => DispatchVerdict::Unknown("unseen within the poll budget"),
    }
}

/// Resolution table once the signature poll timed out. Pure: pinned by
/// tests so no call site can drift back to "timeout means failure".
fn timeout_verdict(
    engine: BundleStatus,
    reconciled: Option<ConfirmOutcome>,
    expiry: UnseenExpiry,
) -> DispatchVerdict {
    // Chain beats engine: a reconciled signature decides outright.
    if let Some(outcome) = reconciled {
        return verdict_from_chain(outcome);
    }
    match engine {
        // The engine saw it land: chain truth, even if the poll lagged.
        BundleStatus::Landed => DispatchVerdict::Landed,
        // Never forwarded: no funds moved, a rebuild is safe.
        BundleStatus::Failed => DispatchVerdict::Dead(DeadReason::NeverForwarded),
        BundleStatus::Pending => DispatchVerdict::Unknown("engine still pending"),
        BundleStatus::Invalid | BundleStatus::Unknown => match expiry {
            // Unseen and the blockhash is dead: can never land now.
            UnseenExpiry::Expired => DispatchVerdict::Dead(DeadReason::Expired),
            // Unseen but still includable: keep tracking, never resend.
            UnseenExpiry::Live => DispatchVerdict::Unknown("unseen; blockhash still valid"),
            // Unseen with no expiry evidence: same hold, honest reason
            // (pre-upgrade WAL lines have no blockhash to check).
            UnseenExpiry::Unknown => {
                DispatchVerdict::Unknown("unseen; expiry unproved (no blockhash to check)")
            }
        },
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
    fn token_account_amount_reads_base_layout() {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&125u64.to_le_bytes());
        assert_eq!(token_account_amount(&data), Some(125));
        // Too short for the base layout: unreadable, never zero.
        assert_eq!(token_account_amount(&data[..64]), None);
    }

    #[test]
    fn status_classification_distinguishes_chain_states() {
        let make =
            |confirmations: Option<usize>, level: Option<TransactionConfirmationStatus>| {
                TransactionStatus {
                    slot: 1,
                    confirmations,
                    status: Ok(()),
                    err: None,
                    confirmation_status: level,
                }
            };
        assert_eq!(
            classify_status(&make(Some(0), Some(TransactionConfirmationStatus::Processed))),
            ConfirmOutcome::Processed
        );
        assert_eq!(
            classify_status(&make(Some(5), Some(TransactionConfirmationStatus::Confirmed))),
            ConfirmOutcome::Confirmed
        );
        assert_eq!(
            classify_status(&make(None, Some(TransactionConfirmationStatus::Finalized))),
            ConfirmOutcome::Finalized
        );
        // Legacy nodes predate `confirmationStatus`: counts decide.
        assert_eq!(
            classify_status(&make(Some(0), None)),
            ConfirmOutcome::Processed
        );
        assert_eq!(classify_status(&make(Some(1), None)), ConfirmOutcome::Confirmed);
        assert_eq!(classify_status(&make(None, None)), ConfirmOutcome::Finalized);
        // An on-chain error outranks any vote state.
        let mut failed = make(None, Some(TransactionConfirmationStatus::Finalized));
        failed.err = Some(solana_transaction_error::TransactionError::AccountNotFound);
        assert_eq!(classify_status(&failed), ConfirmOutcome::FailedOnChain);
    }

    #[test]
    fn chain_verdicts_open_only_on_voted_states() {
        assert_eq!(
            verdict_from_chain(ConfirmOutcome::Confirmed),
            DispatchVerdict::Landed
        );
        assert_eq!(
            verdict_from_chain(ConfirmOutcome::Finalized),
            DispatchVerdict::Landed
        );
        assert_eq!(
            verdict_from_chain(ConfirmOutcome::FailedOnChain),
            DispatchVerdict::Dead(DeadReason::FailedOnChain)
        );
        // Processed and timeout are unknowns: never a fill, never a resend.
        assert!(matches!(
            verdict_from_chain(ConfirmOutcome::Processed),
            DispatchVerdict::Unknown(_)
        ));
        assert!(matches!(
            verdict_from_chain(ConfirmOutcome::Timeout),
            DispatchVerdict::Unknown(_)
        ));
    }

    #[test]
    fn timeout_is_never_a_failure() {
        // Engine "forgot" (5-minute lookback) + unseen + live blockhash:
        // keep tracking, never declare dead.
        assert_eq!(
            timeout_verdict(BundleStatus::Invalid, None, UnseenExpiry::Live),
            DispatchVerdict::Unknown("unseen; blockhash still valid")
        );
        // Status unreadable + unseen: unknown too.
        assert_eq!(
            timeout_verdict(BundleStatus::Unknown, None, UnseenExpiry::Live),
            DispatchVerdict::Unknown("unseen; blockhash still valid")
        );
        // No blockhash recorded (pre-upgrade line): unproved, never dead.
        assert_eq!(
            timeout_verdict(BundleStatus::Invalid, None, UnseenExpiry::Unknown),
            DispatchVerdict::Unknown("unseen; expiry unproved (no blockhash to check)")
        );
        // Unseen and the blockhash is dead: the only proof of expiry.
        assert_eq!(
            timeout_verdict(BundleStatus::Invalid, None, UnseenExpiry::Expired),
            DispatchVerdict::Dead(DeadReason::Expired)
        );
        // Engine forgot, but the signature is on-chain: chain beats engine.
        assert_eq!(
            timeout_verdict(
                BundleStatus::Invalid,
                Some(ConfirmOutcome::Confirmed),
                UnseenExpiry::Unknown
            ),
            DispatchVerdict::Landed
        );
        assert_eq!(
            timeout_verdict(
                BundleStatus::Invalid,
                Some(ConfirmOutcome::Processed),
                UnseenExpiry::Unknown
            ),
            DispatchVerdict::Unknown("seen processed; not voted yet")
        );
        // Engine never forwarded: provably dead, no funds moved.
        assert_eq!(
            timeout_verdict(BundleStatus::Failed, None, UnseenExpiry::Unknown),
            DispatchVerdict::Dead(DeadReason::NeverForwarded)
        );
        // ...but even that verdict yields to the chain: a reconciled
        // landing is the deciding word.
        assert_eq!(
            timeout_verdict(
                BundleStatus::Failed,
                Some(ConfirmOutcome::Confirmed),
                UnseenExpiry::Unknown
            ),
            DispatchVerdict::Landed
        );
        // Engine pending holds; engine landed wins.
        assert_eq!(
            timeout_verdict(BundleStatus::Pending, None, UnseenExpiry::Unknown),
            DispatchVerdict::Unknown("engine still pending")
        );
        assert_eq!(
            timeout_verdict(BundleStatus::Landed, None, UnseenExpiry::Unknown),
            DispatchVerdict::Landed
        );
    }

    #[test]
    fn buy_fill_math_nets_known_overhead() {
        // Hand-computed: spent 10M, 2000 tokens (6dp), 1 created ATA,
        // tip 100k, priority 200. Overhead = 100_000 + 200 + 5_000 +
        // 2_039_280 = 2_144_480; swap = 7_855_520.
        assert_eq!(
            buy_fill_from_deltas(10_000_000, 2_000_000_000, 1, 100_000, 200),
            Some(ObservedBuyFill {
                tokens_base_units: 2_000_000_000,
                swap_sol_lamports: 7_855_520,
            })
        );
        // No created ATA: no rent leg.
        assert_eq!(
            buy_fill_from_deltas(1_000_000, 500_000_000, 0, 100_000, 200)
                .expect("fill")
                .swap_sol_lamports,
            1_000_000 - 100_000 - 200 - 5_000
        );
    }

    #[test]
    fn buy_fill_rejects_degenerate() {
        // Zero tokens: nothing to price.
        assert_eq!(buy_fill_from_deltas(10_000_000, 0, 1, 100_000, 200), None);
        // Exact overhead: zero swap leg (would divide by zero downstream).
        assert_eq!(buy_fill_from_deltas(2_144_480, 1_000, 1, 100_000, 200), None);
        // Underfunded: spent below overhead.
        assert_eq!(buy_fill_from_deltas(100_000, 1_000, 1, 100_000, 200), None);
        // Overflow: absurd created count.
        assert_eq!(
            buy_fill_from_deltas(u64::MAX, 1_000, u64::MAX, 100_000, 200),
            None
        );
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
        // Default on protects live funds; paper opts out explicitly.
        assert!(sim_transport_outcome(false, "timeout").is_ok());
        // Strict: transport/timeout failures refuse to dispatch unproven.
        for cause in ["timeout", "RPC failure"] {
            match sim_transport_outcome(true, cause) {
                Err(ExecutorError::SimFailed(msg)) => assert!(msg.contains(cause), "{msg}"),
                other => panic!("expected SimFailed, got {other:?}"),
            }
        }
        // Builder plumbing enables the policy (default strict, paper opts out).
        assert!(executor().strict_sim);
        assert!(!executor().with_strict_sim(false).strict_sim);
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
        data[..8].copy_from_slice(&ids::BONDING_CURVE_DISCRIMINATOR);
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
        // Pump-owned non-curve account (bad discriminator) refuses
        // instead of quoting garbage.
        let mut impostor = data.clone();
        impostor[0] ^= 0xff;
        assert!(parse_curve_state(&pump, &impostor).is_none());
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
