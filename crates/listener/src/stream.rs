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
    EncodedTransaction, UiCompiledInstruction, UiInstruction, UiMessage, UiTransactionEncoding,
    option_serializer::OptionSerializer,
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

/// Backfill page: signatures pulled per reconnect to cover the socket
/// gap (newest-first from RPC, replayed oldest-first).
const BACKFILL_SIG_LIMIT: usize = 100;

/// Seen-signature memory cap. Cleared wholesale past it (a duplicate
/// across the clear is possible but harmless: the gate re-submits and
/// confirm guards — holding, cooldown — absorb it).
const SEEN_SIG_CAP: usize = 4_000;

/// Deadline for the WebSocket handshake (`connect` + `logs_subscribe`):
/// a hung handshake must trip the reconnect loop (with backoff) instead
/// of stalling it forever with no metric and no retry.
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Anchor log markers emitted for launch instructions (matched
/// case-insensitively at the call site). Both versions are listed
/// explicitly: v2 matching used to ride on the v1 prefix
/// (`"instruction: createv2".contains("instruction: create")`), which is
/// correct but implicit — a program-side log rename would silently drop
/// one version.
const CREATE_LOG_MARKERS: [&str; 2] = ["instruction: create", "instruction: createv2"];

/// True when any program log line announces a launch (v1 or v2).
/// Pure; the read loop lowercases once per line before calling.
fn is_launch_log(line: &str) -> bool {
    CREATE_LOG_MARKERS.iter().any(|m| line.contains(m))
}

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
    let exp_ms = 100u64.saturating_mul(1u64 << attempt.min(6)).min(5_000);
    let total = exp_ms.saturating_add(jitter_ms(100)).min(5_000);
    Duration::from_millis(total)
}

fn jitter_ms(max: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    if max == 0 {
        return 0;
    }
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => u64::from(d.subsec_nanos() % (max as u32)),
        Err(_) => 0,
    }
}

/// Pump.fun log listener.
pub struct PumpFunListener {
    config: ListenerConfig,
    stats: Arc<ListenerStats>,
    /// Recently seen signatures (dedup for gap backfill; bounded).
    seen: std::sync::Mutex<std::collections::HashSet<String>>,
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
            seen: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Shared stats handle.
    pub fn stats(&self) -> Arc<ListenerStats> {
        Arc::clone(&self.stats)
    }

    /// Record a signature as seen (bounded dedup memory).
    fn mark_seen(&self, sig: &str) {
        match self.seen.lock() {
            Ok(mut guard) => {
                if guard.len() >= SEEN_SIG_CAP {
                    guard.clear();
                }
                guard.insert(sig.to_string());
            }
            Err(_) => debug!("seen-set lock poisoned; dedup skipped"),
        }
    }

