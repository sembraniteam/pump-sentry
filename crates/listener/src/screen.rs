//! Entry confirmation gate + developer screening (win-rate hardening).
//!
//! - [`extract_pump_swaps`]: classify Pump.fun buy/sell instructions in a
//!   fetched transaction into [`SwapRecord`]s.
//! - [`EntryGate`]: pure watchlist state machine — submit on signal,
//!   confirm on distinct-buyer quorum, abort on dev dump or timeout.
//! - [`screen_developer`]: RPC dev-wallet history (age + prior launches),
//!   fail-closed on transport errors.

use crate::parser::{PUMP_CREATE_DISCRIMINATOR, PUMP_PROGRAM_ID, decode_ix_data};
use migration::{MigrationDestination, MigrationMonitor, RAYDIUM_V4_PROGRAM_ID, RaydiumV4Pool};
use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient,
    rpc_client::GetConfirmedSignaturesForAddress2Config, rpc_config::RpcTransactionConfig,
};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, UiInstruction, UiMessage,
    UiRawMessage, UiTransactionEncoding, UiTransactionStatusMeta,
    option_serializer::OptionSerializer,
};
use std::{
    collections::HashMap,
    collections::HashSet,
    future::Future,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{debug, warn};
pub use ids::{PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_SELL_DISCRIMINATOR, SYSTEM_PROGRAM_ID};
/// Account index of the trader (`user`) in buy/sell instructions.
pub const IDX_SWAP_USER: usize = 6;
/// Account index of the trader in `buy_v2` instructions: the fee payer
/// (first signer) at position 13. Verified against a real mainnet
/// `buy_v2` (27 accounts; position 13 resolved to the fee-payer key).
pub const IDX_BUY_V2_USER: usize = 13;
/// Account index of the creator (`user`) in create instructions.
pub const IDX_CREATE_USER: usize = 7;

/// Screening failures: config, keys, and RPC transport.
#[derive(Error, Debug)]
pub enum ScreenError {
    /// RPC endpoint URL is invalid.
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    /// Address is not a valid pubkey: {0}
    #[error("invalid address: {0}")]
    BadAddress(String),
    /// RPC transport failure.
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
    /// RPC read exceeded its deadline (fail-closed upstream).
    #[error("rpc read timed out: {0}")]
    Timeout(&'static str),
}

/// Swap direction on the bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSide {
    /// Tokens bought with SOL.
    Buy,
    /// Tokens sold for SOL.
    Sell,
}

/// One classified Pump.fun swap inside a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapRecord {
    /// Transaction signature (base58).
    pub signature: String,
    /// Slot the transaction landed in (creation-slot rule input).
    pub slot: u64,
    /// Trader (`user` account, index 6).
    pub wallet: Pubkey,
    /// Direction by discriminator.
    pub side: SwapSide,
    /// Token base units moved (Borsh `amount` arg; 0 when undecodable).
    pub amount_base_units: u64,
}

impl SwapRecord {
    /// Build from strings (parses the wallet; handy for tests/plumbing).
    pub fn new(
        signature: &str,
        wallet: &str,
        side: SwapSide,
        slot: u64,
        amount_base_units: u64,
    ) -> Result<Self, ScreenError> {
        Ok(Self {
            signature: signature.to_string(),
            slot,
            wallet: parse_address(wallet)?,
            side,
            amount_base_units,
        })
    }
}

/// Classify every Pump.fun buy/sell instruction in a fetched transaction.
/// Covers legacy `buy`/`sell` plus `buy_v2` (v2/mayhem-curve buys use a
/// different discriminator and a 27-account layout with the trader at
/// index 13). Non-pump instructions, undecodable data, and short account
/// lists are skipped. Never panics.
pub fn extract_pump_swaps(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    sig: &str,
) -> Vec<SwapRecord> {
    let mut out = Vec::new();
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return out;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return out;
    };
    let Some(keys) = resolve_keys(raw, &tx_with_meta.meta) else {
        return out;
    };

    let mut classify = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        let program = keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default();
        if program != PUMP_PROGRAM_ID {
            return;
        }
        let Some(raw) = decode_ix_data(data_b58) else {
            return;
        };
        // (side, trader account index): v2 buys share the side but not
        // the layout. `sell_v2` is unknown and stays unhandled.
        let (side, user_idx) = if raw.starts_with(&PUMP_BUY_DISCRIMINATOR) {
            (SwapSide::Buy, IDX_SWAP_USER)
        } else if raw.starts_with(&PUMP_SELL_DISCRIMINATOR) {
            (SwapSide::Sell, IDX_SWAP_USER)
        } else if raw.starts_with(&PUMP_BUY_V2_DISCRIMINATOR) {
            (SwapSide::Buy, IDX_BUY_V2_USER)
        } else {
            return;
        };
        let Some(account) = accounts.get(user_idx) else {
            return;
        };
        let Some(wallet) = keys.get(usize::from(*account)).copied() else {
            return;
        };
        let amount_base_units = raw
            .get(8..16)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0);
        out.push(SwapRecord {
            signature: sig.to_string(),
            slot: fetched.slot,
            wallet,
            side,
            amount_base_units,
        });
    };

    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut classify);
    out
}

/// Count Pump.fun `create` instructions authored by `dev` in a transaction.
/// Powers the prior-launch check. Never panics.
pub fn count_dev_creates(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    dev: &Pubkey,
) -> usize {
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return 0;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return 0;
    };
    let Some(keys) = resolve_keys(raw, &tx_with_meta.meta) else {
        return 0;
    };

    let mut count = 0;
    let mut check = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        if keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default()
            != PUMP_PROGRAM_ID
        {
            return;
        }
        let Some(raw) = decode_ix_data(data_b58) else {
            return;
        };
        if !raw.starts_with(&PUMP_CREATE_DISCRIMINATOR) {
            return;
        }
        let author = accounts
            .get(IDX_CREATE_USER)
            .and_then(|idx| keys.get(usize::from(*idx)).copied());
        if author == Some(*dev) {
            count += 1;
        }
    };
    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut check);
    count
}

/// Parse a base58 address, mapping failure to [`ScreenError`].
pub fn parse_address(s: &str) -> Result<Pubkey, ScreenError> {
    Pubkey::from_str(s).map_err(|_| ScreenError::BadAddress(s.to_string()))
}

/// Deterministic fixture wallet address (base58 of `[byte; 32]`) for
/// tests and offline plumbing demos.
pub fn fixture_wallet(byte: u8) -> String {
    bs58::encode([byte; 32]).into_string()
}

/// Gate outcome for one observation round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Quorum reached: open with this distinct-buyer count as velocity.
    Confirm {
        /// Distinct non-dev buyers observed.
        buyers: u32,
    },
    /// Never open: dev dump, timeout, or (via sweep) stale entry.
    Abort {
        /// Human-readable cause for logs.
        reason: String,
    },
    /// Keep watching.
    Wait,
}

/// Pending watchlist entry.
#[derive(Debug)]
struct PendingGate {
    dev: Pubkey,
    deadline_secs: u64,
    buyers: HashSet<Pubkey>,
    /// First-submit time: caps relaunch refreshes so spam cannot extend
    /// the window forever (see [`EntryGate::submit`]).
    created_secs: u64,
}

/// Time-windowed buyer-quorum state machine. Pure except for the clock
/// the caller passes in — fully unit-testable.
#[derive(Debug, Default)]
pub struct EntryGate {
    window_secs: u64,
    min_buyers: u32,
    pending: HashMap<String, PendingGate>,
}

impl EntryGate {
    /// New gate: entries get `window_secs` to gather `min_buyers`.
    pub fn new(window_secs: u64, min_buyers: u32) -> Self {
        Self {
            window_secs,
            min_buyers: min_buyers.max(1),
            pending: HashMap::new(),
        }
    }

    /// Watchlist size (for metrics).
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// True when a mint currently has a pending entry.
    pub fn contains(&self, mint: &str) -> bool {
        self.pending.contains_key(mint)
    }

    /// Submit a fresh signal (idempotent per mint). Returns false when
    /// the dev address is unparsable (fail-closed: caller must skip).
    ///
    /// Relaunch refreshes the window but buyers carry over (demand already
    /// observed stays valid). The refresh is capped at `created + 3×window`
    /// so a spammed relaunch cannot extend the watchlist entry forever.
    pub fn submit(&mut self, mint: &str, dev: &str, now_secs: u64) -> bool {
        let Ok(dev) = Pubkey::from_str(dev) else {
            return false;
        };
        let deadline_secs = now_secs.saturating_add(self.window_secs);
        match self.pending.get_mut(mint) {
            Some(entry) => {
                entry.dev = dev;
                let cap = entry.created_secs.saturating_add(self.window_secs.saturating_mul(3));
                entry.deadline_secs = deadline_secs.min(cap);
            }
            None => {
                self.pending.insert(
                    mint.to_string(),
                    PendingGate { dev, deadline_secs, buyers: HashSet::new(), created_secs: now_secs },
                );
            }
        }
        true
    }

