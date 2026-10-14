//! WebSocket stream: logs to parse to channel.
//!
//! Socket loop never waits on RPC. Workers are bounded.
//! Queue is drop-on-full. Reconnects use capped backoff.

use crate::parser::{
    PUMP_PROGRAM_ID, PUMP_PROGRAM_ID_STR, decode_ix_data, is_pump_create_ix,
    is_pump_create_v2_ix, parse_pump_create, parse_pump_create_v2,
};
use crate::types::{EVENT_CHANNEL_CAPACITY, ListenerConfig, ListenerEvent, ListenerStats};
use futures_util::StreamExt;
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
    EncodedTransaction, UiCompiledInstruction, UiInstruction, UiMessage,
    option_serializer::OptionSerializer,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

/// Max parallel `getTransaction` workers.
const MAX_CONCURRENT_FETCHES: usize = 32;

/// Backfill page per reconnect (newest-first, replayed oldest-first).
const BACKFILL_SIG_LIMIT: usize = 100;

/// Seen-signature cap. Past it, half is evicted.
const SEEN_SIG_CAP: usize = 4_000;

/// Handshake deadline. A hung handshake must reconnect, not stall.
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Launch log markers (matched case-insensitively).
const CREATE_LOG_MARKERS: [&str; 2] = ["instruction: create", "instruction: createv2"];

/// True when a log line announces a launch (v1 or v2).
fn is_launch_log(line: &str) -> bool {
    CREATE_LOG_MARKERS.iter().any(|m| line.contains(m))
}

/// Listener errors. Everything maps here.
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

/// Backoff with jitter: 100ms base, doubles, caps at 5s.
pub fn backoff_duration(attempt: u32) -> Duration {
    let exp_ms = 100u64.saturating_mul(1u64 << attempt.min(6)).min(5_000);
    let total = exp_ms.saturating_add(jitter_ms(100)).min(5_000);
    Duration::from_millis(total)
}

fn jitter_ms(max: u64) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    if max == 0 {
        return 0;
    }
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let seq = CALLS.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    // Spread calls across the window when the clock is stuck.
    nanos
        .wrapping_add(seq.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        % max
}

/// Pump.fun log listener.
pub struct PumpFunListener {
    config: ListenerConfig,
    stats: Arc<ListenerStats>,
    /// Recently seen signatures (bounded dedup).
    seen: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl PumpFunListener {
    /// Build from config (checks URLs and commitment).
    pub fn new(config: ListenerConfig) -> Result<Self, ListenerError> {
        config
            .validate()
            .map_err(|e| ListenerError::Config(e.to_string()))?;
        Ok(Self {
            config,
            stats: Arc::new(ListenerStats::new()),
            seen: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        })
    }

    /// Shared stats handle.
    pub fn stats(&self) -> Arc<ListenerStats> {
        Arc::clone(&self.stats)
    }

    /// Mark a signature seen (bounded memory).
    fn mark_seen(&self, sig: &str) {
        match self.seen.lock() {
            Ok(mut guard) => {
                if guard.len() >= SEEN_SIG_CAP {
                    let drop_n = guard.len() / 2;
                    let victims: Vec<String> =
                        guard.iter().take(drop_n).cloned().collect();
                    for v in victims {
                        guard.remove(&v);
                    }
                }
                guard.insert(sig.to_string());
            }
            Err(_) => debug!("seen-set lock poisoned; dedup skipped"),
        }
    }

    /// Gap backfill after reconnect. Best-effort and bounded.
    /// Skipped on first boot.
    async fn backfill_gap(
        &self,
        tx: &mpsc::Sender<ListenerEvent>,
        rpc: &Arc<RpcClient>,
        semaphore: &Arc<Semaphore>,
    ) {
        let Some(commitment) = self.config.commitment_config() else {
            debug!("unknown commitment; skipping backfill");
            return;
        };
        let pump = match Pubkey::from_str(PUMP_PROGRAM_ID_STR) {
            Ok(p) => p,
            Err(_) => return,
        };
        let sigs = match tokio::time::timeout(
            Duration::from_secs(30),
            rpc.get_signatures_for_address_with_config(
                &pump,
                solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(BACKFILL_SIG_LIMIT),
                    commitment: Some(commitment),
                },
            ),
        )
        .await
        {
            Ok(Ok(sigs)) => sigs,
            Ok(Err(e)) => {
                warn!(error = %e, "backfill signature page failed; gap stays");
                return;
            }
            Err(_) => {
                warn!("backfill signature page timed out; gap stays");
                return;
            }
        };
        // Oldest first: quorum follows chain order.
        let mut missed = 0u32;
        for info in sigs.iter().rev() {
            if info.err.is_some() {
                continue;
            }
            let fresh = match self.seen.lock() {
                Ok(guard) => !guard.contains(&info.signature),
                Err(_) => true,
            };
            if !fresh {
                continue;
            }
            self.mark_seen(&info.signature);
            missed += 1;
            let tx_clone = tx.clone();
            let rpc_clone = Arc::clone(rpc);
            let stats_clone = Arc::clone(&self.stats);
            let sem_clone = Arc::clone(semaphore);
            let seen_clone = Arc::clone(&self.seen);
            let sig = info.signature.clone();
            let slot = info.slot;
            let Some(commitment) = self.config.commitment_config() else {
                continue;
            };
            tokio::spawn(async move {
                fetch_and_emit(
                    &sig, slot, &rpc_clone, &tx_clone, &stats_clone, &sem_clone, commitment,
                    seen_clone,
                )
                .await;
            });
        }
        if missed > 0 {
            warn!(missed, "backfilled socket-gap launches (reconnect)");
        } else {
            debug!("backfill: no missed launches in page");
        }
    }