    /// Gap backfill after a reconnect: pull the latest program
    /// signatures and replay any the socket never delivered, oldest
    /// first through the same bounded workers. Best-effort and
    /// deadline-bound — a failed backfill warns and the live loop
    /// continues (a gap is a miss, never a stall). Skipped on first
    /// boot (live-only start preserves current boot semantics).
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
        // Oldest first: quorum order should follow chain order.
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
            let sig = info.signature.clone();
            let slot = info.slot;
            let Some(commitment) = self.config.commitment_config() else {
                continue;
            };
            tokio::spawn(async move {
                fetch_and_emit(
                    &sig, slot, &rpc_clone, &tx_clone, &stats_clone, &sem_clone, commitment,
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

    /// Run the reconnect loop forever, emitting events into `tx`.
    ///
    /// Uses a bounded `mpsc::Sender` (capacity 10_000 expected). Drops on
    /// full with metric accounting; never blocks the socket loop.
    pub async fn run(self, tx: mpsc::Sender<ListenerEvent>) -> Result<(), ListenerError> {
        let rpc = Arc::new(RpcClient::new(self.config.http_url.clone()));
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
        let mut attempt: u32 = 0;
        // First session starts live-only (current boot semantics); every
        // later session backfills the gap the dead socket missed.
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
            // A long healthy session resets the backoff: without this, one
            // old outage keeps every later blip sleeping at the ~5s cap and
            // the listener bleeds launches it should catch instantly.
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

    /// Single subscription session; returns when the socket stream ends.
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
            // Downstream may drop mid-session: stop spawning workers for
            // a dead channel instead of fetching until the socket ends.
            if tx.is_closed() {
                info!("downstream closed mid-session; ending subscription");
                break;
            }
            self.stats.inc_logs_seen();
            let sig_str = response.value.signature.clone();
            let slot_hint = response.context.slot;
            self.mark_seen(&sig_str);

            // Only successful transactions are interesting.
            if response.value.err.is_some() {
                continue;
            }
            // Anchor emits `Instruction: Create` in program logs for `create`.
            let has_create = response
                .value
                .logs
                .iter()
                .any(|line| is_launch_log(&line.to_ascii_lowercase()));
            if !has_create {
                continue;
            }
            self.stats.inc_candidates();

            // Hand the RPC round-trip to a worker; never block the read loop.
            // Concurrency is bounded INSIDE the worker (per-attempt fetch
            // permits): workers that find no permit exit immediately, and
            // backoff sleeps never pin a permit.
            let tx_clone = tx.clone();
            let rpc_clone = Arc::clone(rpc);
            let stats_clone = Arc::clone(&self.stats);
            let sem_clone = Arc::clone(semaphore);
            let Some(commitment) = self.config.commitment_config() else {
                // Validated at subscribe(); unreachable in practice, but
                // never coerce silently.
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
        Self::subscribe_with_capacity(config, EVENT_CHANNEL_CAPACITY).await
    }

    /// Spawn a listener with an explicit channel capacity (tests tune
    /// this down to exercise drop-on-full paths without 10k events).
    pub async fn subscribe_with_capacity(
        config: ListenerConfig,
        capacity: usize,
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
        let (tx, rx) = mpsc::channel::<ListenerEvent>(capacity.max(1));
        let handle: JoinHandle<()> = tokio::spawn(async move {
            if let Err(e) = listener.run(tx).await {
                warn!(error = %e, "listener exited with error");
            }
        });
        Ok((rx, stats_out, handle))
    }
}

/// Fetch one transaction with retries for transient failures.
/// Permits are held only across live RPCs, never across the backoff
/// sleeps: each attempt runs under a fresh permit (giving up with a
/// metric when saturated). Tiers: 250ms retry for any transient error,
/// plus a 1s third attempt for `429` alone (the fast retry usually lands
/// inside the same limit window).
/// Returns `None` on any terminal outcome (metrics recorded inside).
/// Deterministic errors (unknown transaction, bad params, rejected
/// encoding) return immediately without burning an extra RPC.
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
                // Warn, not debug: sustained throttling is a systemic
                // miss-trade signal during Pump.fun bursts (fail-closed
                // for funds, but the sniper is blind while this fires).
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
                // Rate limits get one last, longer tier: the 250ms retry
                // usually lands inside the same window, while a ~1s pause
                // often clears it. Jittered (RPC hides Retry-After, so a
                // fixed delay would re-collide with other clients'
                // retries). Other transients stop here (two RPCs per
                // candidate is the quota budget).
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

/// True for rate-limit rejections specifically (`429`): worth a longer
/// second backoff tier, since the first 250ms retry usually lands inside
/// the same limit window.
fn is_rate_limited(e: &ClientError) -> bool {    use solana_client::client_error::ClientErrorKind;
    matches!(
        e.kind(),
        ClientErrorKind::RpcError(solana_client::rpc_request::RpcError::RpcResponseError {
            code: 429,
            ..
        })
    )
}

/// True for failures worth one retry: transport-level trouble plus
/// node-overload/availability RPC codes — NOT deterministic rejections.
/// Retried: I/O errors, reqwest timeouts/connects, and JSON-RPC
/// `-32005` (node behind/unhealthy), `-32007`/`-32009` (skipped or
/// missing slots), `429` (rate-limited). Returned at once: unknown
/// transaction, bad params (`-32602`), unsupported tx version
/// (`-32015`), decode/body failures, signing/transaction errors.
/// Mirrors the auditor's transient classification.
fn is_transient_fetch_error(e: &ClientError) -> bool {
    use solana_client::client_error::ClientErrorKind;
    match e.kind() {
        ClientErrorKind::Io(_) => true,
        ClientErrorKind::Reqwest(e) => e.is_timeout() || e.is_connect(),
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

/// Fetch one transaction and emit every `create` event found in it.
///
/// Runs inside a spawned worker (not the socket read loop). Never panics;
/// all failures are counted and logged. Fetch permits are acquired per
/// attempt and released across the backoff sleep: a sleeping worker must
/// not pin one of the 32 fetch slots.
async fn fetch_and_emit(
    sig_str: &str,
    slot_hint: u64,
    rpc: &RpcClient,
    tx: &mpsc::Sender<ListenerEvent>,
    stats: &Arc<ListenerStats>,
    sem: &Arc<Semaphore>,
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
    // `loadedAddresses` split). Binary payloads are rejected below
    // (`UnsupportedEncoding`) — a node forcing binary drops launches.
    // Version through v1: current Pump.fun launches are v1 envelopes and
    // `Some(0)` makes the RPC refuse them (-32015); see `json_tx_config`.
    // If the program ever ships v2 envelopes, every fetch here fails
    // deterministically (-32015) — loud in logs, unmissable in sims.
    let tx_config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(1),
    };

    // Whole fetch is deadline-bound (one throttled/overloaded retry, then
    // give up: a missed launch beats a wedged worker) so hung RPCs cannot
    // hold semaphore permits forever.
    let fetched = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fetch_tx_permit_bound(rpc, &signature, tx_config, sem, stats, sig_str),
    )
    .await    {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(_) => {
            stats.inc_fetch_err();
            debug!(signature = %sig_str, "getTransaction timed out");
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
    // Prefer the on-chain slot; fall back to the log-context slot when the
    // fetch reports slot 0 (downstream bundle rules key on slots).
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

    // Top-level and inner instructions share one create-classification
    // path (a `sell_v2` fix in one place must not be forgotten in the
    // other): decode, then parse v2-or-v1, counting outcomes identically.
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

    // Inner instructions: `data` may be base58 or base64 — decoded
    // through the same shared helper as top-level.
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

    // Slot observability for the fallback above.
    debug!(signature = %sig_str, slot, slot_hint, "extracted events");

    out
}

/// Classify one decoded Pump.fun instruction as a launch event:
/// `Some` on `create`/`create_v2`, `None` on other discriminators,
/// `Err` on malformed create payloads. Single home for the v2→v1
/// branching used by top-level and inner loops alike.
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
            } else if meta.is_some() {
                tracing::trace!("inner instructions absent (Skip/None); top-level only");
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
/// Out-of-bounds indexes (malformed RPC payloads) log at debug and skip —
/// callers must never mistake a truncated instruction for an absent one
/// in silence.
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

/// Resolve one `UiInstruction` (inner) — only compiled form carries raw data.
/// Parsed-form instructions are skipped with a trace (no data to decode).
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
        // Anchor PascalCases the log (`Create`, `CreateV2`); the read
        // loop lowercases before matching. Both versions must hit, and an
        // unrelated swap log must not.
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
            http_url: "https://api.mainnet-beta.solana.com".to_string(),
            commitment: "confirmed".to_string(),
        };
        assert!(PumpFunListener::new(cfg).is_err());
    }

    #[test]
    fn seen_set_dedups_and_bounds_memory() {
        let cfg = ListenerConfig {
            ws_url: "wss://api.mainnet-beta.solana.com".to_string(),
            http_url: "https://api.mainnet-beta.solana.com".to_string(),
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
        // Real mainnet create_v2 (Pump.fun program 6EF8..., slot 455573900,
        // fetched live): proves the v2 wire shape — 16-account instruction,
        // Borsh (name/symbol/uri) + trailing versioned args — decodes and
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
}
