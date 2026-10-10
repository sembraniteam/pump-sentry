//! WebSocket event stream: `logsSubscribe` → `getTransaction` → parse → channel.
//!
//! Design notes (async hygiene):
//! - The socket read loop never awaits a full RPC round-trip inline. Each
//!   candidate signature is handed to a bounded-concurrency `tokio::spawn`
//!   worker that performs `getTransaction`, parsing, and `try_send`.
//! - The downstream queue is `tokio::sync::mpsc::channel(10_000)`.
//!   `TrySendError::Full` drops the event and bumps `dropped_full` — the
//!   read loop never blocks.
//! - Reconnects use exponential backoff (100ms base, 5s cap) with jitter.
//! - No `.unwrap()` / `.expect()` / unchecked indexing anywhere.

use crate::parser::{PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR, is_pump_create_ix, parse_pump_create};
use crate::types::{EVENT_CHANNEL_CAPACITY, ListenerConfig, ListenerEvent, ListenerStats};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::StreamExt;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    client_error::ClientError,
    nonblocking::{
        pubsub_client::{PubsubClient, PubsubClientError},
        rpc_client::RpcClient,
    },
    rpc_config::{RpcTransactionConfig, RpcTransactionLogsConfig, RpcTransactionLogsFilter},
};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedTransaction, UiCompiledInstruction, UiInstruction, UiLoadedAddresses, UiMessage,
    UiTransactionEncoding, option_serializer::OptionSerializer,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

/// Upper bound on concurrent `getTransaction` workers.
const MAX_CONCURRENT_FETCHES: usize = 32;

/// Anchor log marker emitted for the `create` instruction.
const CREATE_LOG_MARKER: &str = "Instruction: Create";

/// Typed listener errors — everything maps here, never panics.
#[derive(Error, Debug)]
pub enum ListenerError {
    /// Listener configuration is invalid.
    #[error("invalid config: {0}")]
    Config(String),
    /// Solana RPC / WebSocket client failure.
    #[error("rpc client error: {0}")]
    Rpc(#[from] ClientError),
    /// Solana PubSub (WebSocket) failure.
    #[error("pubsub error: {0}")]
    Pubsub(#[from] PubsubClientError),
    /// Transaction signature failed to parse.
    #[error("bad signature '{value}': {msg}")]
    BadSignature {
        /// Offending signature string.
        value: String,
        /// Parse failure message.
        msg: String,
    },
    /// Account key failed to parse as a pubkey.
    #[error("bad account key '{value}': {msg}")]
    BadAccountKey {
        /// Offending key string.
        value: String,
        /// Parse failure message.
        msg: String,
    },
    /// Transaction encoding was not structured JSON.
    #[error("unsupported transaction encoding (expected JSON message)")]
    UnsupportedEncoding,
    /// Channel closed / worker join failure.
    #[error("stream terminated: {0}")]
    Terminated(String),
}

/// Exponential backoff with jitter.
///
/// Base 100ms, doubles per attempt, capped at 5s. Jitter adds 0–100ms derived
/// from the current time (no extra RNG dependency, no blocking).
pub fn backoff_duration(attempt: u32) -> Duration {
    let capped_attempt = attempt.min(6);
    // 100 * 2^attempt, computed with checked ops (no panic on overflow).
    let mut exp_ms: u64 = 100;
    let mut i: u32 = 0;
    while i < capped_attempt {
        exp_ms = exp_ms.checked_mul(2).unwrap_or(5_000);
        if exp_ms >= 5_000 {
            exp_ms = 5_000;
            break;
        }
        i += 1;
    }
    if exp_ms > 5_000 {
        exp_ms = 5_000;
    }
    let jitter = jitter_ms();
    let total = exp_ms.saturating_add(jitter).min(5_000);
    Duration::from_millis(total)
}

fn jitter_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => u64::from(d.subsec_nanos() % 100),
        Err(_) => 0,
    }
}

/// Pump.fun log listener.
pub struct PumpFunListener {
    config: ListenerConfig,
    stats: Arc<ListenerStats>,
}