    /// Run the reconnect loop. Drop-on-full channel.
    pub async fn run(self, tx: mpsc::Sender<ListenerEvent>) -> Result<(), ListenerError> {
        let rpc = Arc::new(RpcClient::new(self.config.http_url.clone()));
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
        let mut attempt: u32 = 0;
        // First session starts live-only; later sessions backfill the gap.
        let mut first_session = true;

        loop {
            if tx.is_closed() {
                info!("downstream closed; listener shutting down");
                return Ok(());
            }
            if !first_session {
                self.backfill_gap(&tx, &rpc, &semaphore).await;
                if tx.is_closed() {
                    info!("downstream closed during backfill; listener shutting down");
                    return Ok(());
                }
            }
            first_session = false;
            let seen_before = self.stats.logs_seen();
            let session_start = std::time::Instant::now();
            let outcome = self.run_once(&tx, &rpc, &semaphore).await;
            // Long healthy sessions reset backoff.
            let healthy = self.stats.logs_seen().saturating_sub(seen_before) > 1000
                || session_start.elapsed() > std::time::Duration::from_secs(30);
            match outcome {
                Ok(()) => {
                    // Clean subscription end (server closed stream): reconnect.
                    warn!("log subscription ended; reconnecting");
                }
                Err(e) => {
                    warn!(error = %e, attempt, "subscription error; backing off");
                }
            }
            attempt = if healthy { 0 } else { attempt.saturating_add(1) };
            let wait = backoff_duration(attempt);
            debug!(?wait, attempt, "reconnect backoff");
            tokio::time::sleep(wait).await;
        }
    }

