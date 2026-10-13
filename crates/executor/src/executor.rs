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
    /// Slippage guard in basis points for the fee-inclusive guard
    /// recompute (see `quote_buy_guard`): the engine quotes the amount
    /// with protocol+creator fees, then ceilings + this margin.
    pub slippage_bps: u64,
    /// Watchlist-gate reference hint: signature of the first successful
    /// buy observed during buyer-quorum waiting. When set, the buy
    /// routing resolver fetches this transaction first (one RPC) before
    /// falling back to the bounded curve scan, testing it against BOTH
    /// shapes (18-account mayhem and 27-account `buy_v2`). `None` keeps
    /// scan-only behavior. The hint must name a successful same-curve
    /// buy — gate observations only record successful transactions, so
    /// this is guaranteed by construction; the resolver still verifies
    /// curve+mint before lifting any routing.
    pub mayhem_ref_sig: Option<String>,
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
    /// A known creator means a `create_v2` launch, whose curve takes the
    /// 26-account `sell_v2` shape (see `preflight_sell_v2`); `None` keeps
    /// the legacy core.
    pub creator: Option<Pubkey>,
}

/// Opaque v2-sell routing lifted from a same-curve reference sell:
/// fee recipient [6], quote mint [2], pfee vault [8] + quote account
/// [9]. Per-curve stable (like buyback routing); never derived, never
/// guessed — absence fails the sell closed. Position [18] needs no
/// reference: it is the derived sharing-config PDA (proven bump 255 on
/// both frozen samples).
///
/// The fee comes from the reference for the same era-consistency
/// reason as mayhem buys (see [`MayhemReference`]): live Global
/// rotates under running curves, and a mixed-era set fails
/// authorization while the reference set simulates clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SellV2Reference {
    /// Reference transaction signature (base58, for logs).
    pub signature: String,
    /// Fee recipient (position 6).
    pub fee_recipient: Pubkey,
    /// Quote mint (position 2).
    pub quote_mint: Pubkey,
    /// pfee quote vault (position 8).
    pub pfee_vault: Pubkey,
    /// pfee vault's quote account (position 9).
    pub pfee_quote_account: Pubkey,
}

/// Signature-scan depth for reference resolution (bounded RPC budget).
pub const REFERENCE_SCAN_LIMIT: usize = 20;

/// Pick the first top-level `sell_v2` on OUR curve+mint from a raw
/// `getTransaction` Json envelope and lift its opaque routing. Pure;
/// `None` on any shape mismatch (wrong curve/mint, short accounts,
/// undecodable data) — callers keep scanning, never misattribute.
/// Account keys resolve static-first then address-table order
/// (writable, readonly), mirroring the canonical resolvers.
pub fn select_sell_v2_reference(
    tx: &serde_json::Value,
    curve: &Pubkey,
    mint: &Pubkey,
) -> Option<SellV2Reference> {
    let msg = tx.get("transaction")?.get("message")?;
    let mut keys: Vec<String> = msg
        .get("accountKeys")?
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    if let Some(loaded) = tx
        .get("meta")
        .and_then(|m| m.get("loadedAddresses"))
        .and_then(|l| l.as_object())
    {
        for table in ["writable", "readonly"] {
            if let Some(addrs) = loaded.get(table).and_then(|v| v.as_array()) {
                keys.extend(addrs.iter().filter_map(|k| k.as_str().map(str::to_string)));
            }
        }
    }
    let pump = crate::instructions::PUMP_PROGRAM_ID.to_string();
    let pump_idx = keys.iter().position(|k| k == &pump)? as u64;
    let ixs = msg.get("instructions")?.as_array()?;
    let sig = tx
        .get("transaction")?
        .get("signatures")?
        .as_array()?
        .first()?
        .as_str()?
        .to_string();
    for ix in ixs {
        let Some(pi) = ix.get("programIdIndex").and_then(|v| v.as_u64()) else {
            continue;
        };
        if pi != pump_idx {
            continue;
        }
        let Some(data_str) = ix.get("data").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(raw) = bs58::decode(data_str).into_vec() else {
            continue;
        };
        if !raw.starts_with(&crate::instructions::PUMP_SELL_V2_DISCRIMINATOR) || raw.len() != 24 {
            continue;
        }
        let Some(arr) = ix.get("accounts").and_then(|v| v.as_array()) else {
            continue;
        };
        let idx: Vec<usize> = arr
            .iter()
            .filter_map(|a| a.as_u64().map(|i| i as usize))
            .collect();
        if idx.len() != 26 {
            continue;
        }
        let at = |pos: usize| -> Option<Pubkey> {
            keys.get(*idx.get(pos)?)
                .and_then(|k| Pubkey::from_str(k).ok())
        };
        // Must be OUR curve+mint: a shared-tx stranger sell must never
        // supply routing for another market.
        if at(10).as_ref() != Some(curve) || at(1).as_ref() != Some(mint) {
            continue;
        }
        return Some(SellV2Reference {
            signature: sig,
            fee_recipient: at(6)?,
            quote_mint: at(2)?,
            pfee_vault: at(8)?,
            pfee_quote_account: at(9)?,
        });
    }
    None
}

/// Opaque v2-buy routing lifted from a same-curve reference `buy_v2`:
/// fee recipient [6], quote mint [2], pfee vault [8] + quote account
/// [9]. Per-curve stable (like mayhem-buyback routing); never derived,
/// never guessed — absence fails the buy closed (see [`BuyRouting`]).
/// Position [18] needs no reference: it is the derived sharing-config
/// PDA (proven bump 255 on both frozen samples).
///
/// The fee comes from the reference for the same era-consistency reason
/// as mayhem buys and `sell_v2`: fee sets rotate globally while curves
/// keep working, and a mixed-era set fails authorization while the
/// reference set simulates clean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuyV2Reference {
    /// Reference transaction signature (base58, for logs).
    pub signature: String,
    /// Fee recipient (position 6).
    pub fee_recipient: Pubkey,
    /// Quote mint (position 2).
    pub quote_mint: Pubkey,
    /// pfee quote vault (position 8).
    pub pfee_vault: Pubkey,
    /// pfee vault's quote account (position 9).
    pub pfee_quote_account: Pubkey,
}