    /// Fold one observation round into the entry. Removes the entry on
    /// terminal outcomes. Unknown mints report [`GateOutcome::Wait`].
    pub fn observe(&mut self, mint: &str, swaps: &[SwapRecord], now_secs: u64) -> GateOutcome {
        let Some(entry) = self.pending.get_mut(mint) else {
            return GateOutcome::Wait;
        };
        for swap in swaps {
            match swap.side {
                SwapSide::Buy => {
                    if swap.wallet != entry.dev {
                        entry.buyers.insert(swap.wallet);
                    }
                }
                SwapSide::Sell => {
                    if swap.wallet == entry.dev {
                        let reason = format!("dev dump by {} in {}", swap.wallet, swap.signature);
                        self.pending.remove(mint);
                        return GateOutcome::Abort { reason };
                    }
                }
            }
        }
        let buyers = entry.buyers.len();
        if buyers >= self.min_buyers as usize {
            self.pending.remove(mint);
            return GateOutcome::Confirm {
                buyers: buyers as u32,
            };
        }
        if now_secs >= entry.deadline_secs {
            let reason = format!("timeout: only {buyers} distinct buyers in window");
            self.pending.remove(mint);
            return GateOutcome::Abort { reason };
        }
        GateOutcome::Wait
    }

    /// Abort every entry past its deadline. Returns `(mint, outcome)` pairs.
    pub fn sweep_timeouts(&mut self, now_secs: u64) -> Vec<(String, GateOutcome)> {
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, entry)| now_secs >= entry.deadline_secs)
            .map(|(mint, _)| mint.clone())
            .collect();
        stale
            .into_iter()
            .map(|mint| {
                let buyers = self
                    .pending
                    .remove(&mint)
                    .map(|e| e.buyers.len())
                    .unwrap_or(0);
                (
                    mint,
                    GateOutcome::Abort {
                        reason: format!("timeout: only {buyers} distinct buyers in window"),
                    },
                )
            })
            .collect()
    }
}

/// Developer history policy.
#[derive(Debug, Clone)]
pub struct DevScreenConfig {
    /// Minimum wallet age in seconds (default 3600: reject fresh wallets).
    pub min_age_secs: u64,
    /// Maximum tolerated prior Pump.fun launches (default 1).
    pub max_prior_launches: usize,
    /// Recent transactions sampled for the launch count (default 20).
    pub sample_txs: usize,
    /// Signature window fetched per wallet (default 100).
    pub sig_limit: usize,
}

impl Default for DevScreenConfig {
    fn default() -> Self {
        Self {
            min_age_secs: 3600,
            max_prior_launches: 1,
            sample_txs: 20,
            sig_limit: 100,
        }
    }
}

/// Screening verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevVerdict {
    /// True when the wallet may be trusted with a position.
    pub pass: bool,
    /// Wallet age in seconds (`None` = no history at all).
    pub age_secs: Option<u64>,
    /// Pump.fun launches authored in the sampled window.
    pub prior_launches: usize,
    /// Human-readable cause for logs.
    pub reason: String,
}

/// Pure policy over measured history. `age_secs=None` always fails
/// (fail-closed: no history, no trust).
pub fn judge_developer(
    age_secs: Option<u64>,
    prior_launches: usize,
    cfg: &DevScreenConfig,
) -> DevVerdict {
    let Some(age) = age_secs else {
        return DevVerdict {
            pass: false,
            age_secs: None,
            prior_launches,
            reason: "no on-chain history (fresh wallet)".to_string(),
        };
    };
    if age < cfg.min_age_secs {
        return DevVerdict {
            pass: false,
            age_secs: Some(age),
            prior_launches,
            reason: format!("wallet age {age}s below minimum {}", cfg.min_age_secs),
        };
    }
    if prior_launches > cfg.max_prior_launches {
        return DevVerdict {
            pass: false,
            age_secs: Some(age),
            prior_launches,
            reason: format!(
                "serial launcher: {prior_launches} prior launches (max {})",
                cfg.max_prior_launches
            ),
        };
    }
    DevVerdict {
        pass: true,
        age_secs: Some(age),
        prior_launches,
        reason: "history acceptable".to_string(),
    }
}

/// RPC-backed screening: wallet age from the oldest signature plus a
/// sampled prior-launch count. Transport errors propagate (pipeline treats
/// them as fail-closed skips).
pub async fn screen_developer(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    dev: &Pubkey,
    now_secs: u64,
    cfg: &DevScreenConfig,
) -> Result<DevVerdict, ClientError> {
    let sigs = rpc
        .get_signatures_for_address_with_config(
            dev,
            GetConfirmedSignaturesForAddress2Config {
                before: None,
                until: None,
                limit: Some(cfg.sig_limit),
                commitment: Some(commitment),
            },
        )
        .await?;
    if sigs.is_empty() {
        return Ok(judge_developer(None, 0, cfg));
    }
    // Newest-first: the last entry is the oldest observed activity.
    let age_secs = sigs
        .last()
        .and_then(|s| s.block_time)
        .map(|t| now_secs.saturating_sub(t.max(0) as u64));

    let tx_config = json_tx_config(commitment);
    // Concurrent fan-out with per-tx deadlines: 20 sequential fetches
    // could consume the whole 15s budget. Failed transactions are skipped
    // (a reverted `create` never launched), unreadable ones don't count
    // (lower bound; the age gate carries the strictness).
    let candidates: Vec<(Signature, bool)> = sigs
        .iter()
        .take(cfg.sample_txs)
        .filter_map(|s| {
            Signature::from_str(&s.signature)
                .ok()
                .map(|sig| (sig, s.err.is_some()))
        })
        .collect();
    let fetches = futures_util::future::join_all(candidates.iter().map(|(sig, failed)| async move {
        if *failed {
            return 0usize;
        }
        let fetched = match tokio::time::timeout(
            TX_FETCH_TIMEOUT,
            rpc.get_transaction_with_config(sig, tx_config),
        )
        .await
        {
            Ok(Ok(fetched)) => fetched,
            _ => return 0usize,
        };
        // Defensive: skip failed transactions even if the status flag was
        // missing (a failed create never launched a token).
        let failed_tx = fetched
            .transaction
            .meta
            .as_ref()
            .is_some_and(|m| m.err.is_some());
        if failed_tx {
            return 0usize;
        }
        count_dev_creates(&fetched, dev)
    }))
    .await;
    let prior_launches: usize = fetches.iter().sum();
    Ok(judge_developer(age_secs, prior_launches, cfg))
}

/// RPC-backed swap reader for one curve: signatures since `since_sig`
/// (newest-first walk, stop at the watermark), classified records.
/// Returns `(records, newest_signature)`.
///
/// Paginated (up to [`CURVE_SWAP_MAX_PAGES`] × [`CURVE_SWAP_PAGE_LIMIT`]):
/// when `since_sig` is not found and the last page is full, the watermark
/// is stale — the caller keeps watching but high-velocity gaps are
/// expected. Failed transactions are skipped; undecodable ones simply
/// contribute no records (gate stays `Wait`: fail-closed for entries).
pub async fn read_curve_swaps(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    curve: &Pubkey,
    since_sig: Option<&str>,
) -> Result<(Vec<SwapRecord>, Option<String>), ClientError> {
    let tx_config = json_tx_config(commitment);
    let mut records = Vec::new();
    let mut newest: Option<String> = None;
    let mut before: Option<Signature> = None;
    let mut watermark_found = since_sig.is_none();
    let mut pages = 0usize;

    for _ in 0..CURVE_SWAP_MAX_PAGES {
        let sigs = rpc
            .get_signatures_for_address_with_config(
                curve,
                GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(CURVE_SWAP_PAGE_LIMIT),
                    commitment: Some(commitment),
                },
            )
            .await?;
        if sigs.is_empty() {
            break;
        }
        pages += 1;
        let page_full = sigs.len() >= CURVE_SWAP_PAGE_LIMIT;

        for status in &sigs {
            if Some(status.signature.as_str()) == since_sig {
                watermark_found = true;
                break;
            }
            if newest.is_none() {
                newest = Some(status.signature.clone());
            }
            if status.err.is_some() {
                continue;
            }
            let Ok(signature) = Signature::from_str(&status.signature) else {
                continue;
            };
            // Per-tx deadline: one slow fetch must not consume the whole
            // screening budget (outer `with_screen_timeout` is the backstop).
            let fetched = match tokio::time::timeout(
                TX_FETCH_TIMEOUT,
                rpc.get_transaction_with_config(&signature, tx_config),
            )
            .await
            {
                Ok(Ok(fetched)) => fetched,
                _ => continue,
            };
            records.extend(extract_pump_swaps(&fetched, &status.signature));
        }
        if watermark_found {
            break;
        }
        // Watermark not in this page: continue with `before` cursor, unless
        // the page was short (history exhausted).
        if !page_full {
            break;
        }
        before = sigs
            .last()
            .and_then(|s| Signature::from_str(&s.signature).ok());
        let Some(cursor) = before else { break };
        before = Some(cursor);
    }

    if since_sig.is_some() && !watermark_found && pages >= CURVE_SWAP_MAX_PAGES {
        // High-velocity gap: newest 75 returned, intermediates missed.
        // Callers treat this as "keep watching"; the gate cannot confirm
        // on a partial view with confidence, but we must not silently
        // claim completeness.
        tracing::debug!(
            pages,
            fetched = records.len(),
            "curve read truncated at page cap; watermark stale"
        );
    }
    Ok((records, newest))
}