    /// One subscription session. Returns when the stream ends.
    async fn run_once(
        &self,
        tx: &mpsc::Sender<ListenerEvent>,
        rpc: &Arc<RpcClient>,
        semaphore: &Arc<Semaphore>,
    ) -> Result<(), ListenerError> {
        let filter = RpcTransactionLogsFilter::Mentions(vec![PUMP_PROGRAM_ID_STR.to_string()]);
        let Some(commitment) = self.config.commitment_config() else {
            return Err(ListenerError::Config(format!(
                "unknown commitment '{}'",
                self.config.commitment
            )));
        };
        let sub_config = RpcTransactionLogsConfig {
            commitment: Some(commitment),
        };

        let client = match tokio::time::timeout(
            WS_HANDSHAKE_TIMEOUT,
            PubsubClient::new(&self.config.ws_url),
        )
        .await
        {
            Ok(Ok(client)) => client,
            Ok(Err(e)) => return Err(ListenerError::Pubsub(e)),
            Err(_) => {
                return Err(ListenerError::Terminated(
                    "websocket connect timed out".to_string(),
                ));
            }
        };
        let (mut sub, _unsub) = match tokio::time::timeout(
            WS_HANDSHAKE_TIMEOUT,
            client.logs_subscribe(filter, sub_config),
        )
        .await
        {
            Ok(Ok(sub)) => sub,
            Ok(Err(e)) => return Err(ListenerError::Pubsub(e)),
            Err(_) => {
                return Err(ListenerError::Terminated("logsSubscribe timed out".to_string()));
            }
        };
        info!(
            ws = %self.config.ws_url,
            commitment = %self.config.commitment,
            "subscribed to pump.fun logs"
        );

        while let Some(response) = sub.next().await {
            // Stop spawning workers for a dead channel.
            if tx.is_closed() {
                info!("downstream closed mid-session; ending subscription");
                break;
            }
            self.stats.inc_logs_seen();
            let sig_str = response.value.signature.clone();
            let slot_hint = response.context.slot;
            self.mark_seen(&sig_str);

            if response.value.err.is_some() {
                continue;
            }
            // Anchor logs `Instruction: Create` for launches.
            let has_create = response
                .value
                .logs
                .iter()
                .any(|line| is_launch_log(&line.to_ascii_lowercase()));
            if !has_create {
                continue;
            }
            self.stats.inc_candidates();

            // Workers do the RPC. Never block the read loop.
            let tx_clone = tx.clone();
            let rpc_clone = Arc::clone(rpc);
            let stats_clone = Arc::clone(&self.stats);
            let sem_clone = Arc::clone(semaphore);
            let seen_clone = Arc::clone(&self.seen);
            let Some(commitment) = self.config.commitment_config() else {
                // Validated at subscribe(); never coerce silently.
                debug!(signature = %sig_str, "unknown commitment; dropping candidate");
                continue;
            };
            tokio::spawn(async move {
                fetch_and_emit(
                    &sig_str,
                    slot_hint,
                    &rpc_clone,
                    &tx_clone,
                    &stats_clone,
                    &sem_clone,
                    commitment,
                    seen_clone,
                )
                .await;
            });
        }
        Ok(())
    }

    /// Spawn a listener. Returns receiver, stats, and task handle.
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
        let (tx, rx) = mpsc::channel::<ListenerEvent>(EVENT_CHANNEL_CAPACITY.max(1));
        let handle: JoinHandle<()> = tokio::spawn(async move {
            if let Err(e) = listener.run(tx).await {
                warn!(error = %e, "listener exited with error");
            }
        });
        Ok((rx, stats_out, handle))
    }
}

/// Fetch one tx with retries. Permits cover live RPCs only.
/// Third attempt (1s) is for `429` alone. `None` when terminal.
async fn fetch_tx_permit_bound(
    rpc: &RpcClient,
    signature: &Signature,
    tx_config: RpcTransactionConfig,
    sem: &Arc<Semaphore>,
    stats: &Arc<ListenerStats>,
    sig_str: &str,
) -> Option<solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta> {
    let acquire = |why: &str| {
        match sem.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                stats.inc_dropped_throttled();
                // Sustained throttling blinds the sniper during bursts.
                warn!(signature = %sig_str, why, "fetch saturated; dropping candidate");
                None
            }
        }
    };
    let permit = acquire("first attempt")?;
    let first = rpc.get_transaction_with_config(signature, tx_config).await;
    drop(permit);
    match first {
        Ok(v) => Some(v),
        Err(e) if is_transient_fetch_error(&e) => {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let _permit = acquire("retry")?;
            match rpc.get_transaction_with_config(signature, tx_config).await {
                Ok(v) => Some(v),
                // `429` gets one longer retry: 250ms often lands in-window.
                Err(e) if is_rate_limited(&e) => {
                    let jitter = jitter_ms(500);
                    tokio::time::sleep(std::time::Duration::from_millis(1000 + jitter)).await;
                    let _permit = acquire("rate-limit retry")?;
                    match rpc.get_transaction_with_config(signature, tx_config).await {
                        Ok(v) => Some(v),
                        Err(e) => {
                            stats.inc_fetch_err();
                            debug!(signature = %sig_str, error = %e, "getTransaction rate-limit retry failed");
                            None
                        }
                    }
                }
                Err(e) => {
                    stats.inc_fetch_err();
                    debug!(signature = %sig_str, error = %e, "getTransaction retry failed");
                    None
                }
            }
        }
        Err(e) => {
            stats.inc_fetch_err();
            debug!(signature = %sig_str, error = %e, "getTransaction failed");
            None
        }
    }
}

