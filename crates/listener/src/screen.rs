//! Entry confirmation gate + developer screening (win-rate hardening).
//!
//! - [`extract_pump_swaps`]: classify Pump.fun buy/sell instructions in a
//!   fetched transaction into [`SwapRecord`]s.
//! - [`EntryGate`]: pure watchlist state machine — submit on signal,
//!   confirm on distinct-buyer quorum, abort on dev dump or timeout.
//! - [`screen_developer`]: RPC dev-wallet history (age + prior launches),
//!   fail-closed on transport errors.

use crate::parser::{PUMP_CREATE_DISCRIMINATOR, PUMP_PROGRAM_ID};
use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient,
    rpc_client::GetConfirmedSignaturesForAddress2Config, rpc_config::RpcTransactionConfig,
};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, UiInstruction, UiMessage,
    UiTransactionEncoding, option_serializer::OptionSerializer,
};
use std::{collections::HashMap, collections::HashSet, str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
/// Buy discriminator: sha256(`global:buy`)[..8] (skill solana-pumpfun-swap).
pub const PUMP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// Sell discriminator: sha256(`global:sell`)[..8].
pub const PUMP_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
/// Account index of the trader (`user`) in buy/sell instructions.
pub const IDX_SWAP_USER: usize = 6;
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
/// Non-pump instructions, undecodable data, and short account lists are
/// skipped. Never panics.
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

    let mut keys: Vec<Pubkey> = Vec::with_capacity(raw.account_keys.len());
    for key in &raw.account_keys {
        let Ok(pubkey) = Pubkey::from_str(key) else {
            return out;
        };
        keys.push(pubkey);
    }
    if let Some(meta) = &tx_with_meta.meta
        && let OptionSerializer::Some(loaded) = &meta.loaded_addresses
    {
        for key in loaded.writable.iter().chain(loaded.readonly.iter()) {
            let Ok(pubkey) = Pubkey::from_str(key) else {
                return out;
            };
            keys.push(pubkey);
        }
    }

    let mut classify = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        let program = keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default();
        if program != PUMP_PROGRAM_ID {
            return;
        }
        let Ok(raw) = bs58::decode(data_b58).into_vec() else {
            return;
        };
        let side = if raw.starts_with(&PUMP_BUY_DISCRIMINATOR) {
            SwapSide::Buy
        } else if raw.starts_with(&PUMP_SELL_DISCRIMINATOR) {
            SwapSide::Sell
        } else {
            return;
        };
        let Some(account) = accounts.get(IDX_SWAP_USER) else {
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

    for cix in &raw.instructions {
        classify(cix.program_id_index, &cix.accounts, &cix.data);
    }
    if let Some(meta) = &tx_with_meta.meta
        && let OptionSerializer::Some(sets) = &meta.inner_instructions
    {
        for set in sets {
            for cix in &set.instructions {
                if let UiInstruction::Compiled(compiled) = cix {
                    classify(
                        compiled.program_id_index,
                        &compiled.accounts,
                        &compiled.data,
                    );
                }
            }
        }
    }
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
    let mut keys: Vec<Pubkey> = Vec::new();
    for key in &raw.account_keys {
        let Ok(pubkey) = Pubkey::from_str(key) else {
            return 0;
        };
        keys.push(pubkey);
    }

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
        let Ok(raw) = bs58::decode(data_b58).into_vec() else {
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
    for cix in &raw.instructions {
        check(cix.program_id_index, &cix.accounts, &cix.data);
    }
    if let Some(meta) = &tx_with_meta.meta
        && let OptionSerializer::Some(sets) = &meta.inner_instructions
    {
        for set in sets {
            for cix in &set.instructions {
                if let UiInstruction::Compiled(compiled) = cix {
                    check(
                        compiled.program_id_index,
                        &compiled.accounts,
                        &compiled.data,
                    );
                }
            }
        }
    }
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

    /// Submit a fresh signal (idempotent per mint). Returns false when
    /// the dev address is unparsable (fail-closed: caller must skip).
    pub fn submit(&mut self, mint: &str, dev: &str, now_secs: u64) -> bool {
        let Ok(dev) = Pubkey::from_str(dev) else {
            return false;
        };
        self.pending.entry(mint.to_string()).or_insert(PendingGate {
            dev,
            deadline_secs: now_secs.saturating_add(self.window_secs),
            buyers: HashSet::new(),
        });
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

    let tx_config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(0),
    };
    let mut prior_launches = 0usize;
    for status in sigs.iter().take(cfg.sample_txs) {
        let Ok(signature) = Signature::from_str(&status.signature) else {
            continue;
        };
        // Best-effort sampling: unreadable transactions simply don't count
        // (the count is a lower bound; age gate carries the strictness).
        let Ok(fetched) = rpc.get_transaction_with_config(&signature, tx_config).await else {
            continue;
        };
        prior_launches += count_dev_creates(&fetched, dev);
        if prior_launches > cfg.max_prior_launches {
            break;
        }
    }
    Ok(judge_developer(age_secs, prior_launches, cfg))
}

/// RPC-backed swap reader for one curve: signatures since `since_sig`
/// (newest-first walk, stop at the watermark), classified records.
/// Returns `(records, newest_signature)`.
pub async fn read_curve_swaps(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    curve: &Pubkey,
    since_sig: Option<&str>,
) -> Result<(Vec<SwapRecord>, Option<String>), ClientError> {
    let sigs = rpc
        .get_signatures_for_address_with_config(
            curve,
            GetConfirmedSignaturesForAddress2Config {
                before: None,
                until: None,
                limit: Some(25),
                commitment: Some(commitment),
            },
        )
        .await?;
    let tx_config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(0),
    };
    let mut records = Vec::new();
    let mut newest: Option<String> = None;
    for status in &sigs {
        if Some(status.signature.as_str()) == since_sig {
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
        let Ok(fetched) = rpc.get_transaction_with_config(&signature, tx_config).await else {
            continue;
        };
        records.extend(extract_pump_swaps(&fetched, &status.signature));
    }
    Ok((records, newest))
}

/// Poll interval floor for gate rounds (not a tight loop: RPC budget).
pub const GATE_POLL_INTERVAL: Duration = Duration::from_millis(2000);

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

/// RPC-backed screening client: developer verdicts plus incremental
/// curve-swap reads with a per-mint signature watermark.
#[derive(Clone)]
pub struct RpcScreener {
    rpc: Arc<RpcClient>,
    commitment: CommitmentConfig,
    dev_cfg: DevScreenConfig,
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
        })
    }

    /// Developer history verdict (fail-closed on transport errors).
    pub async fn dev_verdict(
        &self,
        dev: &Pubkey,
        now_secs: u64,
    ) -> Result<DevVerdict, ClientError> {
        screen_developer(&self.rpc, self.commitment, dev, now_secs, &self.dev_cfg).await
    }

    /// Swap records for a curve since `since_sig`, plus the new watermark.
    pub async fn curve_swaps(
        &self,
        curve: &Pubkey,
        since_sig: Option<&str>,
    ) -> Result<(Vec<SwapRecord>, Option<String>), ClientError> {
        read_curve_swaps(&self.rpc, self.commitment, curve, since_sig).await
    }

    /// Full bundle forensics for a curve at confirm time: recent buy
    /// swaps with slots and supply shares, genesis funders for the first
    /// few distinct buyers, evaluated against `creation_slot`.
    /// Transport errors propagate (fail-closed upstream).
    pub async fn bundle_verdict(
        &self,
        curve: &Pubkey,
        creation_slot: u64,
    ) -> Result<BundleAnalysis, ClientError> {
        let (records, _) = read_curve_swaps(&self.rpc, self.commitment, curve, None).await?;
        let total_supply = self.curve_total_supply(curve).await.unwrap_or(0);

        // Distinct buyers, first-seen order, bounded (RPC budget).
        let mut buyers: Vec<Pubkey> = Vec::new();
        for record in records.iter().filter(|r| r.side == SwapSide::Buy) {
            if !buyers.contains(&record.wallet) {
                buyers.push(record.wallet);
            }
            if buyers.len() >= MAX_TRACED_BUYERS {
                break;
            }
        }

        let mut per_buyer_amount: HashMap<Pubkey, u64> = HashMap::new();
        for record in records.iter().filter(|r| r.side == SwapSide::Buy) {
            *per_buyer_amount.entry(record.wallet).or_default() = per_buyer_amount
                .get(&record.wallet)
                .copied()
                .unwrap_or(0)
                .saturating_add(record.amount_base_units);
        }

        let mut swaps: Vec<(u64, Pubkey, f64)> = Vec::new();
        for buyer in &buyers {
            let amount = per_buyer_amount.get(buyer).copied().unwrap_or(0);
            let pct = if total_supply > 0 {
                amount as f64 / total_supply as f64 * 100.0
            } else {
                0.0
            };
            // Slot of the buyer's first observed swap.
            let slot = records
                .iter()
                .find(|r| r.side == SwapSide::Buy && r.wallet == *buyer)
                .map(|r| r.slot)
                .unwrap_or(u64::MAX);
            swaps.push((slot, *buyer, pct));
        }

        let mut funding_parents: Vec<(Pubkey, Pubkey)> = Vec::new();
        for buyer in &buyers {
            if let Some(funder) = self.genesis_funder(buyer).await {
                funding_parents.push((*buyer, funder));
            }
        }

        Ok(BundleDetector::evaluate_launch_slot(
            creation_slot,
            &swaps,
            &funding_parents,
        ))
    }

    /// Total token supply from the curve account (`token_total_supply`
    /// u64 at offset 40). Zero when unreadable (supply rule degrades to
    /// slot+funder signals only).
    async fn curve_total_supply(&self, curve: &Pubkey) -> Result<u64, ClientError> {
        let account = self.rpc.get_account(curve).await?;
        let supply = account
            .data
            .get(40..48)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0);
        Ok(supply)
    }

    /// Genesis funder: source of the first SOL transfer into `buyer`
    /// across its most recent signatures. `None` when untraceable.
    /// Genesis funder: walk the wallet's full signature history
    /// (newest-first pages of 1000, up to [`MAX_FUNDER_PAGES`]) down to
    /// its first transaction, then return the source of the first SOL
    /// funding found there (oldest few tried in order). `None` when the
    /// history is truncated by the page cap or no funding parses.
    async fn genesis_funder(&self, buyer: &Pubkey) -> Option<Pubkey> {
        let mut before: Option<Signature> = None;
        let mut oldest_page: Vec<(String, bool)> = Vec::new();
        for _ in 0..MAX_FUNDER_PAGES {
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
        let tx_config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(self.commitment),
            max_supported_transaction_version: Some(0),
        };
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
/// Signature pages walked when seeking a wallet's first transaction
/// (1000 signatures per page).
pub const MAX_FUNDER_PAGES: usize = 10;
/// Signatures kept from the oldest page for funding fallbacks.
const FUNDER_TRAIL: usize = 4;

/// System program (native SOL transfers).
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

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
    let mut keys: Vec<Pubkey> = Vec::new();
    for key in &raw.account_keys {
        let Ok(pubkey) = Pubkey::from_str(key) else {
            return None;
        };
        keys.push(pubkey);
    }

    let scan = |program_id_index: u8, accounts: &[u8], data_b58: &str| -> Option<Pubkey> {
        if keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default()
            != SYSTEM_PROGRAM_ID
        {
            return None;
        }
        let Ok(raw) = bs58::decode(data_b58).into_vec() else {
            return None;
        };
        // System transfer (u32 LE 2, 12 bytes, [from, to]) or account
        // creation (u32 LE 0, 52 bytes, [funder, new]).
        let (from_idx, to_idx) = match (raw.len(), u32::from_le_bytes(raw[..4].try_into().ok()?)) {
            (12, 2) | (52, 0) => (0usize, 1usize),
            _ => return None,
        };
        let from = keys.get(usize::from(*accounts.get(from_idx)?)).copied()?;
        let to = keys.get(usize::from(*accounts.get(to_idx)?)).copied()?;
        (to == *beneficiary).then_some(from)
    };

    for cix in &raw.instructions {
        if let Some(funder) = scan(cix.program_id_index, &cix.accounts, &cix.data) {
            return Some(funder);
        }
    }
    if let Some(meta) = &tx_with_meta.meta
        && let OptionSerializer::Some(sets) = &meta.inner_instructions
    {
        for set in sets {
            for cix in &set.instructions {
                if let UiInstruction::Compiled(compiled) = cix
                    && let Some(funder) = scan(
                        compiled.program_id_index,
                        &compiled.accounts,
                        &compiled.data,
                    )
                {
                    return Some(funder);
                }
            }
        }
    }
    None
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
}