/// Poll interval floor for gate rounds (not a tight loop: RPC budget).
pub const GATE_POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// Resolve the full account-key list for a raw message: static keys plus
/// address-table-loaded addresses (writable then readonly). `None` on any
/// undecodable key — callers fail the whole transaction rather than
/// mis-resolve indices (a wrong program match is worse than a miss).
fn resolve_keys(
    raw: &UiRawMessage,
    meta: &Option<UiTransactionStatusMeta>,
) -> Option<Vec<Pubkey>> {
    let mut keys: Vec<Pubkey> = Vec::with_capacity(raw.account_keys.len());
    for key in &raw.account_keys {
        keys.push(Pubkey::from_str(key).ok()?);
    }
    if let Some(meta) = meta
        && let OptionSerializer::Some(loaded) = &meta.loaded_addresses
    {
        for key in loaded.writable.iter().chain(loaded.readonly.iter()) {
            keys.push(Pubkey::from_str(key).ok()?);
        }
    }
    Some(keys)
}

/// Standard JSON transaction-fetch config (shared by all readers).
/// Accepts transaction versions through v1: current Pump.fun launches
/// are v1 envelopes, and `Some(0)` makes the RPC refuse them outright
/// (-32015). The Json wire shape decodes identically across versions
/// (proven by the mainnet `create_v2` replay fixture).
pub fn json_tx_config(commitment: CommitmentConfig) -> RpcTransactionConfig {
    RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(1),
    }
}

/// Single fan-out over every compiled instruction in a fetched
/// transaction (top-level plus inner compiled). DRY helper shared by
/// [`extract_pump_swaps`], [`count_dev_creates`], and
/// [`find_funding_source`] so the top/inner traversal cannot drift.
fn for_each_compiled_ix(
    raw: &UiRawMessage,
    meta: &Option<UiTransactionStatusMeta>,
    f: &mut impl FnMut(u8, &[u8], &str),
) {
    for cix in &raw.instructions {
        f(cix.program_id_index, &cix.accounts, &cix.data);
    }
    if let Some(meta) = meta
        && let OptionSerializer::Some(sets) = &meta.inner_instructions
    {
        for set in sets {
            for cix in &set.instructions {
                if let UiInstruction::Compiled(compiled) = cix {
                    f(
                        compiled.program_id_index,
                        &compiled.accounts,
                        &compiled.data,
                    );
                }
            }
        }
    }
}

/// RPC read deadline for screening paths (no unbounded hangs).
///
/// Timeout budget per path (outer → inner, inner must fire first):
/// - screening (`dev_verdict`, `curve_swaps`, `hunt_migration`,
///   `bundle_verdict`): 15s outer / 5s per-tx fetch.
/// - funder tracing: 15s outer / 8s per-buyer walk (page-capped).
/// - curve/vault tickers: 15s per round (`ROUND_TIMEOUT`), 1s poll
///   interval (a slow round skips; misses back off).
/// - executor: 90s send-and-confirm, 15s blockhash/poll, 10s balance
///   reads, 2s Jito HTTP; live confirm window 60s with pending recheck.
pub const SCREEN_RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// Run an RPC future with the screening deadline. `None` on timeout.
async fn with_screen_timeout<F, T>(future: F) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(SCREEN_RPC_TIMEOUT, future).await.ok()
}

/// Detected migration, ready for alerting and swap routing.
///
/// RPC-layer shape: holds typed [`Pubkey`]s and the parsed
/// [`RaydiumV4Pool`]. Convert to [`MigrationReport`] at the crate
/// boundary (see [`RpcScreener::hunt_migration`] callers) — the two types
/// are an intentional layer split, not duplication: typed inside
/// `listener`, plain strings outside it.
#[derive(Debug, Clone)]
pub struct DetectedMigration {
    /// Migration transaction signature (base58).
    pub signature: String,
    /// Where liquidity landed.
    pub destination: MigrationDestination,
    /// New AMM account for Raydium migrations (initialize2 account 1).
    pub amm_id: Option<Pubkey>,
    /// Parsed pool for Raydium migrations (swap-ready addresses).
    pub pool: Option<RaydiumV4Pool>,
}

/// Binary-friendly migration report (plain strings for cross-crate use).
/// Produced from [`DetectedMigration::report`]; the pipeline's `Screener`
/// trait speaks only this shape so the binary never depends on RPC-layer
/// key types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Migration transaction signature (base58).
    pub signature: String,
    /// Where liquidity landed.
    pub destination: MigrationDestination,
    /// New AMM account, if resolved.
    pub amm_id: Option<String>,
    /// Pool base mint, if parsed.
    pub base_mint: Option<String>,
    /// Pool quote mint, if parsed.
    pub quote_mint: Option<String>,
    /// Coin-side LP vault, if parsed (post-migration tracking).
    pub base_vault: Option<String>,
    /// Pc-side LP vault, if parsed (post-migration tracking).
    pub quote_vault: Option<String>,
}

impl DetectedMigration {
    /// Convert to the plain report shape.
    pub fn report(&self) -> MigrationReport {
        MigrationReport {
            signature: self.signature.clone(),
            destination: self.destination,
            amm_id: self.amm_id.map(|k| k.to_string()),
            base_mint: self.pool.as_ref().map(|p| p.base_mint.to_string()),
            quote_mint: self.pool.as_ref().map(|p| p.quote_mint.to_string()),
            base_vault: self.pool.as_ref().map(|p| p.base_vault.to_string()),
            quote_vault: self.pool.as_ref().map(|p| p.quote_vault.to_string()),
        }
    }
}

/// AMM account index inside a Raydium `initialize2` instruction.
/// Verified against `raydium-amm/program/src/instruction.rs::initialize2`:
/// `[0]=SPL Token program, [1]=ATA program, [2]=system program,
/// [3]=rent sysvar, [4]=new AMM pool` (then authority, open-orders, ...).
/// (A prior revision used index 1, confusing `initialize2` with the
/// `Deposit`/`Swap` layouts where index 1 is the AMM; index 1 here is the
/// ATA *program*, so pool resolution never succeeded.)
pub const INITIALIZE2_AMM_INDEX: usize = 4;

/// Resolve the new AMM id from decoded instructions given as
/// `(program_id, resolved account keys)` pairs. Returns the accounts[1]
/// of the first Raydium-program instruction. Pure and unit-tested.
pub fn initialize2_amm_id(instructions: &[(Pubkey, Vec<Pubkey>)]) -> Option<Pubkey> {
    instructions
        .iter()
        .find(|(program, _)| *program == RAYDIUM_V4_PROGRAM_ID)
        .and_then(|(_, accounts)| accounts.get(INITIALIZE2_AMM_INDEX).copied())
}

/// Creation-slot bundle verdict (skill `sybil-cluster-detection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleVerdict {
    /// No bundle evidence: organic launch.
    SafeOrganic,
    /// Bundled but below the supply monopoly threshold: suspicious,
    /// tradeable only with tightened risk.
    SuspectLowVolume,
    /// Bundled wallets control over 15% of initial supply: reject.
    RejectBundledMonopoly,
}

/// Bundle forensics over early launch swaps.
#[derive(Debug, Clone)]
pub struct BundleAnalysis {
    /// True when bundled by the creation-slot or funder rule.
    pub is_bundled: bool,
    /// Buy swaps landing in the exact creation slot.
    pub creation_slot_buys: usize,
    /// Wallets sharing a funder with at least one other wallet.
    pub shared_funder_count: usize,
    /// Combined supply share of creation-slot buys, in percent.
    pub total_cluster_supply_pct: f64,
    /// Final call.
    pub verdict: BundleVerdict,
}