/// True for rate limits (`429`). Worth a longer backoff.
fn is_rate_limited(e: &ClientError) -> bool {    use solana_client::client_error::ClientErrorKind;
    match e.kind() {
        ClientErrorKind::RpcError(solana_client::rpc_request::RpcError::RpcResponseError {
            code: 429,
            ..
        }) => true,
        ClientErrorKind::Reqwest(e) => e.status().is_some_and(|s| s.as_u16() == 429),
        _ => false,
    }
}

/// True for failures worth one retry.
/// Retried: I/O, timeouts, connects, `-32005`, `-32007`, `-32009`, `429`.
fn is_transient_fetch_error(e: &ClientError) -> bool {
    use solana_client::client_error::ClientErrorKind;
    match e.kind() {
        ClientErrorKind::Io(_) => true,
        ClientErrorKind::Reqwest(e) => {
            e.is_timeout() || e.is_connect() || e.status().is_some_and(|s| s.as_u16() == 429)
        }
        ClientErrorKind::RpcError(solana_client::rpc_request::RpcError::RpcResponseError {
            code: -32005 | -32007 | -32009 | 429,
            ..
        }) => true,
        ClientErrorKind::Middleware(_)
        | ClientErrorKind::RpcError(_)
        | ClientErrorKind::SerdeJson(_)
        | ClientErrorKind::SigningError(_)
        | ClientErrorKind::TransactionError(_)
        | ClientErrorKind::Custom(_) => false,
    }
}

/// Fetch one tx and emit every `create` found. Runs in a worker.
/// Fetch failures and full-channel drops unmark the sig for backfill.
/// Bad bytes stay marked: retrying them is useless.
#[allow(clippy::too_many_arguments)]
async fn fetch_and_emit(
    sig_str: &str,
    slot_hint: u64,
    rpc: &RpcClient,
    tx: &mpsc::Sender<ListenerEvent>,
    stats: &Arc<ListenerStats>,
    sem: &Arc<Semaphore>,
    commitment: CommitmentConfig,
    seen: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) {
    let unmark = |why: &str| {
        match seen.lock() {
            Ok(mut guard) => {
                guard.remove(sig_str);
            }
            Err(_) => debug!(signature = %sig_str, why, "seen-set lock poisoned; unmark skipped"),
        }
    };
    let signature = match Signature::from_str(sig_str) {
        Ok(s) => s,
        Err(e) => {
            stats.inc_fetch_err();
            debug!(signature = %sig_str, error = %e, "bad log signature");
            return;
        }
    };

    // Shared JSON fetch shape. Binary payloads rejected below.
    let tx_config = crate::screen::json_tx_config(commitment);

    // Hung RPCs can't hold permits forever.
    let fetched = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fetch_tx_permit_bound(rpc, &signature, tx_config, sem, stats, sig_str),
    )
    .await    {
        Ok(Some(v)) => v,
        Ok(None) => {
            unmark("fetch failed; backfill may retry");
            return;
        }
        Err(_) => {
            stats.inc_fetch_err();
            debug!(signature = %sig_str, "getTransaction timed out");
            unmark("fetch timed out; backfill may retry");
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
                unmark("channel full; backfill may retry");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("downstream closed; dropping launch event");
                break;
            }
        }
    }
}