/// Pick the first top-level 27-account `buy_v2` on OUR curve+mint from a
/// raw `getTransaction` Json envelope and lift its opaque routing. Pure;
/// `None` on any shape mismatch (wrong curve/mint, short accounts,
/// undecodable data) — callers keep scanning, never misattribute.
/// Account keys resolve static-first then address-table order
/// (writable, readonly), mirroring the canonical resolvers.
pub fn select_buy_v2_reference(
    tx: &serde_json::Value,
    curve: &Pubkey,
    mint: &Pubkey,
) -> Option<BuyV2Reference> {
    let msg = tx.get("transaction")?.get("message")?;
    let mut keys: Vec<String> = msg
        .get("accountKeys")?
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    if let Some(loaded) = tx
        .get("meta")
        .and_then(|m| m.get("loadedAddresses"))
        .and_then(|l| l.as_object())
    {
        for table in ["writable", "readonly"] {
            if let Some(addrs) = loaded.get(table).and_then(|v| v.as_array()) {
                keys.extend(addrs.iter().filter_map(|k| k.as_str().map(str::to_string)));
            }
        }
    }
    let pump = crate::instructions::PUMP_PROGRAM_ID.to_string();
    let pump_idx = keys.iter().position(|k| k == &pump)? as u64;
    let ixs = msg.get("instructions")?.as_array()?;
    let sig = tx
        .get("transaction")?
        .get("signatures")?
        .as_array()?
        .first()?
        .as_str()?
        .to_string();
    for ix in ixs {
        let Some(pi) = ix.get("programIdIndex").and_then(|v| v.as_u64()) else {
            continue;
        };
        if pi != pump_idx {
            continue;
        }
        let Some(data_str) = ix.get("data").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(raw) = bs58::decode(data_str).into_vec() else {
            continue;
        };
        if !raw.starts_with(&crate::instructions::PUMP_BUY_V2_DISCRIMINATOR) || raw.len() != 24
        {
            continue;
        }
        let Some(arr) = ix.get("accounts").and_then(|v| v.as_array()) else {
            continue;
        };
        let idx: Vec<usize> = arr
            .iter()
            .filter_map(|a| a.as_u64().map(|i| i as usize))
            .collect();
        if idx.len() != 27 {
            continue;
        }
        let at = |pos: usize| -> Option<Pubkey> {
            keys.get(*idx.get(pos)?)
                .and_then(|k| Pubkey::from_str(k).ok())
        };
        // Must be OUR curve+mint: a shared-tx stranger buy must never
        // supply routing for another market.
        if at(10).as_ref() != Some(curve) || at(1).as_ref() != Some(mint) {
            continue;
        }
        return Some(BuyV2Reference {
            signature: sig,
            fee_recipient: at(6)?,
            quote_mint: at(2)?,
            pfee_vault: at(8)?,
            pfee_quote_account: at(9)?,
        });
    }
    None
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
    /// Cashback coin (v3 refused on-chain with 6094 — must use v2).
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

/// Resolved buy routing for a creator curve: the curve's own successful
/// buys decide the shape — an 18-account mayhem reference builds the
/// 18-account buy, a 27-account `buy_v2` reference builds `buy_v2`.
/// Never configuration, never guessing: without either reference the buy
/// fails closed (a creator curve rejects every static shape — 12-account
/// with 6062, 18-account where `buy_v2` is enforced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuyRouting {
    /// 18-account legacy mayhem buy routing ([1]/[16]/[17]).
    Mayhem(MayhemReference),
    /// 27-account `buy_v2` opaque routing.
    V2(BuyV2Reference),
}

/// Opaque mayhem-buy routing lifted from a same-curve reference buy:
/// fee recipient [1] plus unknown [16] + pfee vault [17]. Same-curve
/// buys share all three (two distinct buyers, one slot, identical
/// triple on-chain) — never derived, never guessed; absence falls back
/// to the 16-account shape with preflight simulation deciding.
///
/// The fee comes from the reference — NOT live Global. Fee Recipients
/// rotate globally while curves keep working: a same-tx-consistent set
/// simulates clean where a live-Global mix fails `NotAuthorized`
/// (fee_recipient.rs:19), proven by the 18-shape sim probe both ways.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MayhemReference {
    /// Reference transaction signature (base58, for logs).
    pub signature: String,
    /// Fee recipient (position 1).
    pub fee_recipient: Pubkey,
    /// Position 16 account.
    pub unknown_16: Pubkey,
    /// Position 17 pfee buyback vault.
    pub pfee_vault: Pubkey,
}