/// Creation-slot bundle detector (skill `sybil-cluster-detection`).
///
/// Hard rule: a Jito bundle lands atomically in one slot, so no
/// uncoordinated outsider can transact in the token's creation slot —
/// 3+ buys there means a developer/insider bundle. Shared genesis
/// funders (2+ wallets per source) independently prove coordination.
/// Either signal sets `is_bundled`; over 15% creation-slot supply
/// escalates to [`BundleVerdict::RejectBundledMonopoly`].
///
/// Callers pass **buy** swaps only as `(slot, buyer, supply_pct)` plus
/// known `(buyer, funder)` ancestry pairs (empty when untraced — the
/// slot rule still fires).
pub struct BundleDetector;

impl BundleDetector {
    /// Evaluate early swaps against the creation slot.
    pub fn evaluate_launch_slot(
        creation_slot: u64,
        early_swaps: &[(u64, Pubkey, f64)],
        funding_parents: &[(Pubkey, Pubkey)],
    ) -> BundleAnalysis {
        // 1. Creation-slot buys.
        let creation_slot_buys = early_swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .count();

        // 2. Common funder ancestry.
        let mut funder_map: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        for (buyer, funder) in funding_parents {
            funder_map.entry(*funder).or_default().push(*buyer);
        }
        let shared_funder_count: usize = funder_map
            .values()
            .filter(|wallets| wallets.len() >= 2)
            .map(|wallets| wallets.len())
            .sum();

        // 3. Creation-slot supply share.
        let total_cluster_supply_pct: f64 = early_swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .map(|(_, _, pct)| pct)
            .sum();

        let is_bundled = creation_slot_buys >= 3 || shared_funder_count >= 2;

        let verdict = if is_bundled && total_cluster_supply_pct > 15.0 {
            BundleVerdict::RejectBundledMonopoly
        } else if is_bundled {
            BundleVerdict::SuspectLowVolume
        } else {
            BundleVerdict::SafeOrganic
        };

        BundleAnalysis {
            is_bundled,
            creation_slot_buys,
            shared_funder_count,
            total_cluster_supply_pct,
            verdict,
        }
    }
}

/// One decoded instruction for migration classification: program,
/// resolved account keys, and raw instruction data.
#[derive(Debug, Clone)]
struct DecodedIx {
    program: Pubkey,
    accounts: Vec<Pubkey>,
    data: Vec<u8>,
}