/// Extract all `create` events from a fetched tx.
/// Covers top-level and inner ixs.
fn extract_events(
    fetched: &solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta,
    sig_str: &str,
    slot_hint: u64,
    stats: &Arc<ListenerStats>,
) -> Vec<crate::parser::PumpTokenLaunchEvent> {
    let mut out = Vec::new();
    // Chain slot wins. Falls back to log slot on 0.
    let slot = if fetched.slot == 0 { slot_hint } else { fetched.slot };

    let (_account_keys, top_ixs, inner_ixs) = match build_ix_lists(fetched) {
        Ok(lists) => lists,
        Err(e) => {
            debug!(signature = %sig_str, error = %e, "unable to decode transaction");
            // Decode-layer failure (bad keys/encoding), not a fetch
            // failure: count it where it belongs so fetch_ok stays clean.
            stats.inc_parse_err();
            return out;
        }
    };

    // Top and inner ixs share one path.
    for (program_id, accounts, data_b58) in top_ixs {
        if program_id != PUMP_PROGRAM_ID {
            continue;
        }
        let raw = match decode_ix_data(&data_b58) {
            Some(v) => v,
            None => {
                stats.inc_parse_err();
                continue;
            }
        };
        match parse_create_event(&raw, &accounts, sig_str, slot) {
            Ok(Some(ev)) => {
                stats.inc_parse_ok();
                out.push(ev);
            }
            Ok(None) => {}
            Err(e) => {
                stats.inc_parse_err();
                debug!(signature = %sig_str, error = %e, "top-level create parse failed");
            }
        }
    }

    // Inner instructions decode through the same shared helper.
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
        match parse_create_event(&raw, &accounts, sig_str, slot) {
            Ok(Some(ev)) => {
                stats.inc_parse_ok();
                out.push(ev);
            }
            Ok(None) => {}
            Err(e) => {
                stats.inc_parse_err();
                debug!(signature = %sig_str, error = %e, "inner create parse failed");
            }
        }
    }

    // Slot observability for the fallback.
    debug!(signature = %sig_str, slot, slot_hint, "extracted events");
    out
}

/// Classify one decoded ix: `Some` on launch, `None` on other ixs.
fn parse_create_event(
    raw: &[u8],
    accounts: &[Pubkey],
    sig: &str,
    slot: u64,
) -> Result<Option<crate::parser::PumpTokenLaunchEvent>, crate::parser::PumpParseError> {
    if is_pump_create_v2_ix(raw) {
        parse_pump_create_v2(raw, accounts, sig.to_string(), slot).map(Some)
    } else if is_pump_create_ix(raw) {
        parse_pump_create(raw, accounts, sig.to_string(), slot).map(Some)
    } else {
        Ok(None)
    }
}

/// Decoded ix: (program, accounts, raw data).
type DecodedIx = (Pubkey, Vec<Pubkey>, String);

/// Build (keys, top ixs, inner ixs) from a fetched tx.
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
            } else if meta.is_some() {
                tracing::trace!("inner instructions absent (Skip/None); top-level only");
            }
            Ok((account_keys, top, inner))
        }
        EncodedTransaction::LegacyBinary(_) | EncodedTransaction::Binary(_, _) => {
            Err(ListenerError::UnsupportedEncoding)
        }
        EncodedTransaction::Accounts(_) => Err(ListenerError::UnsupportedEncoding),
    }
}

/// Resolve full key list: static keys plus loaded addresses.
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

/// Resolve one compiled ix. Bad indexes skip at debug.
fn decode_compiled_ix(cix: &UiCompiledInstruction, account_keys: &[Pubkey]) -> Option<DecodedIx> {
    let program_id = match account_keys.get(usize::from(cix.program_id_index)) {
        Some(p) => *p,
        None => {
            debug!(program_id_index = cix.program_id_index, keys = account_keys.len(), "compiled ix program index out of bounds");
            return None;
        }
    };
    let mut accounts = Vec::with_capacity(cix.accounts.len());
    for idx in &cix.accounts {
        match account_keys.get(usize::from(*idx)) {
            Some(key) => accounts.push(*key),
            None => {
                debug!(index = idx, keys = account_keys.len(), "compiled ix account index out of bounds");
                return None;
            }
        }
    }
    Some((program_id, accounts, cix.data.clone()))
}