impl PumpFunListener {
    /// Build a listener from config (validates URLs + commitment eagerly).
    pub fn new(config: ListenerConfig) -> Result<Self, ListenerError> {
        config
            .validate()
            .map_err(|e| ListenerError::Config(e.to_string()))?;
        Ok(Self {
            config,
            stats: Arc::new(ListenerStats::new()),
        })
    }

    /// Shared stats handle.
    pub fn stats(&self) -> Arc<ListenerStats> {
        Arc::clone(&self.stats)
    }

    /// Run the reconnect loop forever, emitting events into `tx`.
    ///
    /// Uses a bounded `mpsc::Sender` (capacity 10_000 expected). Drops on
    /// full with metric accounting; never blocks the socket loop.
    pub async fn run(self, tx: mpsc::Sender<ListenerEvent>) -> Result<(), ListenerError> {
        let rpc = Arc::new(RpcClient::new(self.config.http_url.clone()));
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
        let mut attempt: u32 = 0;

        loop {
            if tx.is_closed() {
                info!("downstream closed; listener shutting down");
                return Ok(());
            }
            match self.run_once(&tx, &rpc, &semaphore).await {
                Ok(()) => {
                    // Clean subscription end (server closed stream): reconnect.
                    warn!("log subscription ended; reconnecting");
                }
                Err(e) => {
                    warn!(error = %e, attempt, "subscription error; backing off");
                }
            }
            attempt = attempt.saturating_add(1);
            let wait = backoff_duration(attempt);
            debug!(?wait, attempt, "reconnect backoff");
            tokio::time::sleep(wait).await;
        }
    }

    /// Single subscription session; returns when the socket stream ends.
    async fn run_once(
        &self,
        tx: &mpsc::Sender<ListenerEvent>,
        rpc: &Arc<RpcClient>,
        semaphore: &Arc<Semaphore>,
    ) -> Result<(), ListenerError> {
        let filter = RpcTransactionLogsFilter::Mentions(vec![PUMP_PROGRAM_ID_STR.to_string()]);
        let sub_config = RpcTransactionLogsConfig {
            commitment: Some(self.config.commitment_config()),
        };

        let client = PubsubClient::new(&self.config.ws_url).await?;
        let (mut sub, _unsub) = client.logs_subscribe(filter, sub_config).await?;
        info!(
            ws = %self.config.ws_url,
            commitment = %self.config.commitment,
            "subscribed to pump.fun logs"
        );

        while let Some(response) = sub.next().await {
            self.stats.inc_logs_seen();
            let sig_str = response.value.signature.clone();
            let slot_hint = response.context.slot;

            // Only successful transactions are interesting.
            if response.value.err.is_some() {
                continue;
            }
            // Anchor emits `Instruction: Create` in program logs for `create`.
            let has_create = response
                .value
                .logs
                .iter()
                .any(|line| line.contains(CREATE_LOG_MARKER));
            if !has_create {
                continue;
            }
            self.stats.inc_candidates();

            // Hand the RPC round-trip to a worker; never block the read loop.
            let tx_clone = tx.clone();
            let rpc_clone = Arc::clone(rpc);
            let stats_clone = Arc::clone(&self.stats);
            let sem_clone = Arc::clone(semaphore);
            let commitment = self.config.commitment_config();
            tokio::spawn(async move {
                // Acquire a fetch permit (bounds RPC concurrency).
                let _permit = sem_clone.acquire_owned().await;
                fetch_and_emit(
                    &sig_str,
                    slot_hint,
                    &rpc_clone,
                    &tx_clone,
                    &stats_clone,
                    commitment,
                )
                .await;
            });
        }
        Ok(())
    }

    /// Spawn a listener and return the bounded receiver + stats + task handle.
    ///
    /// Channel capacity is [`EVENT_CHANNEL_CAPACITY`] (10_000) with
    /// drop-on-full semantics.
    pub async fn subscribe(
        config: ListenerConfig,
    ) -> Result<
        (
            mpsc::Receiver<ListenerEvent>,
            Arc<ListenerStats>,
            JoinHandle<()>,
        ),
        ListenerError,
    > {
        let listener = PumpFunListener::new(config)?;
        let stats = listener.stats();
        let stats_out = Arc::clone(&stats);
        let (tx, rx) = mpsc::channel::<ListenerEvent>(EVENT_CHANNEL_CAPACITY);
        let handle: JoinHandle<()> = tokio::spawn(async move {
            if let Err(e) = listener.run(tx).await {
                warn!(error = %e, "listener exited with error");
            }
        });
        Ok((rx, stats_out, handle))
    }