/// Decode one fetched transaction for migration classification:
/// full account keys, [`DecodedIx`] per instruction (top-level plus
/// inner compiled), and program logs. `None` on any undecodable key.
/// Pure and unit-tested via downstream asserts.
fn decode_migration_tx(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
) -> Option<(Vec<Pubkey>, Vec<DecodedIx>, Vec<String>)> {
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return None;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return None;
    };
    let keys = resolve_keys(raw, &tx_with_meta.meta)?;

    let mut decoded: Vec<DecodedIx> = Vec::new();
    let mut skipped = 0usize;
    let mut push = |program_id_index: u8, accounts: &[u8], data_str: &str| {
        let program = keys.get(usize::from(program_id_index)).copied()?;
        let mut resolved = Vec::with_capacity(accounts.len());
        for idx in accounts {
            resolved.push(keys.get(usize::from(*idx)).copied()?);
        }
        // Single decode path shared with `decode_ix_data` semantics:
        // bs58 first (Pump/Raydium wire), base64 fallback (RPC JSON).
        let raw = decode_ix_data(data_str)?;
        decoded.push(DecodedIx { program, accounts: resolved, data: raw });
        Some(())
    };
    let mut visit = |program_id_index: u8, accounts: &[u8], data_str: &str| {
        if push(program_id_index, accounts, data_str).is_none() {
            skipped += 1;
        }
    };
    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut visit);
    if skipped > 0 {
        debug!(skipped, decoded = decoded.len(), "migration tx had undecodable ixs; continuing with partial view");
    }

    let logs = match &tx_with_meta.meta {
        Some(meta) => match &meta.log_messages {
            OptionSerializer::Some(logs) => logs.clone(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    Some((keys, decoded, logs))
}

/// RPC-backed screening client: developer verdicts plus incremental
/// curve-swap reads with a per-mint signature watermark.
#[derive(Clone)]
pub struct RpcScreener {
    rpc: Arc<RpcClient>,
    commitment: CommitmentConfig,
    dev_cfg: DevScreenConfig,
    funder_cache: Arc<Mutex<TimedCache<Option<String>>>>,
    dev_cache: Arc<Mutex<TimedCache<DevVerdict>>>,
}

impl RpcScreener {
    /// Build against an HTTP RPC URL with explicit commitment and dev policy.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        dev_cfg: DevScreenConfig,
    ) -> Result<Self, ScreenError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(ScreenError::BadUrl(http_url.to_string())),
        }
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            commitment,
            dev_cfg,
            funder_cache: Arc::new(Mutex::new(TimedCache::new(FUNDER_CACHE_TTL_SECS))),
            dev_cache: Arc::new(Mutex::new(TimedCache::new(DEV_CACHE_TTL_SECS))),
        })
    }

    /// Developer history verdict (fail-closed on transport errors).
    /// Repeat wallets are served from a TTL cache (history barely moves).
    pub async fn dev_verdict(
        &self,
        dev: &Pubkey,
        now_secs: u64,
    ) -> Result<DevVerdict, ScreenError> {
        if let Some(hit) = self
            .dev_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&dev.to_string(), now_secs)
        {
            return Ok(hit);
        }
        let verdict = match with_screen_timeout(screen_developer(
            &self.rpc,
            self.commitment,
            dev,
            now_secs,
            &self.dev_cfg,
        ))
        .await
        {
            None => return Err(ScreenError::Timeout("dev_verdict")),
            Some(result) => result?,
        };
        self.dev_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(dev.to_string(), verdict.clone(), now_secs);
        Ok(verdict)
    }

    /// Swap records for a curve since `since_sig`, plus the new watermark.
    pub async fn curve_swaps(
        &self,
        curve: &Pubkey,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>), ScreenError> {
        match with_screen_timeout(read_curve_swaps(
            &self.rpc,
            self.commitment,
            curve,
            since_sig,
        ))
        .await
        {
            None => Err(ScreenError::Timeout("curve_swaps")),
            Some(result) => result.map_err(ScreenError::Rpc),
        }
    }

    /// Hunt the migration transaction for a graduated curve: scan recent
    /// signatures, classify each candidate, and on a Raydium hit resolve
    /// the new AMM id (initialize2 account 1) plus its parsed pool.
    /// The pool is trusted only after the full gate: Raydium-owned
    /// account, WSOL quote, and base matching `expected_base_mint` —
    /// anything else keeps scanning. Returns `None` when no migration is
    /// found. Runs once per mint upstream — never in a hot loop.
    /// Bounded by [`SCREEN_RPC_TIMEOUT`] like every other screening path
    /// (previously this was the only unbounded RPC path).
    pub async fn hunt_migration(
        &self,
        curve: &Pubkey,
        expected_base_mint: &Pubkey,
    ) -> Result<Option<DetectedMigration>, ScreenError> {
        match with_screen_timeout(self.hunt_migration_inner(curve, expected_base_mint)).await {
            None => Err(ScreenError::Timeout("hunt_migration")),
            Some(result) => result.map_err(ScreenError::Rpc),
        }
    }

    async fn hunt_migration_inner(
        &self,
        curve: &Pubkey,
        expected_base_mint: &Pubkey,
    ) -> Result<Option<DetectedMigration>, ClientError> {
        let monitor = MigrationMonitor::new();
        let sigs = self
            .rpc
            .get_signatures_for_address_with_config(
                curve,
                GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(10),
                    commitment: Some(self.commitment),
                },
            )
            .await?;
        let tx_config = json_tx_config(self.commitment);
        // First Raydium detection whose pool cannot be resolved yet
        // (transient RPC or unparseable account): kept as a fallback and
        // returned only after every candidate is scanned, so one bad
        // account never hides the real migration behind it.
        let mut fallback: Option<(String, Option<Pubkey>)> = None;
        for status in &sigs {
            if status.err.is_some() {
                continue;
            }
            let Ok(signature) = Signature::from_str(&status.signature) else {
                continue;
            };
            let fetched = match tokio::time::timeout(
                TX_FETCH_TIMEOUT,
                self.rpc.get_transaction_with_config(&signature, tx_config),
            )
            .await
            {
                Ok(Ok(fetched)) => fetched,
                _ => continue,
            };
            let Some((keys, decoded, logs)) = decode_migration_tx(&fetched) else {
                continue;
            };
            let mut ix_datas: Vec<Vec<u8>> = Vec::with_capacity(decoded.len());
            let mut pairs: Vec<(Pubkey, Vec<Pubkey>)> = Vec::with_capacity(decoded.len());
            for ix in decoded {
                ix_datas.push(ix.data);
                pairs.push((ix.program, ix.accounts));
            }
            match monitor.classify_migration(&keys, &ix_datas, &logs) {
                Some(MigrationDestination::RaydiumV4) => {
                    let amm_id = initialize2_amm_id(&pairs);
                    let pool = match amm_id {
                        Some(amm) => {
                            let account = match tokio::time::timeout(
                                TX_FETCH_TIMEOUT,
                                self.rpc.get_account(&amm),
                            )
                            .await
                            {
                                Ok(Ok(account)) => account,
                                _ => {
                                    debug!(%amm, "pool account fetch failed; keeping as fallback, continuing hunt");
                                    if fallback.is_none() {
                                        fallback = Some((status.signature.clone(), amm_id));
                                    }
                                    continue;
                                }
                            };
                            match RaydiumV4Pool::parse_validated(
                                &amm,
                                &account.data,
                                &account.owner,
                                expected_base_mint,
                            ) {
                                Some(pool) => Some(pool),
                                None => {
                                    debug!(%amm, "pool failed trust gate; keeping as fallback, continuing hunt");
                                    if fallback.is_none() {
                                        fallback = Some((status.signature.clone(), amm_id));
                                    }
                                    continue;
                                }
                            }
                        }
                        None => {
                            debug!("raydium migration without resolvable AMM; continuing hunt");
                            if fallback.is_none() {
                                fallback = Some((status.signature.clone(), None));
                            }
                            continue;
                        }
                    };
                    return Ok(Some(DetectedMigration {
                        signature: status.signature.clone(),
                        destination: MigrationDestination::RaydiumV4,
                        amm_id,
                        pool,
                    }));
                }
                Some(MigrationDestination::PumpSwap) => {
                    return Ok(Some(DetectedMigration {
                        signature: status.signature.clone(),
                        destination: MigrationDestination::PumpSwap,
                        amm_id: None,
                        pool: None,
                    }));
                }
                None => continue,
            }
        }
        if let Some((signature, amm_id)) = fallback {
            warn!("raydium migration detected but pool unresolvable; reporting without pool");
            return Ok(Some(DetectedMigration {
                signature,
                destination: MigrationDestination::RaydiumV4,
                amm_id,
                pool: None,
            }));
        }
        Ok(None)
    }

    /// Full bundle forensics for a curve at confirm time: recent buy
    /// swaps with slots and supply shares, genesis funders for the first
    /// few distinct buyers, evaluated against `creation_slot`.
    /// Transport errors propagate (fail-closed upstream).
    pub async fn bundle_verdict(
        &self,
        curve: &Pubkey,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ScreenError> {
        match with_screen_timeout(self.bundle_verdict_tiered(curve, creation_slot)).await {
            None => Err(ScreenError::Timeout("bundle_verdict")),
            Some(result) => result.map(|(analysis, _)| analysis).map_err(ScreenError::Rpc),
        }
    }

    /// Two-tier shortcut is applied by the caller shape below: when the
    /// slot rule alone already proves a bundle, funders cannot change the
    /// verdict, so [`Self::bundle_verdict`] skips their walks entirely.
    /// Returns `(analysis, walked_funders)`.
    ///
    /// Slot attribution uses the buyer's **earliest** slot (`min`), and the
    /// supply share counts **creation-slot buys only** — previously the
    /// newest-first order plus all-slot sums overcounted the monopoly rule
    /// into false Rejects. When funder tracing is mostly untraceable the
    /// verdict is escalated to `SuspectLowVolume` (fail-closed: unknown is
    /// not organic).
    async fn bundle_verdict_tiered(
        &self,
        curve: &Pubkey,
        creation_slot: u64,
    ) -> Result<(BundleAnalysis, bool), ClientError> {
        let (records, _) = read_curve_swaps(&self.rpc, self.commitment, curve, None).await?;
        // Fail closed on unreadable supply: a zero fallback would neuter
        // the >15% monopoly rule into a permanent Suspect.
        let total_supply = self.curve_total_supply(curve).await?;

        // Distinct buyers, first-seen order, bounded (RPC budget).
        let mut seen: HashSet<Pubkey> = HashSet::new();
        let mut buyers: Vec<Pubkey> = Vec::new();
        for record in records.iter().filter(|r| r.side == SwapSide::Buy) {
            if seen.insert(record.wallet) {
                buyers.push(record.wallet);
            }
            if buyers.len() >= MAX_TRACED_BUYERS {
                break;
            }
        }

        let mut swaps: Vec<(u64, Pubkey, f64)> = Vec::new();
        for buyer in &buyers {
            // Earliest slot across this buyer's buys (records are
            // newest-first from RPC; `find` would return the newest).
            let slot = records
                .iter()
                .filter(|r| r.side == SwapSide::Buy && r.wallet == *buyer)
                .map(|r| r.slot)
                .min()
                .unwrap_or(u64::MAX);
            // Creation-slot amount only: later accumulation must not
            // inflate the monopoly share attributed to the launch slot.
            let creation_amount: u64 = records
                .iter()
                .filter(|r| {
                    r.side == SwapSide::Buy && r.wallet == *buyer && r.slot == creation_slot
                })
                .fold(0u64, |acc, r| acc.saturating_add(r.amount_base_units));
            let pct = if total_supply > 0 {
                creation_amount as f64 / total_supply as f64 * 100.0
            } else {
                0.0
            };
            swaps.push((slot, *buyer, pct));
        }

        let slot_buys = swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .count();
        let walked = slot_buys < 3;
        let now = unix_now();
        let funding_parents = if walked {
            self.trace_funders(&buyers, now).await
        } else {
            Vec::new()
        };

        let mut analysis =
            BundleDetector::evaluate_launch_slot(creation_slot, &swaps, &funding_parents);
        // Fail-closed on untraceable funders: when we walked but resolved
        // fewer than half the buyers (cache + fresh), unknown ancestry must
        // not read as organic. Escalate Safe → Suspect.
        if walked && analysis.verdict == BundleVerdict::SafeOrganic {
            let traced: HashSet<Pubkey> =
                funding_parents.iter().map(|(b, _)| *b).collect();
            let resolved = buyers.iter().filter(|b| traced.contains(b)).count();
            // `+1` avoids divide noise on tiny sets; threshold = <50%.
            if !buyers.is_empty() && resolved * 2 < buyers.len() {
                analysis.verdict = BundleVerdict::SuspectLowVolume;
                analysis.is_bundled = true;
            }
        }

        Ok((analysis, walked))
    }

    /// Cached, concurrent, deadline-bound funder tracing.
    /// `now` is the caller's wall-clock snapshot (single source per round;
    /// see [`unix_now`]) so TTL edges are deterministic within one verdict.
    async fn trace_funders(&self, buyers: &[Pubkey], now: u64) -> Vec<(Pubkey, Pubkey)> {
        let mut pairs: Vec<(Pubkey, Pubkey)> = Vec::new();
        let mut missing: Vec<Pubkey> = Vec::new();
        {
            let cache = self.funder_cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for buyer in buyers {
                match cache.get(&buyer.to_string(), now) {
                    Some(Some(funder)) => {
                        if let Ok(funder) = Pubkey::from_str(&funder) {
                            pairs.push((*buyer, funder));
                        }
                    }
                    Some(None) => {}
                    None => missing.push(*buyer),
                }
            }
        }
        if missing.is_empty() {
            return pairs;
        }
        let fresh = futures_util::future::join_all(missing.iter().map(|buyer| async move {
            let found = tokio::time::timeout(FUNDER_TRACE_TIMEOUT, self.genesis_funder(buyer))
                .await
                .ok()
                .flatten()
                .map(|funder| funder.to_string());
            (*buyer, found)
        }))
        .await;
        {
            let mut cache = self.funder_cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for (buyer, funder) in &fresh {
                cache.put(buyer.to_string(), funder.clone(), now);
                if let Some(funder) = funder
                    && let Ok(funder) = Pubkey::from_str(funder)
                {
                    pairs.push((*buyer, funder));
                }
            }
        }
        pairs
    }

    /// Total token supply from the curve account (`token_total_supply`
    /// u64 at offset 40). Fail-closed: any unreadable supply (transport
    /// error, timeout, short data) propagates as `Err` so the >15%
    /// monopoly rule can never silently degrade to `Suspect` on a zero
    /// fallback. Bounded by [`TX_FETCH_TIMEOUT`] so one slow account
    /// fetch cannot stall the bundle verdict.
    async fn curve_total_supply(&self, curve: &Pubkey) -> Result<u64, ClientError> {
        let account = match tokio::time::timeout(TX_FETCH_TIMEOUT, self.rpc.get_account(curve)).await
        {
            Ok(Ok(account)) => account,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(solana_client::client_error::ClientErrorKind::Custom(
                    "curve_total_supply timed out".to_string(),
                )
                .into());
            }
        };
        let supply = account
            .data
            .get(curve::TOKEN_SUPPLY_OFFSET..curve::TOKEN_SUPPLY_OFFSET + curve::TOKEN_SUPPLY_LEN)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
            .filter(|s| *s > 0)
            .ok_or_else(|| {
                solana_client::client_error::ClientErrorKind::Custom(
                    "curve account missing token_total_supply".to_string(),
                )
            })?;
        Ok(supply)
    }

    /// Genesis funder: walk the wallet's signature history newest-first
    /// (pages of 1000, capped at [`MAX_FUNDER_PAGES`]) down to its first
    /// transaction, then return the source of the first SOL funding
    /// found there (oldest few tried in order). `None` when the wallet
    /// has no history, exceeds the page cap, or nothing parses.
    /// The caller deadline-bounds each trace via [`FUNDER_TRACE_TIMEOUT`].
    async fn genesis_funder(&self, buyer: &Pubkey) -> Option<Pubkey> {
        let mut before: Option<Signature> = None;
        let mut oldest_page: Vec<(String, bool)> = Vec::new();
        let mut pages = 0u32;
        loop {
            if pages >= MAX_FUNDER_PAGES {
                debug!(%buyer, pages, "funder walk hit page cap; treating as untraceable");
                break;
            }
            let page = self
                .rpc
                .get_signatures_for_address_with_config(
                    buyer,
                    GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: None,
                        limit: Some(1000),
                        commitment: Some(self.commitment),
                    },
                )
                .await
                .ok()?;
            if page.is_empty() {
                break;
            }
            pages += 1;
            let exhausted = page.len() < 1000;
            before = page
                .last()
                .and_then(|s| Signature::from_str(&s.signature).ok());
            oldest_page = page
                .iter()
                .map(|s| (s.signature.clone(), s.err.is_some()))
                .collect();
            if exhausted {
                break;
            }
        }
        if oldest_page.is_empty() {
            return None;
        }
        let tx_config = json_tx_config(self.commitment);
        // Oldest first; a few fallbacks in case the very first
        // transaction carries no parseable funding. Failed
        // transactions never fund anyone.
        for (sig, failed) in oldest_page.iter().rev().take(FUNDER_TRAIL) {
            if *failed {
                continue;
            }
            let Ok(signature) = Signature::from_str(sig) else {
                continue;
            };
            let Ok(fetched) = self
                .rpc
                .get_transaction_with_config(&signature, tx_config)
                .await
            else {
                continue;
            };
            if let Some(funder) = find_funding_source(&fetched, buyer) {
                return Some(funder);
            }
        }
        None
    }
}