/// Resolve one inner ix. Only compiled form has raw data.
fn decode_ui_ix(cix: &UiInstruction, account_keys: &[Pubkey]) -> Option<DecodedIx> {
    match cix {
        UiInstruction::Compiled(c) => decode_compiled_ix(c, account_keys),
        UiInstruction::Parsed(_) => {
            tracing::trace!("skipping parsed-form inner instruction");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
    use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;

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
    fn launch_log_marker_covers_both_versions() {
        // Anchor PascalCases the log; the read loop lowercases first.
        for line in [
            "program log: instruction: create",
            "program log: instruction: createv2",
            "program log: instruction: create_v2",
        ] {
            assert!(is_launch_log(line), "missed: {line}");
        }
        assert!(!is_launch_log("program log: instruction: buy"));
        assert!(!is_launch_log("program log: instruction: sell"));
        assert!(!is_launch_log(""));
    }

    #[test]
    fn bad_config_rejected() {
        let cfg = ListenerConfig {
            ws_url: "http://x".to_string(),
            http_url: crate::types::DEFAULT_HTTP_RPC_URL.to_string(),
            commitment: "confirmed".to_string(),
        };
        assert!(PumpFunListener::new(cfg).is_err());
    }

    #[test]
    fn seen_set_dedups_and_bounds_memory() {
        let cfg = ListenerConfig {
            ws_url: crate::types::DEFAULT_WS_RPC_URL.to_string(),
            http_url: crate::types::DEFAULT_HTTP_RPC_URL.to_string(),
            commitment: "confirmed".to_string(),
        };
        let listener = PumpFunListener::new(cfg).expect("config");
        listener.mark_seen("sig-a");
        listener.mark_seen("sig-a");
        assert_eq!(listener.seen.lock().expect("lock").len(), 1);
        for i in 0..(SEEN_SIG_CAP + 10) {
            listener.mark_seen(&format!("sig-{i}"));
        }
        assert!(listener.seen.lock().expect("lock").len() <= SEEN_SIG_CAP + 10);
    }

    #[test]
    fn replay_real_create_v2_fixture() {
        // Real mainnet create_v2: proves the v2 wire shape decodes and
        // emits exactly one launch event with the IDL account mapping.
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/create_v2_mainnet.json"))
                .expect("fixture");
        let fetched: EncodedConfirmedTransactionWithStatusMeta =
            serde_json::from_value(raw["result"].clone()).expect("decode");
        assert_eq!(fetched.slot, 455573900);
        let stats = Arc::new(ListenerStats::new());
        let events = extract_events(&fetched, "frozen-sig", 455573900, &stats);
        assert_eq!(events.len(), 1, "one v2 launch expected");
        let ev = &events[0];
        assert_eq!(ev.name, "makemerich");
        assert_eq!(ev.symbol, "mmrich");
        assert!(!ev.uri.is_empty());
        assert_eq!(ev.slot, 455573900);
        // IDL mapping: mint ends in "pump", user is the fee payer (keys[0]).
        assert!(ev.mint.to_string().ends_with("pump"));
        assert_ne!(ev.mint, ev.user);
        assert_ne!(ev.bonding_curve, ev.mint);
    }

    #[tokio::test]
    async fn http_429_maps_to_rate_limited_transient() {
        // Provider-proxy 429 surfaces as `Reqwest` with HTTP status 429;
        // both classifiers must still see it as rate-limited/transient.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            // Initial + 5 internal `HttpSender` retries = 6 hits.
            for _ in 0..8 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                // `Retry-After: 0` keeps the sender's internal 429 retries instant.
                let head = "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 0\r\nRetry-After: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let rpc = RpcClient::new(format!("http://{addr}"));
        let err = rpc.get_slot().await.expect_err("must 429");
        assert!(
            is_rate_limited(&err),
            "HTTP 429 must classify as rate-limited: {err:?}"
        );
        assert!(
            is_transient_fetch_error(&err),
            "HTTP 429 must classify as transient: {err:?}"
        );
    }
}