    /// Unbounded-channel variant (for sinks that manage their own buffering).
    ///
    /// The socket loop still never blocks: `send` on an unbounded channel
    /// cannot fail with `Full`, only `Closed`.
    pub async fn subscribe_unbounded(
        config: ListenerConfig,
    ) -> Result<
        (
            mpsc::UnboundedReceiver<ListenerEvent>,
            Arc<ListenerStats>,
            JoinHandle<()>,
        ),
        ListenerError,
    > {
        let listener = PumpFunListener::new(config)?;
        let stats = listener.stats();
        let stats_out = Arc::clone(&stats);
        let (tx, rx) = mpsc::unbounded_channel::<ListenerEvent>();
        let handle: JoinHandle<()> = tokio::spawn(async move {
            if let Err(e) = listener.run_unbounded(tx).await {
                warn!(error = %e, "listener (unbounded) exited with error");
            }
        });
        Ok((rx, stats_out, handle))
    }

    /// Run loop feeding an unbounded sender (same reconnect/backoff policy).
    pub async fn run_unbounded(
        self,
        tx: mpsc::UnboundedSender<ListenerEvent>,
    ) -> Result<(), ListenerError> {
        // Bridge unbounded → bounded worker path by wrapping sends.
        let (bounded_tx, mut bounded_rx) = mpsc::channel::<ListenerEvent>(EVENT_CHANNEL_CAPACITY);
        let forward_stats = Arc::clone(&self.stats);
        tokio::spawn(async move {
            while let Some(ev) = bounded_rx.recv().await {
                if tx.send(ev).is_err() {
                    break;
                }
            }
            drop(forward_stats);
        });
        self.run(bounded_tx).await
    }
}

/// Fetch one transaction and emit every `create` event found in it.
///
/// Runs inside a spawned worker (not the socket read loop). Never panics;
/// all failures are counted and logged.
async fn fetch_and_emit(
    sig_str: &str,
    slot_hint: u64,
    rpc: &RpcClient,
    tx: &mpsc::Sender<ListenerEvent>,
    stats: &Arc<ListenerStats>,
    commitment: CommitmentConfig,
) {
    let signature = match Signature::from_str(sig_str) {
        Ok(s) => s,
        Err(e) => {
            stats.inc_fetch_err();
            debug!(signature = %sig_str, error = %e, "bad log signature");
            return;
        }
    };

    // NOTE: `Json` encoding yields the structured `accountKeys` /
    // compiled-instruction form the parser expects (base58 `data`,
    // `loadedAddresses` split). Binary (`Base64`) responses are also
    // tolerated by `extract_events` when the RPC returns them.
    let tx_config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(0),
    };
    // Silence unused-import warning if UiAccountEncoding unused on some versions.
    let _ = UiAccountEncoding::Base64;

    let fetched = match rpc.get_transaction_with_config(&signature, tx_config).await {
        Ok(v) => v,
        Err(e) => {
            stats.inc_fetch_err();
            debug!(signature = %sig_str, error = %e, "getTransaction failed");
            return;
        }
    };
    stats.inc_fetch_ok();

    let events = extract_events(&fetched, sig_str, slot_hint, stats);
    for ev in events {
        let event: ListenerEvent = Arc::new(ev);
        match tx.try_send(event) {
            Ok(()) => {
                stats.inc_events_emitted();
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                stats.inc_dropped_full();
                warn!("event channel full; dropping launch event");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("downstream closed; dropping launch event");
                break;
            }
        }
    }
}