/// Maximum distinct buyers traced for funders per evaluation.
pub const MAX_TRACED_BUYERS: usize = 8;
/// Per-buyer genesis-trace deadline: bounds the stall when one wallet
/// has a pathological history (the page walk itself is page-capped).
/// Must stay below [`SCREEN_RPC_TIMEOUT`] so the inner deadline fires
/// first and the outer timeout remains a backstop, not the primary path.
pub const FUNDER_TRACE_TIMEOUT: Duration = Duration::from_secs(8);
/// Per-transaction fetch deadline inside fan-out reads (dev sampling,
/// curve pages). Keeps one slow RPC from consuming the whole 15s budget.
pub const TX_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Curve-swap signature page size and page cap: 3 pages × 25 = 75 newest
/// signatures per round. Beyond that the watermark is stale and the
/// caller is warned via `truncated` handling upstream.
pub const CURVE_SWAP_PAGE_LIMIT: usize = 25;
pub const CURVE_SWAP_MAX_PAGES: usize = 3;
/// Genesis funder page-walk cap: 10 pages × 1000 signatures. Beyond this
/// the wallet is treated as untraceable (RPC budget over completeness).
pub const MAX_FUNDER_PAGES: u32 = 10;
/// Funder ancestry cache TTL: sybil infrastructure is reused across
/// launches, so repeat buyers cost zero RPC.
pub const FUNDER_CACHE_TTL_SECS: u64 = 86_400;
/// Dev-verdict cache TTL: history barely moves within an hour.
pub const DEV_CACHE_TTL_SECS: u64 = 3_600;
/// Signatures kept from the oldest page for funding fallbacks.
const FUNDER_TRAIL: usize = 4;

/// Tiny TTL map for RPC-derived facts. Pure time math — unit-tested.
#[derive(Debug, Default)]
struct TimedCache<V: Clone> {
    ttl_secs: u64,
    entries: HashMap<String, (V, u64)>,
}

impl<V: Clone> TimedCache<V> {
    fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            entries: HashMap::new(),
        }
    }

    /// Hit (fresh) or `None` on miss/expiry. Never panics.
    fn get(&self, key: &str, now_secs: u64) -> Option<V> {
        self.entries.get(key).and_then(|(value, at)| {
            if now_secs.saturating_sub(*at) <= self.ttl_secs {
                Some(value.clone())
            } else {
                None
            }
        })
    }

    fn put(&mut self, key: String, value: V, now_secs: u64) {
        // Opportunistic eviction: drop expired entries on every insert so a
        // long-running sniper cannot grow this map without bound.
        // `retain` on a small map (<100 entries typical) is negligible.
        let ttl = self.ttl_secs;
        self.entries
            .retain(|_, (_, at)| now_secs.saturating_sub(*at) <= ttl);
        // Hard cap as a second backstop (funder sybil reuse is bounded).
        if self.entries.len() > 10_000
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, (value, now_secs));
    }
}