/// Pick the first top-level 18-account legacy buy on OUR curve+mint
/// from a raw `getTransaction` Json envelope and lift its [16]/[17].
/// Pure; `None` on any shape mismatch — callers keep scanning, never
/// misattribute. Only the 18-account legacy shape qualifies: `buy_v2`
/// positions are a different order and must not mix in.
pub fn select_mayhem_routing(
    tx: &serde_json::Value,
    curve: &Pubkey,
    mint: &Pubkey,
) -> Option<MayhemReference> {
    let msg = tx.get("transaction")?.get("message")?;
    let mut keys: Vec<String> = msg
        .get("accountKeys")?
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    if let Some(loaded) = tx
        .get("meta")
        .and_then(|m| m.get("loadedAddresses"))
        .and_then(|l| l.as_object())
    {
        for table in ["writable", "readonly"] {
            if let Some(addrs) = loaded.get(table).and_then(|v| v.as_array()) {
                keys.extend(addrs.iter().filter_map(|k| k.as_str().map(str::to_string)));
            }
        }
    }
    let pump = crate::instructions::PUMP_PROGRAM_ID.to_string();
    let pump_idx = keys.iter().position(|k| k == &pump)? as u64;
    let ixs = msg.get("instructions")?.as_array()?;
    let sig = tx
        .get("transaction")?
        .get("signatures")?
        .as_array()?
        .first()?
        .as_str()?
        .to_string();
    for ix in ixs {
        let Some(pi) = ix.get("programIdIndex").and_then(|v| v.as_u64()) else {
            continue;
        };
        if pi != pump_idx {
            continue;
        }
        let Some(data_str) = ix.get("data").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(raw) = bs58::decode(data_str).into_vec() else {
            continue;
        };
        if !raw.starts_with(&crate::instructions::PUMP_BUY_DISCRIMINATOR) || raw.len() != 25 {
            continue;
        }
        let Some(arr) = ix.get("accounts").and_then(|v| v.as_array()) else {
            continue;
        };
        let idx: Vec<usize> = arr
            .iter()
            .filter_map(|a| a.as_u64().map(|i| i as usize))
            .collect();
        if idx.len() != 18 {
            continue;
        }
        let at = |pos: usize| -> Option<Pubkey> {
            keys.get(*idx.get(pos)?)
                .and_then(|k| Pubkey::from_str(k).ok())
        };
        // Must be OUR curve+mint: a shared-tx stranger buy must never
        // supply routing for another market.
        if at(3).as_ref() != Some(curve) || at(2).as_ref() != Some(mint) {
            continue;
        }
        return Some(MayhemReference {
            signature: sig,
            fee_recipient: at(1)?,
            unknown_16: at(16)?,
            pfee_vault: at(17)?,
        });
    }
    None
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
    /// Strict preflight: transport/timeout simulation failures reject
    /// the dispatch instead of proceeding with a warning. Default off
    /// (today's behavior); enable for live funds via `SIM_STRICT=true`.
    strict_sim: bool,
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
/// failure (transport, short data, wrong owner): callers fall back to the canonical
/// constant. Deadline-bound.
pub async fn fetch_global_fee_recipient(rpc: &RpcClient) -> Option<Pubkey> {
    let account = match tokio::time::timeout(
        Duration::from_secs(10),
        rpc.get_account(&crate::instructions::GLOBAL_ACCOUNT),
    )
    .await
    {
        Ok(Ok(account)) => account,
        _ => return None,
    };
    if account.owner != crate::instructions::PUMP_PROGRAM_ID {
        return None;
    }
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
            strict_sim: false,
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
    /// never attach it). Keeps plain-path solvency from demanding funds
    /// the transaction will never spend.
    fn effective_tip(&self, tip_lamports: u64) -> u64 {
        if self.jito.is_some() { tip_lamports } else { 0 }
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
        // Route-aware solvency is decided below (after the guard
        // recompute and routing): native-paying shapes (classic,
        // mayhem-18) assert the full guard natively, while WSOL-quoted
        // v2/v3 shapes fund the trade from the WSOL ATA (asserting the
        // guard natively there would demand every lamport twice).
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
        // Program-bound ATA: Token-2022 mints derive a DIFFERENT address
        // than the classic helper (verified live).
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        let mut ixs = self
            .maybe_create_ata_ix(&payer, &payer, &order.mint, &ata, &token_program)
            .await;
        // Fee-inclusive guard recompute (6002 class): fresh
        // fee-inclusive quote + slippage ceiling replaces the
        // notional-derived guard. Falls back with a loud log when
        // unreadable; simulation refuses a truly short guard either
        // way. Native solvency below is asserted against this raised
        // guard (not the sized value), so no second re-check is needed.
        let (final_max, quoted_cost) = self
            .quote_buy_guard(
                &order.mint,
                curve_state.as_ref(),
                order.amount_tokens,
                order.max_sol_cost,
                order.slippage_bps,
            )
            .await;
        // Route-aware funding: classic (no creator) pays native, so the
        // full guard must sit natively; creator shapes only need tip +
        // reserve natively here (their trade funds are asserted per-arm:
        // native for mayhem-18, WSOL ATA for v2/v3 SOL quotes).
        let native_trade = if order.creator.is_none() { final_max } else { 0 };
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, native_trade, self.effective_tip(order.tip_lamports))
            .await?;
        info!(mint = %order.mint, balance, max_cost = final_max, native_trade, "buy pre-flight ok");
        // Creator curves never take a static shape. Routing order:
        // (1) curve state (when readable): complete → refuse (buys
        // belong to the pool/migration now); cashback → v2 reference
        // path (v3 refused on-chain with 6094); otherwise the v3 fast
        // path — derived addresses plus the listed buyback, no
        // reference scan at all (mayhem supported);
        // (2) v2 reference path: the curve's own successful buys decide
        // between the 18-account mayhem buy and the 27-account `buy_v2`
        // (see [`BuyRouting`]) — preferably the watchlist gate's first
        // successful buy (`mayhem_ref_sig`), falling back to a bounded
        // curve scan. The reference supplies the era-consistent fee plus
        // the opaque routing — live Global is wrong here (fee sets
        // rotate globally while curves keep working; proven both ways by
        // the 18-shape sim). No reference means no dispatch
        // (fail-closed).
        if let Some(creator) = order.creator.as_ref() {
            // Creator cross-check against chain bytes when available:
            // the launch-time parse is single-sample provenance — never
            // trust it over curve state (a mismatch fails closed).
            if let Some(state) = curve_state.as_ref() {
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
                if !state.is_cashback {
                    info!(mint = %order.mint, mayhem = state.is_mayhem,
                        "v3 buy via derived addresses + listed buyback (no reference scan)");
                    // No 18-account mayhem fallback when v3 sims red: the
                    // sim gate runs later (send path) on already-built ixs,
                    // so a fallback would need alternate ixs plumbed
                    // through dispatch plus frozen proof it helps — until
                    // then, unroutable curves fail closed here and surface
                    // as diverged/manual upstream for human review.
                    return self
                        .preflight_buy_v3(order, &payer, &token_program, ixs, state, final_max, quoted_cost)
                        .await;
                }
                // Cashback: v3 refused on-chain — v2 reference path below.
            } else {
                warn!(mint = %order.mint,
                    "curve state unreadable; v2 reference path without v3/cashback routing");
            }
            return match self
                .resolve_buy_routing(
                    &order.bonding_curve,
                    &order.mint,
                    order.mayhem_ref_sig.as_deref(),
                )
                .await
            {
                Ok(BuyRouting::Mayhem(reference)) => {
                    info!(mint = %order.mint, ref_sig = %reference.signature,
                        "mayhem buy routed via same-curve reference ([1]/[16]/[17] copied)");
                    // Mayhem-18 pays native (no WSOL debit): the full
                    // raised guard must sit natively (tip+reserve was
                    // covered above; this covers the trade itself).
                    self.wallet
                        .assert_solvency(&self.rpc, final_max, self.effective_tip(order.tip_lamports))
                        .await?;
                    let routing = crate::instructions::MayhemRouting {
                        unknown_16: &reference.unknown_16,
                        pfee_vault: &reference.pfee_vault,
                    };
                    ixs.push(PumpSwapBuilder::build_swap_ix(
                        SwapKind::Buy { amount: order.amount_tokens, max_sol_cost: final_max, track_volume: true },
                        &crate::instructions::SwapAccounts {
                            payer: &payer,
                            mint: &order.mint,
                            bonding_curve: &order.bonding_curve,
                            associated_bonding_curve: &order.associated_bonding_curve,
                            user_ata: &ata,
                            fee_recipient: &reference.fee_recipient,
                            token_program: &token_program,
                            creator: Some(creator),
                            mayhem_routing: Some(routing),
                        },
                    ));
                    Ok(ixs)
                }
                Ok(BuyRouting::V2(reference)) => {
                    self.preflight_buy_v2(
                        order,
                        &payer,
                        &token_program,
                        ixs,
                        creator,
                        &reference,
                        final_max,
                    )
                    .await
                }
                Err(e) => {
                    warn!(mint = %order.mint, error = %e,
                        "no reference buy on creator curve; refusing unroutable dispatch (fail-closed)");
                    Err(e)
                }
            };
        }
        // Classic curve: the legacy 12-account core with the live fee.
        let fee_recipient = self.fee_recipient().await;
        ixs.push(PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount: order.amount_tokens, max_sol_cost: final_max, track_volume: true },
            &crate::instructions::SwapAccounts {
                payer: &payer,
                mint: &order.mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                user_ata: &ata,
                fee_recipient: &fee_recipient,
                token_program: &token_program,
                creator: None,
                mayhem_routing: None,
            },
        ));
        Ok(ixs)
    }

    /// v2 buy path: the verified 27-account `buy_v2` shape. Opaque
    /// routing (fee recipient, quote mint, pfee vault + account)
    /// comes from the resolved same-curve reference — per-curve stable
    /// like mayhem-buyback routing — and fails closed upstream when none
    /// resolves. The fee comes from the reference, NOT live Global (same
    /// era-consistency reason as mayhem buys and `sell_v2`). The spend
    /// guard is the order's own `max_sol_cost` (both frozen samples carry
    /// `u64::MAX` — an open ape — so the guard is always caller-computed,
    /// never a copied value).
    ///
    /// The base-ATA create prefix (if needed) is already in `ixs`; the
    /// quote-ATA create prefix is appended here, mirroring
    /// [`SwapExecutor::preflight_sell_v2`].
    /// (Arity documented, not shrunk: the seven inputs are the
    /// full v2-buy context — order, chain accounts, and resolved
    /// routing — and every caller passes all of them. Splitting the
    /// struct would scatter one shape's inputs across two types.)
    #[allow(clippy::too_many_arguments)]
    async fn preflight_buy_v2(
        &self,
        order: &BuyOrder,
        payer: &Pubkey,
        token_program: &Pubkey,
        mut ixs: Vec<solana_instruction::Instruction>,
        creator: &Pubkey,
        reference: &BuyV2Reference,
        max_sol_cost: u64,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        let Some(quote_program) = self.token_program(&reference.quote_mint).await else {
            return Err(ExecutorError::BadOrder(
                "quote mint account unreadable; token program unknown".to_string(),
            ));
        };
        warn!(mint = %order.mint, curve = %order.bonding_curve, ref_sig = %reference.signature,
            quote = %reference.quote_mint, fee = %reference.fee_recipient,
            "v2 buy routed via same-curve reference (opaque routing copied, guard computed)");
        let quote_ata = user_ata_with_program(
            payer,
            &reference.quote_mint,
            &quote_program,
        );
        ixs.extend(
            self.maybe_create_ata_ix(payer, payer, &reference.quote_mint, &quote_ata, &quote_program)
                .await,
        );
        // SOL quotes pay in WSOL: same guard-balance rule as v3 — the
        // wallet must hold the guard in its WSOL ATA (proven live:
        // a SOL-only buyer fails with token InsufficientFunds after
        // burning rent/fee). Refuse with a wrap hint; token quotes
        // skip (sim decides).
        if reference.quote_mint == crate::instructions::WSOL_MINT {
            match self.quote_balance(&quote_ata).await {
                Some(held) if held < max_sol_cost => {
                    return Err(ExecutorError::BadOrder(format!(
                        "wsol {held} below guard {max_sol_cost}; wrap SOL into the wallet WSOL ATA first"
                    )));
                }
                Some(held) => {
                    info!(mint = %order.mint, held, cost = max_sol_cost, "wsol covers guard");
                }
                None => {
                    warn!(mint = %order.mint, "wsol balance unreadable; simulation decides");
                }
            }
        }
        ixs.push(PumpSwapBuilder::build_buy_v2_ix(
            order.amount_tokens,
            max_sol_cost,
            &crate::instructions::BuyV2Accounts {
                payer,
                mint: &order.mint,
                quote_mint: &reference.quote_mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                fee_recipient: &reference.fee_recipient,
                base_token_program: token_program,
                quote_token_program: &quote_program,
                creator,
                pfee_vault: &reference.pfee_vault,
                pfee_quote_account: &reference.pfee_quote_account,
            },
        ));
        Ok(ixs)
    }

    /// v3 buy path: the 17-account shape with fully derived addresses
    /// plus the listed buyback pick — no same-curve reference needed
    /// (fees stay on the curve). The spend guard is the order's own
    /// `max_sol_cost`, never a copied value. Pre-creates: the payer
    /// quote ATA ([10]) ALWAYS — the trade debits it via
    /// TransferChecked on every quote, including WSOL on SOL pairs
    /// (a SOL-only buyer fails the sim with token InsufficientFunds);
    /// the buyback ATA additionally on token-paired quotes (on SOL
    /// quotes the buyback is the wallet itself). The base-ATA prefix
    /// (if needed) is already in `ixs`.
    /// (Arity documented, not shrunk: the eight inputs are the full
    /// v3-buy context — order, chain accounts, guard pair, and routing
    /// state — and the sole caller passes all of them. Splitting the
    /// struct would scatter one shape's inputs across two types.)
    #[allow(clippy::too_many_arguments)]
    async fn preflight_buy_v3(
        &self,
        order: &BuyOrder,
        payer: &Pubkey,
        token_program: &Pubkey,
        mut ixs: Vec<solana_instruction::Instruction>,
        state: &CurveState,
        max_sol_cost: u64,
        _quoted_cost: Option<u64>,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        let quote_mint = state.quote_mint;
        let Some(quote_program) = self.token_program(&quote_mint).await else {
            return Err(ExecutorError::BadOrder(
                "quote mint account unreadable; token program unknown".to_string(),
            ));
        };
        let buyback_wallet = crate::instructions::DEFAULT_BUYBACK_FEE_RECIPIENT;
        let buyback = buyback_account(&buyback_wallet, &quote_mint, &quote_program);
        // Payer quote ATA ([10]) must exist on EVERY quote: the trade
        // debits it via TransferChecked, and the program never creates
        // accounts (proven live 2026-10-14: a funded-SOL but WSOL-empty
        // buyer failed the sim with token InsufficientFunds). The
        // idempotent prefix makes this safe on all paths.
        let user_quote =
            user_ata_with_program(payer, &quote_mint, &quote_program);
        ixs.extend(
            self.maybe_create_ata_ix(payer, payer, &quote_mint, &user_quote, &quote_program)
                .await,
        );
        // SOL quotes pay in WSOL: the wallet must hold the quoted guard
        // in its WSOL ATA (native SOL alone cannot fill). Compare against
        // the ceiling (max_sol_cost), not the bare quote — a guard above
        // cost still fails without matching WSOL. Refuse with a
        // wrap hint instead of burning fees on a doomed sim; transport
        // trouble proceeds (sim decides), token quotes skip (sim
        // decides — their economics are outside the quote engine).
        if quote_mint == crate::instructions::WSOL_MINT {
            match self.quote_balance(&user_quote).await {
                Some(held) if held < max_sol_cost => {
                    return Err(ExecutorError::BadOrder(format!(
                        "wsol {held} below guard {max_sol_cost}; wrap SOL into the wallet WSOL ATA first"
                    )));
                }
                Some(held) => {
                    info!(mint = %order.mint, held, cost = max_sol_cost, "wsol covers guard");
                }
                None => {
                    warn!(mint = %order.mint, "wsol balance unreadable; simulation decides");
                }
            }
        } else {
            // Anyone may fund the rent (official docs); the recipient owns it.
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
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("sell amount_tokens is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, self.effective_tip(order.tip_lamports))
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
        // Creator curves never take the legacy core. Routing order
        // (mirrors buys): complete → refuse without burning fees (the
        // exit belongs to migration/manual now); cashback → v2 reference
        // path (v3 refused on-chain with 6094); otherwise the v3 fast
        // path (derived addresses + listed buyback, no scan).
        if let Some(creator) = order.creator.as_ref() {
            if let Some(state) = self.read_curve_state(&order.bonding_curve).await {
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
                if !state.is_cashback {
                    info!(mint = %order.mint, mayhem = state.is_mayhem,
                        "v3 sell via derived addresses + listed buyback (no reference scan)");
                    return self
                        .preflight_sell_v3(order, &payer, &token_program, &state)
                        .await;
                }
                // Cashback: v3 refused on-chain — v2 reference path below.
            } else {
                warn!(mint = %order.mint,
                    "curve state unreadable; v2 reference path without v3/cashback routing");
            }
            return self
                .preflight_sell_v2(order, &payer, &token_program, &ata, creator)
                .await;
        }
        // Classic curve: the legacy core with the live fee.
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
                // Always None here: creator-carrying orders return via
                // the v2 path above (legacy sells die on v2 curves).
                creator: None,
                mayhem_routing: None,
            },
        )])
    }

    /// v2 sell path: the verified 26-account `sell_v2` shape. Opaque
    /// routing (fee recipient, quote mint, pfee vault + account)
    /// resolves from a recent same-curve reference sell — per-curve
    /// stable like buyback routing — and fails closed when none scans.
    /// The fee comes from the reference, NOT live Global: fee recipients
    /// rotate globally while curves keep working, and a mixed-era set
    /// fails authorization (proven both ways by the 18-shape sim). The
    /// SOL floor is the order's own `min_sol_output` (two-sample
    /// confirmed semantics), never a copied value.
    ///
    /// Young-curve edge: a curve with zero prior sells has no reference
    /// yet — the sell fails closed here and the caller's retry path
    /// (cooldown + budget, then manual exit) covers the wait, exactly
    /// like any other unconfirmed live sell.
    async fn preflight_sell_v2(
        &self,
        order: &SellOrder,
        payer: &Pubkey,
        token_program: &Pubkey,
        base_ata: &Pubkey,
        creator: &Pubkey,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        let reference = self
            .resolve_sell_v2_reference(&order.bonding_curve, &order.mint)
            .await?;
        let Some(quote_program) = self.token_program(&reference.quote_mint).await else {
            return Err(ExecutorError::BadOrder(
                "quote mint account unreadable; token program unknown".to_string(),
            ));
        };
        warn!(mint = %order.mint, curve = %order.bonding_curve, ref_sig = %reference.signature,
            quote = %reference.quote_mint, fee = %reference.fee_recipient, floor = order.min_sol_output,
            "v2 sell routed via same-curve reference (opaque routing copied, guard computed)");
        let quote_ata = user_ata_with_program(
            payer,
            &reference.quote_mint,
            &quote_program,
        );
        let mut ixs = self
            .maybe_create_ata_ix(payer, payer, &order.mint, base_ata, token_program)
            .await;
        ixs.extend(
            self.maybe_create_ata_ix(payer, payer, &reference.quote_mint, &quote_ata, &quote_program)
                .await,
        );
        ixs.push(PumpSwapBuilder::build_sell_v2_ix(
            order.amount_tokens,
            order.min_sol_output,
            &crate::instructions::SellV2Accounts {
                payer,
                mint: &order.mint,
                quote_mint: &reference.quote_mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                fee_recipient: &reference.fee_recipient,
                base_token_program: token_program,
                quote_token_program: &quote_program,
                creator,
                pfee_vault: &reference.pfee_vault,
                pfee_quote_account: &reference.pfee_quote_account,
            },
        ));
        Ok(ixs)
    }

    /// Resolve buy routing for OUR curve+mint: the curve's own successful
    /// buys decide the shape (see [`BuyRouting`]). Tries the watchlist-gate
    /// hint first (one `getTransaction` for the first successful buy
    /// observed while waiting for buyer quorum — the hint names a
    /// successful buy by construction, and the curve+mint checks inside
    /// the selectors still guard against misattribution), then scans
    /// recent curve transactions once, testing every fetched transaction
    /// against BOTH shapes. Bounded (one signature page + per-tx
    /// deadline); fail-closed `BadOrder` when neither shape resolves — a
    /// creator curve rejects every static shape, so guessing burns fees.
    async fn resolve_buy_routing(
        &self,
        curve: &Pubkey,
        mint: &Pubkey,
        hint_sig: Option<&str>,
    ) -> Result<BuyRouting, ExecutorError> {
        // Gate hint first: one fetch, no signature-page scan. The hint is
        // a single transaction, so at most one shape matches — whichever
        // the quorum's buyers actually used.
        if let Some(sig) = hint_sig
            && let Some(tx) = self.fetch_tx_json(sig).await
        {
            if let Some(reference) = select_mayhem_routing(&tx, curve, mint) {
                return Ok(BuyRouting::Mayhem(reference));
            }
            if let Some(reference) = select_buy_v2_reference(&tx, curve, mint) {
                return Ok(BuyRouting::V2(reference));
            }
        }
        use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
        let sigs = match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_signatures_for_address_with_config(
                curve,
                GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(REFERENCE_SCAN_LIMIT),
                    // Same horizon as every other read: the default
                    // (`None`) would follow the RPC default instead of
                    // the configured gate commitment.
                    commitment: Some(self.rpc.commitment()),
                },
            ),
        )
        .await
        {
            Ok(Ok(sigs)) => sigs,
            Ok(Err(e)) => return Err(ExecutorError::Rpc(e)),
            Err(_) => return Err(ExecutorError::Timeout("reference signature scan")),
        };
        for status in sigs {
            if status.err.is_some() {
                continue;
            }
            let tx = match self.fetch_tx_json(&status.signature).await {
                Some(tx) => tx,
                None => continue,
            };
            if let Some(reference) = select_mayhem_routing(&tx, curve, mint) {
                return Ok(BuyRouting::Mayhem(reference));
            }
            if let Some(reference) = select_buy_v2_reference(&tx, curve, mint) {
                return Ok(BuyRouting::V2(reference));
            }
        }
        Err(ExecutorError::BadOrder(
            "no reference buy (18-account mayhem or buy_v2) on curve; buy unroutable".to_string(),
        ))
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

    /// Scan recent curve transactions for a top-level `sell_v2` on OUR
    /// curve+mint and lift its opaque routing (quote mint/fee, pfee
    /// vault + account). Bounded (signature page + per-tx
    /// deadline); fail-closed `BadOrder` when nothing resolves — a v2
    /// sell without a reference is unroutable, never guessed.
    async fn resolve_sell_v2_reference(
        &self,
        curve: &Pubkey,
        mint: &Pubkey,
    ) -> Result<SellV2Reference, ExecutorError> {
        use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
        let sigs = match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_signatures_for_address_with_config(
                curve,
                GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(REFERENCE_SCAN_LIMIT),
                    // Same horizon as every other read: the default
                    // (`None`) would follow the RPC default instead of
                    // the configured gate commitment.
                    commitment: Some(self.rpc.commitment()),
                },
            ),
        )
        .await
        {
            Ok(Ok(sigs)) => sigs,
            Ok(Err(e)) => return Err(ExecutorError::Rpc(e)),
            Err(_) => return Err(ExecutorError::Timeout("reference signature scan")),
        };
        for status in sigs {
            if status.err.is_some() {
                continue;
            }
            let tx = match self.fetch_tx_json(&status.signature).await {
                Some(tx) => tx,
                None => continue,
            };
            if let Some(reference) = select_sell_v2_reference(&tx, curve, mint) {
                return Ok(reference);
            }
        }
        Err(ExecutorError::BadOrder(
            "no reference sell_v2 on curve; v2 sell unroutable".to_string(),
        ))
    }

    /// One raw `getTransaction` (Json + v1 envelopes, like every other
    /// reader in the tree). `None` on any transport/timeout/shape
    /// failure — callers skip loudly, never misparse.
    async fn fetch_tx_json(&self, signature: &str) -> Option<serde_json::Value> {
        use solana_client::rpc_request::RpcRequest;
        let params = serde_json::json!([
            signature,
            {"encoding": "json", "maxSupportedTransactionVersion": 1},
        ]);
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.rpc
                .send::<serde_json::Value>(RpcRequest::GetTransaction, params),
        )
        .await
        {
            Ok(Ok(tx)) => Some(tx),
            _ => None,
        }
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
    fn select_mayhem_routing_lifts_16_17() {
        // Frozen mayhem buy (18 accounts): opaque tail ([16] unknown,
        // [17] pfee vault) extracts against OUR curve+mint; strangers
        // and shapes mismatch.
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/mayhem_buy_mainnet.json"
        ))
        .expect("fixture");
        // shape: the mayhem fixture is a bare getTransaction result
        // (no RPC envelope), which is exactly what select expects.
        let tx = &raw;
        let curve =
            Pubkey::from_str("BAwdshXChLSK8SEyz5AYh7qEcryohNwREU1GdrSPJzic").expect("curve");
        let mint =
            Pubkey::from_str("8xGpPeXp3B6TqYhBW6BN95CCdfJgcNPDCvFirC9Spump").expect("mint");
        let reference = select_mayhem_routing(tx, &curve, &mint).expect("reference");
        assert!(!reference.signature.is_empty());
        assert_eq!(
            reference.fee_recipient.to_string(),
            "GesfTA3X2arioaHp8bbKdjG9vJtskViWACZoYvxp4twS"
        );
        assert_eq!(
            reference.unknown_16.to_string(),
            "BvCA7SSDEjm59ZzSiyfhztNpTW7hQx8YTFQVdYcQkENh"
        );
        assert_eq!(
            reference.pfee_vault.to_string(),
            "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"
        );
        let stranger = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(select_mayhem_routing(tx, &stranger, &mint).is_none());
        assert!(select_mayhem_routing(tx, &curve, &stranger).is_none());
        assert!(select_mayhem_routing(&serde_json::Value::Null, &curve, &mint).is_none());
    }

    #[test]
    fn select_mayhem_routing_lifts_16_17_from_successful_buy() {
        // #2 proof: the success fixture is a mainnet buy that LANDED
        // (`meta.err` null — asserted below), found by a finalized-block
        // scan (42 blocks, 53k txs, 3 hits). Its [16]/[17] triple
        // extracts against OUR curve+mint: [16] is per-curve unique and
        // [17] belongs to the rotating pfee-vault set (this vault differs
        // from the frozen failed sample's — era rotation is why routing
        // is copied per curve, never hardcoded).
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/mayhem_buy_success_mainnet.json"
        ))
        .expect("fixture");
        assert!(
            raw.get("meta").and_then(|m| m.get("err")).is_some_and(|e| e.is_null()),
            "success fixture must carry a landed transaction"
        );
        let tx = &raw;
        let curve =
            Pubkey::from_str("G5qeVtsGdBnDePYfVQX1eD1vyRfD19noUzPNMFQB9bqM").expect("curve");
        let mint =
            Pubkey::from_str("6F1gFjE56Q792Mu7jdwoxKpuyhNWUp241eTgWeHwe4u1").expect("mint");
        let reference = select_mayhem_routing(tx, &curve, &mint).expect("reference");
        assert!(!reference.signature.is_empty());
        assert_eq!(
            reference.fee_recipient.to_string(),
            "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"
        );
        assert_eq!(
            reference.unknown_16.to_string(),
            "4W3yTT5Zo7VkapvBYniUBAEVxErrwnXU85EcsPZMR8VQ"
        );
        assert_eq!(
            reference.pfee_vault.to_string(),
            "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"
        );
        let stranger = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(select_mayhem_routing(tx, &stranger, &mint).is_none());
        assert!(select_mayhem_routing(tx, &curve, &stranger).is_none());
    }

    #[test]
    fn select_buy_v2_reference_lifts_opaque_routing() {
        // Frozen `buy_v2` fills (27 accounts each): opaque routing ([2]
        // quote, [6] fee, [8]/[9] pfee) extracts against OUR
        // curve+mint; strangers and shapes mismatch. The two samples sit
        // on different curves with different vaults — per-curve copying,
        // never derivation.
        for (path, curve, mint, fee, quote, vault, vault_quote) in [
            (
                "../../listener/tests/fixtures/create_v2_mainnet.json",
                "BAwdshXChLSK8SEyz5AYh7qEcryohNwREU1GdrSPJzic",
                "8xGpPeXp3B6TqYhBW6BN95CCdfJgcNPDCvFirC9Spump",
                "8sNeir4QsLsJdYpc9RZacohhK1Y5FLU3nC5LXgYB4aa6",
                "So11111111111111111111111111111111111111112",
                "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD",
                "CASRL2zkwDnppxEFQ4LgdwgR9pdz5Q8R8nEMKVZ9QoLp",
            ),
            (
                "../../listener/tests/fixtures/buy_v2_second_mainnet.json",
                "8yZi6seSHJ2v5LShCWCGkw5i5JFZnrK5CBrpgG1LMHro",
                "FiiXewrZ8mgfwXsMhby1PEFXeffMhZwG1PAAWPujpump",
                "GesfTA3X2arioaHp8bbKdjG9vJtskViWACZoYvxp4twS",
                "So11111111111111111111111111111111111111112",
                "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
                "6rVkF4HSgy1jrnC3HogfRgPHrq4CtLg5f11URpsC4i9D",
            ),
        ] {
            let contents = if path.contains("second") {
                include_str!("../../listener/tests/fixtures/buy_v2_second_mainnet.json")
            } else {
                include_str!("../../listener/tests/fixtures/create_v2_mainnet.json")
            };
            let raw: serde_json::Value =
                serde_json::from_str(contents).expect("fixture");
            // Shape: full RPC envelope here (unlike the bare mayhem
            // fixtures), so select reads the `result` object.
            let tx = &raw["result"];
            let curve = Pubkey::from_str(curve).expect("curve");
            let mint = Pubkey::from_str(mint).expect("mint");
            let reference = select_buy_v2_reference(tx, &curve, &mint).expect("reference");
            assert!(!reference.signature.is_empty());
            assert_eq!(reference.fee_recipient.to_string(), fee);
            assert_eq!(reference.quote_mint.to_string(), quote);
            assert_eq!(reference.pfee_vault.to_string(), vault);
            assert_eq!(reference.pfee_quote_account.to_string(), vault_quote);
            // [18] is derived (sharing_config), not reference-copied —
            // asserted in the builder tests, not here.
            let stranger =
                Pubkey::from_str("11111111111111111111111111111111").expect("key");
            assert!(select_buy_v2_reference(tx, &stranger, &mint).is_none());
            assert!(select_buy_v2_reference(tx, &curve, &stranger).is_none());
        }
        // Garbage JSON: None, never a panic.
        let curve = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(
            select_buy_v2_reference(&serde_json::Value::Null, &curve, &curve).is_none()
        );
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
    fn select_sell_v2_reference_lifts_opaque_routing() {
        // Frozen sample: opaque routing ([2] quote, [8]/[9] pfee)
        // extracts against OUR curve+mint; strangers and shapes mismatch.
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/sell_v2_mainnet.json"
        ))
        .expect("fixture");
        let tx = &raw["result"];
        let curve =
            Pubkey::from_str("8rRGZTqPSSwbSsGPagUrYzJf1dHZ33QdxQkWEzpEnsKx").expect("curve");
        let mint = Pubkey::from_str("GmixVU6E1xeQWjEM8LAYsiFt9dJj27Ji4oDGKkmEpump").expect("mint");
        let reference = select_sell_v2_reference(tx, &curve, &mint).expect("reference");
        assert!(!reference.signature.is_empty());
        assert_eq!(
            reference.fee_recipient.to_string(),
            "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM"
        );
        assert_eq!(
            reference.quote_mint.to_string(),
            "RACEyWiM2ztEZcJx2AHXU2eWjhxU57x3vXn92b39dLD"
        );
        assert_eq!(
            reference.pfee_vault.to_string(),
            "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"
        );
        assert_eq!(
            reference.pfee_quote_account.to_string(),
            "6vquTehtsUfPNbYKQ6McujXLnye6dsvkhTnDtoN2rNbA"
        );
        // [18] is derived (sharing_config), not reference-copied —
        // asserted in the builder tests, not here.
        // Wrong curve or mint: never misattribute a stranger sell.
        let stranger = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(select_sell_v2_reference(tx, &stranger, &mint).is_none());
        assert!(select_sell_v2_reference(tx, &curve, &stranger).is_none());
        // Garbage JSON: None, never a panic.
        assert!(select_sell_v2_reference(&serde_json::Value::Null, &curve, &mint).is_none());
        assert!(
            select_sell_v2_reference(&serde_json::json!({"transaction": {}}), &curve, &mint)
                .is_none()
        );
    }

    #[test]
    fn mayhem_failed_sample_is_a_guard_failure_not_a_shape_failure() {
        // Negative fixture: this 18-account buy FAILED with 6002
        // (`TooMuchSolRequired`, slippage) — NOT a derivation error.
        // The guard it sent (payload max) is below what the program
        // required (log Right), so no PDA change could have saved it.
        // Locks the failure class: guard bugs get sizing fixes, shape
        // bugs get derivation fixes — never mixed.
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/mayhem_buy_mainnet.json"
        ))
        .expect("fixture");
        let err = raw["meta"]["err"].clone();
        assert_eq!(
            err,
            serde_json::json!({"InstructionError": [4, {"Custom": 6002}]}),
            "fixture must carry the 6002 guard failure"
        );
        let logs: Vec<String> =
            serde_json::from_value(raw["meta"]["logMessages"].clone()).expect("logs");
        let text = logs.join("\n");
        assert!(
            text.contains("TooMuchSolRequired"),
            "program must name the guard failure"
        );
        assert!(text.contains("Left: 1650001"), "log Left must match payload guard");
        assert!(text.contains("Right: 1668970"), "log Right must exceed it");
        // Payload guard equals log Left and falls short of Right.
        let msg = &raw["transaction"]["message"];
        let keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        let pump = crate::instructions::PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        let mut guard = None;
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) {
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().expect("data"))
                .into_vec()
                .expect("b58");
            if data.starts_with(&crate::instructions::PUMP_BUY_DISCRIMINATOR)
                && data.len() == 25
            {
                guard = Some(u64::from_le_bytes(data[16..24].try_into().expect("u64")));
                break;
            }
        }
        let guard = guard.expect("buy in fixture");
        assert_eq!(guard, 1_650_001, "payload guard must equal log Left");
        assert!(guard < 1_668_970, "guard shortfall is the whole failure");
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