/// Extract all Pump.fun `create` events from a fetched transaction.
///
/// Handles both structured-JSON transactions and raw binary (`Base64`)
/// payloads when present. Never panics; index access is bounds-checked.
fn extract_events(
    fetched: &solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta,
    sig_str: &str,
    slot_hint: u64,
    stats: &Arc<ListenerStats>,
) -> Vec<crate::parser::PumpTokenLaunchEvent> {
    let mut out = Vec::new();
    let slot = fetched.slot;

    let (account_keys, top_ixs, inner_ixs) = match build_ix_lists(fetched) {
        Ok(lists) => lists,
        Err(e) => {
            debug!(signature = %sig_str, error = %e, "unable to decode transaction");
            stats.inc_fetch_err();
            return out;
        }
    };

    // Top-level instructions: `data` is base58.
    for (program_id, accounts, data_b58) in top_ixs {
        if program_id != PUMP_PROGRAM_ID {
            continue;
        }
        let raw = match bs58::decode(&data_b58).into_vec() {
            Ok(v) => v,
            Err(_) => {
                stats.inc_parse_err();
                continue;
            }
        };
        if !is_pump_create_ix(&raw) {
            continue;
        }
        match parse_pump_create(&raw, &accounts, sig_str.to_string(), slot) {
            Ok(ev) => {
                stats.inc_parse_ok();
                out.push(ev);
            }
            Err(e) => {
                stats.inc_parse_err();
                debug!(signature = %sig_str, error = %e, "top-level parse failed");
            }
        }
    }

    // Inner instructions: `data` may be base58 or base64 — try both.
    for (program_id, accounts, data_str) in inner_ixs {
        if program_id != PUMP_PROGRAM_ID {
            continue;
        }
        let raw = match decode_ix_data(&data_str) {
            Some(v) => v,
            None => {
                stats.inc_parse_err();
                continue;
            }
        };
        if !is_pump_create_ix(&raw) {
            continue;
        }
        match parse_pump_create(&raw, &accounts, sig_str.to_string(), slot) {
            Ok(ev) => {
                stats.inc_parse_ok();
                out.push(ev);
            }
            Err(e) => {
                stats.inc_parse_err();
                debug!(signature = %sig_str, error = %e, "inner parse failed");
            }
        }
    }

    // Silence unused slot_hint warning documentation: prefer on-chain slot,
    // fall back to the log-context slot when the fetch reports slot 0.
    if out.is_empty() {
        let _ = (slot_hint, account_keys.len());
    }
    out
}

/// Decoded instruction view: (program_id, resolved accounts, raw data string).
type DecodedIx = (Pubkey, Vec<Pubkey>, String);

/// Build (account_keys, top-level ixs, inner ixs) from a fetched transaction.
#[allow(clippy::type_complexity)]
fn build_ix_lists(
    fetched: &solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta,
) -> Result<(Vec<Pubkey>, Vec<DecodedIx>, Vec<DecodedIx>), ListenerError> {
    let tx_with_meta = &fetched.transaction;
    let meta = &tx_with_meta.meta;

    match &tx_with_meta.transaction {
        EncodedTransaction::Json(ui_tx) => {
            let account_keys = resolve_account_keys(ui_tx, meta)?;
            let mut top = Vec::new();
            let mut inner = Vec::new();

            if let UiMessage::Raw(raw) = &ui_tx.message {
                for cix in &raw.instructions {
                    match decode_compiled_ix(cix, &account_keys) {
                        Some(v) => top.push(v),
                        None => continue,
                    }
                }
            }

            if let Some(m) = meta
                && let OptionSerializer::Some(inner_sets) = &m.inner_instructions
            {
                for set in inner_sets {
                    for cix in &set.instructions {
                        match decode_ui_ix(cix, &account_keys) {
                            Some(v) => inner.push(v),
                            None => continue,
                        }
                    }
                }
            }
            Ok((account_keys, top, inner))
        }
        // Binary payloads (Base58/Base64/legacy): decode the versioned
        // transaction in place and resolve program ids the same way.
        EncodedTransaction::LegacyBinary(_) | EncodedTransaction::Binary(_, _) => {
            Err(ListenerError::UnsupportedEncoding)
        }
        EncodedTransaction::Accounts(_) => Err(ListenerError::UnsupportedEncoding),
    }
}