/// Local unix seconds for cache TTLs.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Find the source of a native SOL transfer into `beneficiary` inside a
/// fetched transaction (top-level and inner instructions). Handles both
/// funding shapes: system `transfer` (u32 LE 2, 12 bytes, accounts
/// `[from, to]`) and system `createAccount` (u32 LE 0, 52 bytes, accounts
/// `[funder, new]`). Pure; `None` when no such transfer exists.
pub fn find_funding_source(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    beneficiary: &Pubkey,
) -> Option<Pubkey> {
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return None;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return None;
    };
    let keys = resolve_keys(raw, &tx_with_meta.meta)?;

    let scan = |program_id_index: u8, accounts: &[u8], data_b58: &str| -> Option<Pubkey> {
        if keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default()
            != SYSTEM_PROGRAM_ID
        {
            return None;
        }
        let raw = decode_ix_data(data_b58)?;
        // System transfer (u32 LE 2, 12 bytes, [from, to]) or account
        // creation (u32 LE 0, 52 bytes, [funder, new]).
        // Length-guarded BEFORE slicing: short payloads are not funding,
        // never a panic (this function promises never to panic).
        if raw.len() < 4 {
            return None;
        }
        let tag = u32::from_le_bytes(raw[..4].try_into().ok()?);
        let (from_idx, to_idx) = match (raw.len(), tag) {
            (12, 2) | (52, 0) => (0usize, 1usize),
            _ => return None,
        };
        let from = keys.get(usize::from(*accounts.get(from_idx)?)).copied()?;
        let to = keys.get(usize::from(*accounts.get(to_idx)?)).copied()?;
        (to == *beneficiary).then_some(from)
    };

    let mut found: Option<Pubkey> = None;
    let mut visit = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        if found.is_some() {
            return;
        }
        if let Some(funder) = scan(program_id_index, accounts, data_b58) {
            found = Some(funder);
        }
    };
    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut visit);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_transaction_status::{
        EncodedTransactionWithStatusMeta, UiCompiledInstruction, UiRawMessage, UiTransaction,
    };

    fn key(byte: u8) -> String {
        Pubkey::new_from_array([byte; 32]).to_string()
    }

    fn buy_ix(program_idx: u8, user_idx: u8) -> UiCompiledInstruction {
        let mut data = PUMP_BUY_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[9u8; 16]);
        UiCompiledInstruction {
            program_id_index: program_idx,
            accounts: vec![20, 21, 22, 23, 24, 25, user_idx, 27],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        }
    }

    fn sell_ix(program_idx: u8, user_idx: u8) -> UiCompiledInstruction {
        let mut data = PUMP_SELL_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[9u8; 16]);
        UiCompiledInstruction {
            program_id_index: program_idx,
            accounts: vec![20, 21, 22, 23, 24, 25, user_idx, 27],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        }
    }

    /// Fabricated Json transaction: account 0 = pump program, account 6
    /// of each swap ix = trader wallet.
    fn swap_tx(
        instructions: Vec<UiCompiledInstruction>,
    ) -> EncodedConfirmedTransactionWithStatusMeta {
        let mut account_keys = vec![PUMP_PROGRAM_ID.to_string()];
        for b in 1u8..=27 {
            account_keys.push(key(b));
        }
        EncodedConfirmedTransactionWithStatusMeta {
            slot: 1,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys,
                        recent_blockhash: key(99),
                        instructions,
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta: None,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        }
    }

    #[test]
    fn classifies_buy_and_sell() {
        // program idx 0 = pump; user idx 6 → account_keys[6] = key(6).
        let tx = swap_tx(vec![buy_ix(0, 6), sell_ix(0, 6)]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].side, SwapSide::Buy);
        assert_eq!(records[1].side, SwapSide::Sell);
        assert_eq!(records[0].wallet, Pubkey::new_from_array([6; 32]));
    }

    #[test]
    fn classifies_buy_v2_with_trader_at_index_13() {
        // buy_v2 layout: trader (fee payer) at account position 13.
        let mut data = PUMP_BUY_V2_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[7u8; 16]);
        let v2_ix = UiCompiledInstruction {
            program_id_index: 0,
            accounts: (0u8..27).collect(),
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let tx = swap_tx(vec![v2_ix]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].side, SwapSide::Buy);
        // account_keys[13] = key(13) in the fabricated tx.
        assert_eq!(records[0].wallet, Pubkey::new_from_array([13; 32]));
        assert_eq!(records[0].amount_base_units, u64::from_le_bytes([7; 8]));
    }

    #[test]
    fn replay_mainnet_buy_v2_swap() {
        // Same frozen mainnet tx as the create_v2 replay: its second
        // top-level instruction is a real `buy_v2` (27 accounts). The
        // trader must resolve to the fee payer with the exact on-chain
        // amount (3_485_709_524_202 base units).
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/create_v2_mainnet.json"))
                .expect("fixture");
        let fetched: EncodedConfirmedTransactionWithStatusMeta =
            serde_json::from_value(raw["result"].clone()).expect("decode");
        let records = extract_pump_swaps(&fetched, "sig");
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1);
        assert_eq!(
            buys[0].wallet.to_string(),
            "DC4X9szDJk2wVm2NAGsshGFydKAahBg6sAAjmxvSJZGh"
        );
        assert_eq!(buys[0].amount_base_units, 3_485_709_524_202);
    }

    #[test]
    fn ignores_non_pump_and_garbage() {
        let mut foreign = buy_ix(5, 6); // program idx 5 = key(5), not pump
        let _ = &mut foreign;
        let mut bad_discr = buy_ix(0, 6);
        bad_discr.data = bs58::encode([0u8; 24]).into_string();
        let tx = swap_tx(vec![foreign, bad_discr]);
        assert!(extract_pump_swaps(&tx, "sig").is_empty());
    }

    #[test]
    fn gate_rejects_bad_dev_address() {
        let mut gate = EntryGate::new(60, 3);
        assert!(!gate.submit("MINT", "not-a-key", 1000));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn gate_confirms_on_quorum() {
        let mut gate = EntryGate::new(60, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        let buyers = [11u8, 12, 13].map(|b| SwapRecord {
            signature: format!("s{b}"),
            slot: 1001,
            wallet: Pubkey::new_from_array([b; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        });
        assert_eq!(gate.observe("MINT", &buyers[..2], 1001), GateOutcome::Wait);
        assert_eq!(
            gate.observe("MINT", &buyers[2..], 1002),
            GateOutcome::Confirm { buyers: 3 }
        );
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn gate_ignores_dev_buys_and_aborts_dev_dump() {
        let mut gate = EntryGate::new(60, 2);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        // Dev buying does not count toward quorum.
        let dev_buy = SwapRecord {
            signature: "s1".to_string(),
            slot: 1001,
            wallet: dev,
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("MINT", &[dev_buy], 1001), GateOutcome::Wait);
        // One real buyer is still below quorum of 2.
        let buy = SwapRecord {
            signature: "s2".to_string(),
            slot: 1002,
            wallet: Pubkey::new_from_array([11; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("MINT", &[buy], 1002), GateOutcome::Wait);
        // Dev selling aborts immediately.
        let dump = SwapRecord {
            signature: "s3".to_string(),
            slot: 1003,
            wallet: dev,
            side: SwapSide::Sell,
            amount_base_units: 1_000,
        };
        match gate.observe("MINT", &[dump], 1003) {
            GateOutcome::Abort { reason } => assert!(reason.contains("dev dump")),
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn gate_times_out_and_sweeps() {
        let mut gate = EntryGate::new(10, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("A", &dev.to_string(), 1000));
        assert!(gate.submit("B", &dev.to_string(), 1000));
        let buy = SwapRecord {
            signature: "s".to_string(),
            slot: 1005,
            wallet: Pubkey::new_from_array([11; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("A", &[buy], 1005), GateOutcome::Wait);
        assert!(matches!(
            gate.observe("A", &[], 1011),
            GateOutcome::Abort { .. }
        ));
        let swept = gate.sweep_timeouts(1011);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0, "B");
        assert_eq!(gate.observe("UNKNOWN", &[], 1011), GateOutcome::Wait);
    }

    #[test]
    fn bundle_organic_launch_is_safe() {
        let swaps = vec![
            (100u64, Pubkey::new_from_array([11; 32]), 2.0),
            (101u64, Pubkey::new_from_array([12; 32]), 1.5),
        ];
        let analysis = BundleDetector::evaluate_launch_slot(100, &swaps, &[]);
        assert!(!analysis.is_bundled);
        assert_eq!(analysis.creation_slot_buys, 1);
        assert_eq!(analysis.verdict, BundleVerdict::SafeOrganic);
    }

    #[test]
    fn bundle_three_creation_slot_buys_is_suspect() {
        let swaps: Vec<(u64, Pubkey, f64)> = (21..=23)
            .map(|b| (200u64, Pubkey::new_from_array([b; 32]), 3.0))
            .collect();
        let analysis = BundleDetector::evaluate_launch_slot(200, &swaps, &[]);
        assert!(analysis.is_bundled);
        assert_eq!(analysis.creation_slot_buys, 3);
        assert_eq!(analysis.verdict, BundleVerdict::SuspectLowVolume);
    }

    #[test]
    fn bundle_monopoly_rejects_over_15_percent() {
        let swaps: Vec<(u64, Pubkey, f64)> = (21..=23)
            .map(|b| (300u64, Pubkey::new_from_array([b; 32]), 6.0))
            .collect();
        let analysis = BundleDetector::evaluate_launch_slot(300, &swaps, &[]);
        assert!(analysis.is_bundled);
        assert!((analysis.total_cluster_supply_pct - 18.0).abs() < 1e-9);
        assert_eq!(analysis.verdict, BundleVerdict::RejectBundledMonopoly);
    }

    #[test]
    fn bundle_supply_boundary_is_strict() {
        // Exactly 15.0% does NOT reject (skill: strictly greater).
        let swaps: Vec<(u64, Pubkey, f64)> = (21..=23)
            .map(|b| (400u64, Pubkey::new_from_array([b; 32]), 5.0))
            .collect();
        let analysis = BundleDetector::evaluate_launch_slot(400, &swaps, &[]);
        assert!(analysis.is_bundled);
        assert_eq!(analysis.verdict, BundleVerdict::SuspectLowVolume);
    }

    #[test]
    fn bundle_shared_funder_proves_coordination() {
        // Different slots, but two buyers share one genesis funder.
        let swaps = vec![
            (500u64, Pubkey::new_from_array([11; 32]), 1.0),
            (501u64, Pubkey::new_from_array([12; 32]), 1.0),
        ];
        let funders = vec![
            (
                Pubkey::new_from_array([11; 32]),
                Pubkey::new_from_array([99; 32]),
            ),
            (
                Pubkey::new_from_array([12; 32]),
                Pubkey::new_from_array([99; 32]),
            ),
            (
                Pubkey::new_from_array([13; 32]),
                Pubkey::new_from_array([98; 32]),
            ),
        ];
        let analysis = BundleDetector::evaluate_launch_slot(500, &swaps, &funders);
        assert!(analysis.is_bundled);
        assert_eq!(analysis.shared_funder_count, 2);
        assert_eq!(analysis.verdict, BundleVerdict::SuspectLowVolume);
    }

    #[test]
    fn bundle_two_same_slot_buys_are_not_enough() {
        let swaps = vec![
            (600u64, Pubkey::new_from_array([11; 32]), 1.0),
            (600u64, Pubkey::new_from_array([12; 32]), 1.0),
        ];
        let analysis = BundleDetector::evaluate_launch_slot(600, &swaps, &[]);
        assert!(!analysis.is_bundled);
        assert_eq!(analysis.verdict, BundleVerdict::SafeOrganic);
    }

    #[test]
    fn bundle_empty_input_is_safe() {
        let analysis = BundleDetector::evaluate_launch_slot(0, &[], &[]);
        assert!(!analysis.is_bundled);
        assert_eq!(analysis.creation_slot_buys, 0);
        assert_eq!(analysis.total_cluster_supply_pct, 0.0);
        assert_eq!(analysis.verdict, BundleVerdict::SafeOrganic);
    }

    #[test]
    fn funding_source_found_in_top_level_transfer() {
        // System program at index 0, from=key(1), to=key(2), 12-byte payload.
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&500u64.to_le_bytes());
        let transfer = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
        for b in 1u8..=4 {
            account_keys.push(key(b));
        }
        let tx = EncodedConfirmedTransactionWithStatusMeta {
            slot: 9,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys,
                        recent_blockhash: key(99),
                        instructions: vec![transfer],
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta: None,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        };
        let beneficiary = Pubkey::new_from_array([2; 32]);
        assert_eq!(
            find_funding_source(&tx, &beneficiary),
            Some(Pubkey::new_from_array([1; 32]))
        );
        // Wrong beneficiary: no match.
        assert_eq!(
            find_funding_source(&tx, &Pubkey::new_from_array([3; 32])),
            None
        );
    }

    #[test]
    fn funding_source_found_in_create_account() {
        // System createAccount: u32 LE 0 ++ 48 bytes, funder idx 0, new idx 1.
        let mut data = 0u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[7u8; 48]);
        let create = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
        for b in 1u8..=4 {
            account_keys.push(key(b));
        }
        let tx = EncodedConfirmedTransactionWithStatusMeta {
            slot: 9,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys,
                        recent_blockhash: key(99),
                        instructions: vec![create],
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta: None,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        };
        assert_eq!(
            find_funding_source(&tx, &Pubkey::new_from_array([2; 32])),
            Some(Pubkey::new_from_array([1; 32]))
        );
    }

    #[test]
    fn swap_records_carry_slot_and_amount() {
        let tx = swap_tx(vec![buy_ix(0, 6)]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].slot, 1);
        assert_eq!(records[0].amount_base_units, u64::from_le_bytes([9; 8]));
    }

    #[test]
    fn timed_cache_hits_misses_and_expires() {
        let mut cache: TimedCache<String> = TimedCache::new(100);
        assert_eq!(cache.get("k", 1000), None);
        cache.put("k".to_string(), "v".to_string(), 1000);
        assert_eq!(cache.get("k", 1000), Some("v".to_string()));
        assert_eq!(cache.get("k", 1100), Some("v".to_string()));
        assert_eq!(cache.get("k", 1101), None);
        assert_eq!(cache.get("other", 1000), None);
    }

    #[test]
    fn initialize2_resolves_amm_at_index_4() {
        // Wire order per raydium-amm `initialize2`: [0]=SPL Token program,
        // [1]=ATA program, [2]=system, [3]=rent, [4]=new AMM pool.
        let amm = Pubkey::new_from_array([77; 32]);
        let other = Pubkey::new_from_array([78; 32]);
        let spl_token = Pubkey::new_from_array([10; 32]);
        let ata_program = Pubkey::new_from_array([11; 32]);
        let system = Pubkey::new_from_array([12; 32]);
        let rent = Pubkey::new_from_array([13; 32]);
        let ixs = vec![
            (
                Pubkey::new_from_array([1; 32]),
                vec![Pubkey::new_from_array([2; 32])],
            ),
            (
                RAYDIUM_V4_PROGRAM_ID,
                vec![spl_token, ata_program, system, rent, amm, other],
            ),
        ];
        assert_eq!(initialize2_amm_id(&ixs), Some(amm));
        // No Raydium instruction: no AMM.
        let plain = vec![(Pubkey::new_from_array([1; 32]), vec![amm])];
        assert_eq!(initialize2_amm_id(&plain), None);
        // Truncated account list (fewer than 5): no AMM.
        let short = vec![(RAYDIUM_V4_PROGRAM_ID, vec![Pubkey::new_from_array([3; 32])])];
        assert_eq!(initialize2_amm_id(&short), None);
    }

    #[test]
    fn migration_report_flattens() {
        let detected = DetectedMigration {
            signature: "sig".to_string(),
            destination: MigrationDestination::PumpSwap,
            amm_id: None,
            pool: None,
        };
        let report = detected.report();
        assert_eq!(report.destination, MigrationDestination::PumpSwap);
        assert_eq!(report.signature, "sig");
        assert_eq!(report.amm_id, None);
    }

    #[test]
    fn replay_real_migrate_v2_fixture() {
        // Real mainnet MigrateV2 (PumpSwap) transaction, fetched once and
        // frozen: proves the live wire shape decodes and classifies.
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/migrate_v2_pumpswap.json"))
                .expect("fixture");
        let fetched: EncodedConfirmedTransactionWithStatusMeta =
            serde_json::from_value(raw["result"].clone()).expect("decode");
        assert_eq!(fetched.slot, 455487811);
        let Some((keys, decoded, logs)) = decode_migration_tx(&fetched) else {
            panic!("fixture must decode");
        };
        assert!(logs.iter().any(|l| l.contains("MigrateV2")));
        let ix_datas: Vec<Vec<u8>> = decoded.iter().map(|ix| ix.data.clone()).collect();
        assert_eq!(
            MigrationMonitor::new().classify_migration(&keys, &ix_datas, &logs),
            Some(MigrationDestination::PumpSwap)
        );
    }

    #[test]
    fn judge_rejects_young_serial_and_accepts_clean() {
        let cfg = DevScreenConfig::default();
        assert!(!judge_developer(None, 0, &cfg).pass);
        let young = judge_developer(Some(600), 0, &cfg);
        assert!(!young.pass);
        assert!(young.reason.contains("600"));
        let serial = judge_developer(Some(7200), 3, &cfg);
        assert!(!serial.pass);
        assert!(serial.reason.contains("3"));
        let clean = judge_developer(Some(7200), 1, &cfg);
        assert!(clean.pass);
    }

    #[test]
    fn funding_source_short_payload_never_panics() {
        // Regression: `raw[..4]` on a <4-byte system ix must return None,
        // not panic (the function promises never to panic).
        for short in [vec![], vec![2u8], vec![2u8, 0, 0]] {
            let ix = UiCompiledInstruction {
                program_id_index: 0,
                accounts: vec![1, 2],
                data: bs58::encode(&short).into_string(),
                stack_height: None,
            };
            let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
            for b in 1u8..=4 {
                account_keys.push(key(b));
            }
            let tx = EncodedConfirmedTransactionWithStatusMeta {
                slot: 9,
                transaction: EncodedTransactionWithStatusMeta {
                    transaction: EncodedTransaction::Json(UiTransaction {
                        signatures: vec!["sig".to_string()],
                        message: UiMessage::Raw(UiRawMessage {
                            header: solana_message::MessageHeader {
                                num_required_signatures: 1,
                                num_readonly_signed_accounts: 0,
                                num_readonly_unsigned_accounts: 1,
                            },
                            account_keys,
                            recent_blockhash: key(99),
                            instructions: vec![ix],
                            address_table_lookups: None,
                            transaction_config: None,
                        }),
                    }),
                    meta: None,
                    version: None,
                },
                block_time: None,
                transaction_index: None,
            };
            assert_eq!(
                find_funding_source(&tx, &Pubkey::new_from_array([2; 32])),
                None,
                "short payload {short:?} must be None"
            );
        }
    }

    #[test]
    fn gate_relaunch_refresh_is_capped() {
        // Regression: spammed relaunches must not extend the window forever.
        let mut gate = EntryGate::new(60, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        // Spam refresh every 10s for a long time: deadline caps at
        // created + 3×window = 1000 + 180 = 1180.
        for t in (1010..10_000).step_by(10) {
            assert!(gate.submit("MINT", &dev.to_string(), t as u64));
        }
        // Past the cap the entry must have timed out.
        assert!(matches!(
            gate.observe("MINT", &[], 1181),
            GateOutcome::Abort { .. }
        ));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn timed_cache_prunes_expired_on_put() {
        let mut cache: TimedCache<String> = TimedCache::new(100);
        cache.put("old".to_string(), "v".to_string(), 1000);
        // Inserting far in the future must evict the stale entry.
        cache.put("new".to_string(), "v".to_string(), 5000);
        assert_eq!(cache.get("old", 5000), None);
        assert_eq!(cache.get("new", 5000), Some("v".to_string()));
    }
}