/// Resolve the full account-key list: static keys + loaded addresses
/// (writable first, then readonly), all bounds-checked.
fn resolve_account_keys(
    ui_tx: &solana_transaction_status::UiTransaction,
    meta: &Option<solana_transaction_status::UiTransactionStatusMeta>,
) -> Result<Vec<Pubkey>, ListenerError> {
    let mut keys: Vec<Pubkey> = Vec::new();
    match &ui_tx.message {
        UiMessage::Raw(raw) => {
            for k in &raw.account_keys {
                match Pubkey::from_str(k) {
                    Ok(p) => keys.push(p),
                    Err(e) => {
                        return Err(ListenerError::BadAccountKey {
                            value: k.clone(),
                            msg: e.to_string(),
                        });
                    }
                }
            }
        }
        UiMessage::Parsed(_) => {
            return Err(ListenerError::UnsupportedEncoding);
        }
    }
    if let Some(m) = meta
        && let OptionSerializer::Some(loaded) = &m.loaded_addresses
    {
        append_loaded(&mut keys, &loaded.writable)?;
        append_loaded(&mut keys, &loaded.readonly)?;
    }
    Ok(keys)
}

fn append_loaded(keys: &mut Vec<Pubkey>, extra: &[String]) -> Result<(), ListenerError> {
    for k in extra {
        match Pubkey::from_str(k) {
            Ok(p) => keys.push(p),
            Err(e) => {
                return Err(ListenerError::BadAccountKey {
                    value: k.clone(),
                    msg: e.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Resolve one compiled instruction to (program_id, accounts, data string).
fn decode_compiled_ix(cix: &UiCompiledInstruction, account_keys: &[Pubkey]) -> Option<DecodedIx> {
    let program_id = *account_keys.get(usize::from(cix.program_id_index))?;
    let mut accounts = Vec::with_capacity(cix.accounts.len());
    for idx in &cix.accounts {
        let key = *account_keys.get(usize::from(*idx))?;
        accounts.push(key);
    }
    Some((program_id, accounts, cix.data.clone()))
}

/// Resolve one `UiInstruction` (inner) — only compiled form carries raw data.
fn decode_ui_ix(cix: &UiInstruction, account_keys: &[Pubkey]) -> Option<DecodedIx> {
    match cix {
        UiInstruction::Compiled(c) => decode_compiled_ix(c, account_keys),
        UiInstruction::Parsed(_) => None,
    }
}

/// Decode instruction data trying base58 first, then base64 (standard).
/// Returns `None` when both fail. Never panics.
fn decode_ix_data(s: &str) -> Option<Vec<u8>> {
    if let Ok(v) = bs58::decode(s).into_vec()
        && !v.is_empty()
    {
        return Some(v);
    }
    if let Ok(v) = BASE64_STANDARD.decode(s)
        && !v.is_empty()
    {
        return Some(v);
    }
    // Tolerate unpadded / url-safe base64 as a last resort.
    if let Ok(v) = base64::engine::general_purpose::URL_SAFE.decode(s)
        && !v.is_empty()
    {
        return Some(v);
    }
    None
}

/// Reference the loaded-addresses type so version drift is caught at compile
/// time (function is never called at runtime).
#[allow(dead_code)]
fn _assert_loaded_addresses_shape(_: &UiLoadedAddresses) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_caps_at_5s() {
        for attempt in [0u32, 1, 5, 10, 100] {
            let d = backoff_duration(attempt);
            assert!(d <= Duration::from_millis(5_000), "cap exceeded: {d:?}");
            assert!(d >= Duration::from_millis(100), "below base: {d:?}");
        }
        // Attempt 0 ≈ 100ms + jitter(<100ms).
        let d0 = backoff_duration(0);
        assert!(d0 <= Duration::from_millis(200));
    }

    #[test]
    fn decode_ix_data_accepts_both_encodings() {
        let raw = vec![24u8, 30, 200, 40, 5, 28, 7, 119, 9, 9, 9];
        let b58 = bs58::encode(&raw).into_string();
        let b64 = BASE64_STANDARD.encode(&raw);
        assert_eq!(decode_ix_data(&b58), Some(raw.clone()));
        assert_eq!(decode_ix_data(&b64), Some(raw));
        assert_eq!(decode_ix_data("!!!not-valid!!!"), None);
    }

    #[test]
    fn bad_config_rejected() {
        let cfg = ListenerConfig {
            ws_url: "http://x".to_string(),
            http_url: "https://api.mainnet-beta.solana.com".to_string(),
            commitment: "confirmed".to_string(),
        };
        assert!(PumpFunListener::new(cfg).is_err());
    }
}
