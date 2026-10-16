//! Yellowstone gRPC ingestion: same `ListenerEvent` channel as WS.
//!
//! Selected when `ListenerConfig.geyser_url` is non-empty; WS is used
//! otherwise — never both at once (two feeds would double-parse the
//! same chain). Full transactions arrive inline, so there is no
//! per-candidate `getTransaction` fetch and no saturation drops on
//! this path. Gap coverage rides a bounded REST `getBlock` backfill
//! (`from_slot` replay is server-rejected below `processed`); the
//! channel stays drop-on-full like the WS path.

use crate::parser::{
    PUMP_PROGRAM_ID_STR, is_pump_create_ix, is_pump_create_v2_ix, parse_pump_create,
    parse_pump_create_v2,
};
use crate::stream::backoff_duration;
use crate::types::{EVENT_CHANNEL_CAPACITY, ListenerConfig, ListenerEvent, ListenerStats};
use futures_util::{SinkExt, StreamExt};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::*;

/// Seen-signature cap, same policy as the WS path (`stream.rs`).
const SEEN_SIG_CAP: usize = 4_000;

/// Env var carrying the Geyser `x-token`. Secret: env-only, never the
/// config file (same rule as `INFERENCE_API_KEY`), never logged.
pub const YELLOWSTONE_TOKEN_ENV: &str = "YELLOWSTONE_TOKEN";

/// Geyser failures. Mapped into [`crate::stream::ListenerError`] at
/// the subscribe boundary so callers handle one error type.
#[derive(Error, Debug)]
pub enum GeyserError {
    /// Endpoint unusable or token rejected at connect.
    #[error("geyser connect failed: {0}")]
    Connect(String),
    /// Subscription or stream failure (reconnectable).
    #[error("geyser stream failed: {0}")]
    Stream(String),
}

/// Yellowstone-backed listener. Mirrors `PumpFunListener`'s output
/// contract: `(receiver, stats, task handle)`.
pub struct GeyserSubscriber {
    config: ListenerConfig,
    token: Option<String>,
    stats: Arc<ListenerStats>,
    seen: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Newest fully-processed slot (0 = none). Carried into every
    /// reconnect — including after errors — so the gap replays instead
    /// of going blind (dedup bounds the redelivery).
    last_slot: Arc<std::sync::atomic::AtomicU64>,
    /// Shared REST pool for gap backfill (`getBlock` scans). `None`
    /// skips backfill — never open a private pool here.
    rpc: Option<Arc<RpcClient>>,
}

    /// Rate-limit cooldown: hammering a 429 every 5s sustains the ban
/// (each reconnect is a fresh subscribe the limiter must refuse).
/// A minute of silence lets token-bucket limiters recover; anything
/// still failing after that is quota/exhaustion, not burst.
pub const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);

/// Ban trip point: this many consecutive 429s (~30 min at the 60s
/// 429 cadence) is quota/exhaustion, not burst — far past any
/// token-bucket recovery. Below it, isolated 429s keep the fast path.
pub const BANNED_AFTER_CONSECUTIVE_429: u32 = 30;

/// Banned retry cadence. Hourly probes bound the RPC burn while keeping
/// recovery detection; anything faster sustains the ban it measures.
pub const BANNED_RETRY_INTERVAL: Duration = Duration::from_secs(3_600);

/// Consecutive-429 circuit breaker. Trips once past the threshold;
/// any healthy subscribe resets fully. Pure so the trip policy is
/// unit-pinned: the run loop only wires its edges to backoff, the ban
/// latch, and logs.
#[derive(Debug, Default)]
pub struct Consecutive429Ban {
    consec: u32,
    banned: bool,
}

impl Consecutive429Ban {
    /// Record a 429. Returns true on the rising edge (trip now).
    /// Already-banned stays banned without re-firing.
    pub fn note_429(&mut self) -> bool {
        self.consec = self.consec.saturating_add(1);
        if !self.banned && self.consec >= BANNED_AFTER_CONSECUTIVE_429 {
            self.banned = true;
            true
        } else {
            false
        }
    }

    /// Record a non-429 outcome: the streak breaks, but a live ban
    /// stays slow until a subscribe actually succeeds (a lone h2 reset
    /// proves nothing about the limiter).
    pub fn note_other(&mut self) {
        self.consec = 0;
    }

    /// Record a working subscribe (clean stream end counts: the server
    /// accepted us). Returns true on the recovery edge.
    pub fn note_healthy(&mut self) -> bool {
        let was = self.banned;
        self.consec = 0;
        self.banned = false;
        was
    }

    /// Whether reconnects must crawl.
    pub fn banned(&self) -> bool {
        self.banned
    }
}

/// True when the failure is an explicit 429 (never a transport guess).
/// Word-boundary match: "4290" (counters, slots) must not trigger
/// minute-waits.
fn is_rate_limited(e: &GeyserError) -> bool {
    let msg = match e {
        GeyserError::Stream(msg) => msg,
        GeyserError::Connect(_) => return false,
    };
    msg.match_indices("429").any(|(i, _)| {
        let before = msg[..i].chars().last().is_none_or(|c| !c.is_ascii_digit());
        let after = msg[i + 3..].chars().next().is_none_or(|c| !c.is_ascii_digit());
        before && after
    })
}

impl GeyserSubscriber {
    /// Build from config plus an optional `x-token` (env-only secret).
    /// Empty token is allowed (self-hosted needs none) — loudly.
    pub fn new(config: ListenerConfig, token: Option<String>) -> Result<Self, GeyserError> {
        config
            .validate()
            .map_err(|e| GeyserError::Connect(e.to_string()))?;
        if config.geyser_url.is_empty() {
            return Err(GeyserError::Connect(
                "geyser_url is empty; refusing geyser mode (use WS)".to_string(),
            ));
        }
        if token.as_deref().is_none_or(|t| t.is_empty()) {
            warn!("YELLOWSTONE_TOKEN unset; connecting without x-token (self-hosted only)");
        }
        Ok(Self {
            config,
            token,
            stats: Arc::new(ListenerStats::new()),
            seen: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            last_slot: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rpc: None,
        })
    }

    /// Build around the shared REST pool (gap backfill scans ride it).
    /// Mirrors `PumpFunListener::with_rpc`: `None` means no backfill,
    /// never a private pool.
    pub fn with_rpc(mut self, rpc: Arc<RpcClient>) -> Self {
        self.rpc = Some(rpc);
        self
    }

    /// Shared stats handle.
    pub fn stats(&self) -> Arc<ListenerStats> {
        Arc::clone(&self.stats)
    }

    /// Spawn the subscriber. Returns receiver, stats, and task handle.
    pub async fn subscribe(
        config: ListenerConfig,
        token: Option<String>,
    ) -> Result<(mpsc::Receiver<ListenerEvent>, Arc<ListenerStats>, JoinHandle<()>), GeyserError>
    {
        Self::subscribe_with_rpc(config, token, None).await
    }

    /// Spawn the subscriber around a shared REST pool (`None` = no gap
    /// backfill). Returns receiver, stats, and task handle.
    pub async fn subscribe_with_rpc(
        config: ListenerConfig,
        token: Option<String>,
        rpc: Option<Arc<RpcClient>>,
    ) -> Result<(mpsc::Receiver<ListenerEvent>, Arc<ListenerStats>, JoinHandle<()>), GeyserError>
    {
        let sub = match rpc {
            Some(shared) => GeyserSubscriber::new(config, token)?.with_rpc(shared),
            None => GeyserSubscriber::new(config, token)?,
        };
        let stats = sub.stats();
        let stats_out = Arc::clone(&stats);
        let (tx, rx) = mpsc::channel::<ListenerEvent>(EVENT_CHANNEL_CAPACITY.max(1));
        let handle: JoinHandle<()> = tokio::spawn(async move {
            sub.run(tx).await;
        });
        Ok((rx, stats_out, handle))
    }
}

/// Reconnect wait: normal backoff, floored at a full minute on
/// explicit 429s. Pure so the 429 policy is unit-pinned, not log lore.
fn reconnect_wait(attempt: u32, rate_limited: bool) -> Duration {
    backoff_duration(attempt).max(if rate_limited {
        RATE_LIMIT_COOLDOWN
    } else {
        Duration::ZERO
    })
}

/// Max slots backfilled per reconnect (~25s of chain). Older gaps are
/// unactionable for a sniper (those launches already ran or rugged) —
/// skip them with a warn stating the missed range instead of burning
/// RPC budget pretending to be whole.
pub const MAX_BACKFILL_SLOTS: u64 = 64;

/// Per-block fetch deadline inside backfill. A wedged block must not
/// stall the resubscribe behind it.
const BACKFILL_BLOCK_TIMEOUT: Duration = Duration::from_secs(20);

/// Backfill window over a missed range: `(scan_from, scan_to, skipped)`.
/// `scan_from..=scan_to` is scanned; the `skipped` older slots are left
/// behind (logged, never silently dropped). `(0, 0, 0)` means caught up.
/// Pure for unit pinning.
fn backfill_window(since_slot: u64, tip_slot: u64) -> (u64, u64, u64) {
    if tip_slot <= since_slot {
        return (0, 0, 0);
    }
    let want_from = since_slot.saturating_add(1);
    let cap_from = tip_slot
        .saturating_sub(MAX_BACKFILL_SLOTS)
        .saturating_add(1);
    let scan_from = want_from.max(cap_from);
    (scan_from, tip_slot, scan_from.saturating_sub(want_from))
}

/// Replay slot actually sent: the server honors `from_slot` only at
/// `processed` commitment and rejects it otherwise ("available only
/// for `processed`"), so non-processed reconnects must subscribe
/// clean — sending it anyway fails every cycle deterministically on
/// our own parameter. Gap coverage on those commitments comes from
/// the REST backfill in `run`, not from this parameter (which stays
/// live for `processed`). Pure for unit pinning.
fn effective_replay_slot(commitment: CommitmentLevel, from_slot: Option<u64>) -> Option<u64> {
    match commitment {
        CommitmentLevel::Processed => from_slot,
        _ => {
            if from_slot.is_some() {
                debug!("from_slot replay unsupported at this commitment; clean subscribe");
            }
            None
        }
    }
}

impl GeyserSubscriber {
    /// Reconnect loop. Same backoff + healthy-reset policy as WS.
    async fn run(self, tx: mpsc::Sender<ListenerEvent>) {
        let mut attempt: u32 = 0;
        let mut from_slot: Option<u64> = None;
        let mut ban = Consecutive429Ban::default();
        loop {
            if tx.is_closed() {
                info!("downstream closed; geyser subscriber shutting down");
                return;
            }
            // Gap backfill before the live session: from_slot replay is
            // unavailable below processed, so without this every
            // confirmed downtime is a permanent hole. First boot has no
            // baseline (skip); banned or pool-less runs skip too — the
            // ban exists to stop RPC burn, backfilling through it would
            // defeat the purpose, and there is nothing to scan with
            // without a shared pool (never open a private one here).
            // Runs before the subscribe so the range is exact; the scan
            // itself extends blindness by its own duration (same
            // tradeoff the WS path accepts).
            let baseline = self.last_slot.load(std::sync::atomic::Ordering::Relaxed);
            if baseline > 0 && !ban.banned() {
                if let Some(rpc) = &self.rpc {
                    self.backfill_gap(rpc, &tx, baseline).await;
                } else {
                    debug!("no shared rpc; geyser gap backfill skipped");
                }
            }
            let seen_before = self.stats.logs_seen();
            let session_start = std::time::Instant::now();
            let mut rate_limited = false;
            match self.run_once(&tx, from_slot).await {
                Ok(last_slot) => {
                    // Clean stream end: reconnect (server closed it).
                    warn!("geyser stream ended; reconnecting");
                    // Replay from the last seen slot so the gap is not
                    // blind. Parse-only cost; dedup + drop-on-full hold
                    // the line on long outages.
                    from_slot = last_slot;
                    if ban.note_healthy() {
                        self.stats.ingestion_banned.store(false, std::sync::atomic::Ordering::Relaxed);
                        warn!("geyser ban lifted: subscription accepted again");
                    }
                }
                Err(e) => {
                    rate_limited = is_rate_limited(&e);
                    if rate_limited {
                        if ban.note_429() {
                            self.stats.ingestion_banned.store(true, std::sync::atomic::Ordering::Relaxed);
                            warn!(attempt, "geyser BANNED: {BANNED_AFTER_CONSECUTIVE_429} consecutive 429s — quota/exhaustion, not burst; retrying hourly, check token/quota or fall back to WS mode");
                        } else {
                            warn!(error = %e, attempt, "geyser rate limited (429); yielding a full minute");
                        }
                    } else {
                        ban.note_other();
                        warn!(error = %e, attempt, "geyser error; backing off");
                    }
                    // Error resets must replay too: `last_slot` tracks
                    // every processed message, so the redelivered prefix
                    // is pure dedup, never a blind gap.
                    let seen = self.last_slot.load(std::sync::atomic::Ordering::Relaxed);
                    if seen > 0 {
                        from_slot = Some(seen);
                    }
                }
            }
            let healthy = self.stats.logs_seen().saturating_sub(seen_before) > 1000
                || session_start.elapsed() > Duration::from_secs(30);
            attempt = if healthy { 0 } else { attempt.saturating_add(1) };
            let wait = if ban.banned() {
                BANNED_RETRY_INTERVAL
            } else {
                reconnect_wait(attempt, rate_limited)
            };
            debug!(?wait, attempt, "geyser reconnect backoff");
            tokio::time::sleep(wait).await;
        }
    }

    /// One subscription session. Returns the last seen slot for replay.
    async fn run_once(
        &self,
        tx: &mpsc::Sender<ListenerEvent>,
        from_slot: Option<u64>,
    ) -> Result<Option<u64>, GeyserError> {
        let commitment = geyser_commitment(&self.config.commitment).ok_or_else(|| {
            GeyserError::Connect(format!("unknown commitment '{}'", self.config.commitment))
        })?;
        let builder = GeyserGrpcClient::build_from_shared(self.config.geyser_url.clone())
            .map_err(|e| GeyserError::Connect(format!("bad geyser endpoint: {e}")))?
            .x_token(self.token.clone())
            .map_err(|e| GeyserError::Connect(format!("bad x-token: {e}")))?;
        // HTTPS endpoints need explicit TLS: tonic dials plaintext
        // otherwise and the server kills it as a transport error
        // ("Connecting to HTTPS without TLS enabled", observed live).
        // Plain-http stays plaintext (self-hosted convention).
        let builder = if self.config.geyser_url.starts_with("https://") {
            builder
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .map_err(|e| GeyserError::Connect(format!("bad geyser TLS config: {e}")))?
        } else {
            builder
        };
        let mut client = builder
            .connect()
            .await
            .map_err(|e| {
                // Flatten the tonic source chain: bare `transport error`
                // hides TLS/DNS/refused/auth-reset underneath, and the
                // operator cannot tell those apart from the log line.
                // Token presence (never the value) separates "no auth
                // offered" from "auth rejected or unreachable".
                let mut chain = e.to_string();
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    chain.push_str("; caused by: ");
                    chain.push_str(&s.to_string());
                    src = s.source();
                }
                let token = if self.token.as_deref().is_some_and(|t| !t.is_empty()) {
                    "set"
                } else {
                    "unset"
                };
                GeyserError::Connect(format!(
                    "{chain} (endpoint={}, x-token={token})",
                    ids::redact_endpoint(&self.config.geyser_url),
                ))
            })?;
        let replay_slot = effective_replay_slot(commitment, from_slot);
        let request = subscribe_request(commitment, replay_slot);
        let (mut sink, mut stream) = client
            .subscribe_with_request(Some(request))
            .await
            .map_err(|e| GeyserError::Stream(e.to_string()))?;
        info!(
            endpoint = %ids::redact_endpoint(&self.config.geyser_url),
            commitment = %self.config.commitment,
            replay = replay_slot.is_some(),
            "subscribed to pump.fun transactions via geyser"
        );
        let mut last_slot: Option<u64> = from_slot;
        while let Some(msg) = stream.next().await {
            if tx.is_closed() {
                info!("downstream closed mid-session; ending geyser subscription");
                return Ok(last_slot);
            }
            let update = msg.map_err(|e| GeyserError::Stream(e.to_string()))?;
            let Some(oneof) = update.update_oneof else {
                continue;
            };
            match oneof {
                subscribe_update::UpdateOneof::Transaction(tx_update) => {
                    let slot = tx_update.slot;
                    let Some(info) = tx_update.transaction else {
                        continue;
                    };
                    last_slot = Some(slot);
                    self.last_slot
                        .store(slot, std::sync::atomic::Ordering::Relaxed);
                    self.handle_transaction(info, slot, tx).await;
                }
                subscribe_update::UpdateOneof::Ping(_) => {
                    // Empty server ping: any pong id keeps the connection
                    // alive (there is no id to echo). A failed reply means
                    // the stream is dying — reconnect.
                    let reply = SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    };
                    if sink.send(reply).await.is_err() {
                        return Err(GeyserError::Stream(
                            "ping reply failed; reconnecting".to_string(),
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(last_slot)
    }

    /// One-shot REST backfill over a missed range after reconnect.
    /// `from_slot` replay is unavailable below `processed`, so without
    /// this every confirmed downtime is a permanent hole. Sequential
    /// `getBlock` scans (no spawn storms — the saturation pathology the
    /// WS path taught us); dedup via the seen set; drop-on-full holds
    /// the line; wedged blocks time out instead of stalling the
    /// resubscribe behind them.
    async fn backfill_gap(
        &self,
        rpc: &Arc<RpcClient>,
        tx: &mpsc::Sender<ListenerEvent>,
        since_slot: u64,
    ) {
        use solana_client::rpc_config::RpcBlockConfig;
        use solana_transaction_status::{
            EncodedTransaction, TransactionDetails, UiMessage, UiTransactionEncoding,
            option_serializer::OptionSerializer,
        };
        let tip = match rpc.get_slot().await {
            Ok(tip) => tip,
            Err(e) => {
                warn!(
                    error = %ids::redact_urls_in_text(&e.to_string()),
                    "geyser backfill: tip unreadable; gap stays"
                );
                return;
            }
        };
        let (from, to, skipped) = backfill_window(since_slot, tip);
        if to == 0 {
            return;
        }
        if skipped > 0 {
            warn!(
                skipped,
                from,
                to,
                "geyser gap older than backfill cap; scanning newest only (stale launches unactionable)"
            );
        }
        let mut scanned = 0u32;
        let mut emitted = 0u32;
        for slot in from..=to {
            if tx.is_closed() {
                return;
            }
            let block = match tokio::time::timeout(
                BACKFILL_BLOCK_TIMEOUT,
                rpc.get_block_with_config(
                    slot,
                    RpcBlockConfig {
                        encoding: Some(UiTransactionEncoding::Json),
                        transaction_details: Some(TransactionDetails::Full),
                        rewards: Some(false),
                        commitment: None,
                        max_supported_transaction_version: Some(1),
                    },
                ),
            )
            .await
            {
                Ok(Ok(block)) => block,
                _ => continue, // Pruned, missing, or throttled: skip the slot.
            };
            scanned += 1;
            let empty = Vec::new();
            for tx_meta in block.transactions.as_ref().unwrap_or(&empty) {
                if tx_meta.meta.as_ref().is_some_and(|m| m.err.is_some()) {
                    continue;
                }
                let EncodedTransaction::Json(ui_tx) = &tx_meta.transaction else {
                    continue;
                };
                let UiMessage::Raw(raw) = &ui_tx.message else {
                    continue;
                };
                let sig = match ui_tx.signatures.first() {
                    Some(sig) if !sig.is_empty() => sig,
                    _ => continue,
                };
                if !self.mark_seen_new(sig) {
                    continue;
                }
                let mut keys: Vec<Pubkey> = Vec::with_capacity(raw.account_keys.len() + 8);
                let mut keys_ok = true;
                for k in &raw.account_keys {
                    match Pubkey::from_str(k) {
                        Ok(p) => keys.push(p),
                        Err(_) => {
                            keys_ok = false;
                            break;
                        }
                    }
                }
                if !keys_ok {
                    continue;
                }
                if let Some(meta) = tx_meta.meta.as_ref()
                    && let OptionSerializer::Some(loaded) = &meta.loaded_addresses
                {
                    for k in loaded.writable.iter().chain(loaded.readonly.iter()) {
                        match Pubkey::from_str(k) {
                            Ok(p) => keys.push(p),
                            Err(_) => {
                                keys_ok = false;
                                break;
                            }
                        }
                    }
                    if !keys_ok {
                        continue;
                    }
                }
                let pump_idx = match keys
                    .iter()
                    .position(|k| k.to_string() == PUMP_PROGRAM_ID_STR)
                {
                    Some(i) => i,
                    None => continue,
                };
                for cix in &raw.instructions {
                    if usize::from(cix.program_id_index) != pump_idx {
                        continue;
                    }
                    let Ok(data) = bs58::decode(&cix.data).into_vec() else {
                        continue;
                    };
                    if Self::emit_if_create(
                        &keys,
                        usize::from(cix.program_id_index),
                        &cix.accounts,
                        &data,
                        sig,
                        slot,
                        tx,
                    ) {
                        emitted += 1;
                    }
                }
                // Inner instructions ride the same classifier (parity
                // with the live path, which scans both levels). JSON
                // encodes inners as an enum: only Compiled legs carry
                // index/account/data slots.
                if let Some(meta) = tx_meta.meta.as_ref()
                    && let OptionSerializer::Some(sets) = &meta.inner_instructions
                {
                    use solana_transaction_status::UiInstruction;
                    for set in sets {
                        for cix in &set.instructions {
                            let UiInstruction::Compiled(cix) = cix else {
                                continue;
                            };
                            if usize::from(cix.program_id_index) != pump_idx {
                                continue;
                            }
                            let Ok(data) = bs58::decode(&cix.data).into_vec() else {
                                continue;
                            };
                            if Self::emit_if_create(
                                &keys,
                                usize::from(cix.program_id_index),
                                &cix.accounts,
                                &data,
                                sig,
                                slot,
                                tx,
                            ) {
                                emitted += 1;
                            }
                        }
                    }
                }
            }
        }
        if emitted > 0 {
            warn!(missed = emitted, scanned, "backfilled geyser-gap launches");
        } else {
            debug!(scanned, "geyser backfill: no missed launches in range");
        }
    }

    /// Decode one transaction update and emit every `create` found.
    /// Skips votes, failures, and key tables that do not resolve fully
    /// (a partial table proves nothing — same rule as the WS path).
    async fn handle_transaction(
        &self,
        info: SubscribeUpdateTransactionInfo,
        slot: u64,
        tx: &mpsc::Sender<ListenerEvent>,
    ) {
        if info.is_vote {
            return;
        }
        let Some(meta) = info.meta else {
            return;
        };
        if meta.err.as_ref().is_some_and(|e| !e.err.is_empty()) {
            return;
        }
        let Some(message) = info.transaction.and_then(|t| t.message) else {
            return;
        };
        if info.signature.len() != 64 {
            return;
        }
        let sig = bs58::encode(&info.signature).into_string();
        self.stats.inc_logs_seen();
        self.mark_seen(&sig);
        let keys = resolve_keys(&message.account_keys, &meta);
        if keys.is_empty() {
            return;
        }
        let mut emitted = false;
        for cix in &message.instructions {
            if Self::emit_if_create(
                &keys,
                cix.program_id_index as usize,
                &cix.accounts,
                &cix.data,
                &sig,
                slot,
                tx,
            ) {
                // Same counter the WS path feeds per decoded launch: the
                // heartbeat's parse_ok stays comparable across feeds.
                self.stats.inc_parse_ok();
                emitted = true;
            }
        }
        for set in &meta.inner_instructions {
            for cix in &set.instructions {
                if Self::emit_if_create(
                    &keys,
                    cix.program_id_index as usize,
                    &cix.accounts,
                    &cix.data,
                    &sig,
                    slot,
                    tx,
                ) {
                    self.stats.inc_parse_ok();
                    emitted = true;
                }
            }
        }
        if emitted {
            self.stats.inc_candidates();
        }
    }

    /// Classify one decoded ix: emit on launch, `true` when emitted.
    #[allow(clippy::too_many_arguments)]
    fn emit_if_create(
        keys: &[Pubkey],
        program_idx: usize,
        accounts: &[u8],
        data: &[u8],
        sig: &str,
        slot: u64,
        tx: &mpsc::Sender<ListenerEvent>,
    ) -> bool {
        let Some(program_id) = keys.get(program_idx) else {
            return false;
        };
        if program_id.to_string() != PUMP_PROGRAM_ID_STR {
            return false;
        }
        let resolved: Vec<Pubkey> = accounts
            .iter()
            .filter_map(|i| keys.get(*i as usize).copied())
            .collect();
        if resolved.len() != accounts.len() {
            return false;
        }
        let parsed = if is_pump_create_v2_ix(data) {
            parse_pump_create_v2(data, &resolved, sig.to_string(), slot)
        } else if is_pump_create_ix(data) {
            parse_pump_create(data, &resolved, sig.to_string(), slot)
        } else {
            return false;
        };
        let ev = match parsed {
            Ok(ev) => ev,
            Err(_) => return false,
        };
        match tx.try_send(Arc::new(ev)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("event channel full; dropping geyser launch event");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Mark a signature seen (bounded memory, same policy as WS).
    /// Returns false when already known (poisoned lock counts as new:
    /// dropping a candidate on lock trouble would blind the feed).
    fn mark_seen_new(&self, sig: &str) -> bool {
        match self.seen.lock() {
            Ok(mut guard) => {
                if guard.len() >= SEEN_SIG_CAP {
                    let drop_n = guard.len() / 2;
                    let victims: Vec<String> = guard.iter().take(drop_n).cloned().collect();
                    for v in victims {
                        guard.remove(&v);
                    }
                }
                guard.insert(sig.to_string())
            }
            Err(_) => {
                debug!("seen-set lock poisoned; dedup skipped");
                true
            }
        }
    }

    /// Mark a signature seen (bounded memory, same policy as WS).
    fn mark_seen(&self, sig: &str) {
        let _ = self.mark_seen_new(sig);
    }
}

/// Map the config commitment string to Geyser's enum. `None` when unknown.
fn geyser_commitment(commitment: &str) -> Option<CommitmentLevel> {
    match commitment {
        "processed" => Some(CommitmentLevel::Processed),
        "confirmed" => Some(CommitmentLevel::Confirmed),
        "finalized" => Some(CommitmentLevel::Finalized),
        _ => None,
    }
}

/// Build the subscription: pump-program transactions only, no votes,
/// no failures. Everything else is filtered client-side by the
/// existing parser (same semantics as the WS path).
fn subscribe_request(commitment: CommitmentLevel, from_slot: Option<u64>) -> SubscribeRequest {
    let mut transactions = HashMap::new();
    transactions.insert(
        "pump".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: vec![PUMP_PROGRAM_ID_STR.to_string()],
            ..Default::default()
        },
    );
    SubscribeRequest {
        transactions,
        commitment: Some(commitment.into()),
        from_slot,
        ..Default::default()
    }
}

/// Resolve the full key table: static keys plus address-table-loaded.
/// `None` entries (short/truncated keys) drop the whole transaction —
/// a partial table proves nothing.
fn resolve_keys(
    account_keys: &[Vec<u8>],
    meta: &TransactionStatusMeta,
) -> Vec<Pubkey> {
    let mut keys = Vec::with_capacity(account_keys.len() + 8);
    for raw in account_keys
        .iter()
        .chain(meta.loaded_writable_addresses.iter())
        .chain(meta.loaded_readonly_addresses.iter())
    {
        let Ok(bytes): Result<[u8; 32], _> = raw.as_slice().try_into() else {
            return Vec::new();
        };
        keys.push(Pubkey::new_from_array(bytes));
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use yellowstone_grpc_proto::prelude::{
        CompiledInstruction, InnerInstruction, InnerInstructions, Message, Transaction,
        TransactionError, TransactionStatusMeta,
    };

    #[test]
    fn rate_limit_classifier_is_exact() {
        // Only an explicit 429 counts: transport guesses must keep the
        // fast path, or every outage degrades to minute-waits.
        assert!(is_rate_limited(&GeyserError::Stream(
            "code: 'Internal error', 429 Too Many Requests".to_string()
        )));
        for msg in [
            "geyser connect failed: transport error",
            "ping reply failed; reconnecting",
            "4290 numeric lookalike",
        ] {
            assert!(!is_rate_limited(&GeyserError::Stream(msg.to_string())), "{msg}");
            assert!(!is_rate_limited(&GeyserError::Connect(msg.to_string())), "{msg}");
        }
        assert_eq!(RATE_LIMIT_COOLDOWN, Duration::from_secs(60));
    }

    #[test]
    fn reconnect_wait_floors_429_at_a_minute() {
        // backoff carries jitter, so pin bounds, not exact values: the
        // normal path never exceeds its 5s cap, the 429 path never
        // returns sooner than a minute, at any attempt.
        for attempt in [0, 1, 7, u32::MAX] {
            assert!(
                reconnect_wait(attempt, false) <= Duration::from_secs(5),
                "attempt={attempt}"
            );
            assert!(
                reconnect_wait(attempt, true) >= Duration::from_secs(60),
                "attempt={attempt}"
            );
        }
    }

    #[test]
    fn replay_slot_only_at_processed() {
        // Server rejects from_slot on confirmed/finalized: sending it
        // fails every reconnect deterministically (observed live loop).
        assert_eq!(
            effective_replay_slot(CommitmentLevel::Processed, Some(123)),
            Some(123)
        );
        assert_eq!(effective_replay_slot(CommitmentLevel::Processed, None), None);
        for level in [CommitmentLevel::Confirmed, CommitmentLevel::Finalized] {
            assert_eq!(effective_replay_slot(level, Some(123)), None);
            assert_eq!(effective_replay_slot(level, None), None);
        }
    }

    #[test]
    fn backfill_window_caps_and_reports_skips() {
        // Caught up (or time running backwards): nothing to scan.
        assert_eq!(backfill_window(100, 100), (0, 0, 0));
        assert_eq!(backfill_window(100, 90), (0, 0, 0));
        // Short gap: scan it whole, nothing skipped.
        assert_eq!(backfill_window(100, 110), (101, 110, 0));
        // Exactly at the cap: still whole, still nothing skipped.
        assert_eq!(
            backfill_window(100, 100 + MAX_BACKFILL_SLOTS),
            (101, 100 + MAX_BACKFILL_SLOTS, 0)
        );
        // Long gap: newest cap-sized slice, older slots reported.
        let (from, to, skipped) = backfill_window(100, 100 + MAX_BACKFILL_SLOTS + 50);
        assert_eq!((to - from + 1), MAX_BACKFILL_SLOTS);
        assert_eq!(skipped, 50);
        assert_eq!(to, 100 + MAX_BACKFILL_SLOTS + 50);
    }

    #[test]
    fn rate_ban_trips_once_and_recovers() {
        let mut ban = Consecutive429Ban::default();
        assert!(!ban.banned());
        // Below the threshold: fast path, no edge.
        for _ in 0..BANNED_AFTER_CONSECUTIVE_429 - 1 {
            assert!(!ban.note_429());
        }
        assert!(!ban.banned());
        // Threshold: single rising edge, then sticky without refire.
        assert!(ban.note_429());
        assert!(ban.banned());
        assert!(!ban.note_429());
        assert!(ban.banned());
        // A lone other error breaks the streak but keeps the crawl:
        // it proves nothing about the limiter.
        ban.note_other();
        assert!(ban.banned());
        assert!(!ban.note_429());
        // A working subscribe recovers with an edge for the log.
        assert!(ban.note_healthy());
        assert!(!ban.banned());
        assert!(!ban.note_healthy());
    }

    #[test]
    fn rate_ban_counter_saturates() {
        let mut ban = Consecutive429Ban {
            consec: u32::MAX,
            ..Default::default()
        };
        assert!(ban.note_429());
        assert!(ban.banned());
    }

    #[test]
    fn commitment_maps_all_levels() {
        assert_eq!(geyser_commitment("processed"), Some(CommitmentLevel::Processed));
        assert_eq!(geyser_commitment("confirmed"), Some(CommitmentLevel::Confirmed));
        assert_eq!(geyser_commitment("finalized"), Some(CommitmentLevel::Finalized));
        assert_eq!(geyser_commitment("bogus"), None);
    }

    #[test]
    fn subscribe_request_filters_pump_only() {
        let req = subscribe_request(CommitmentLevel::Confirmed, None);
        let filter = req.transactions.get("pump").expect("pump filter");
        assert_eq!(filter.vote, Some(false));
        assert_eq!(filter.failed, Some(false));
        assert_eq!(filter.account_include, vec![PUMP_PROGRAM_ID_STR.to_string()]);
        assert_eq!(req.commitment, Some(CommitmentLevel::Confirmed as i32));
        assert_eq!(req.from_slot, None);
        let replay = subscribe_request(CommitmentLevel::Finalized, Some(123));
        assert_eq!(replay.from_slot, Some(123));
    }

    #[test]
    fn resolve_keys_joins_static_and_loaded() {
        let meta = TransactionStatusMeta {
            loaded_writable_addresses: vec![vec![9u8; 32]],
            loaded_readonly_addresses: vec![vec![10u8; 32]],
            ..Default::default()
        };
        let keys = resolve_keys(&[vec![7u8; 32], vec![8u8; 32]], &meta);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[2], Pubkey::new_from_array([9u8; 32]));
        // Short keys poison the whole table (fail-closed, never partial).
        let bad_meta = TransactionStatusMeta {
            loaded_writable_addresses: vec![vec![1u8; 4]],
            ..Default::default()
        };
        assert!(resolve_keys(&[vec![7u8; 32]], &bad_meta).is_empty());
    }

    /// Frozen mainnet `create_v2` bytes through the geyser decode path:
    /// proves the proto-native extractor emits the same event as the WS
    /// path, with no RPC fetch involved. Same envelope + assertions as
    /// the parser's own frozen test (`makemerich` / `mmrich`).
    fn fixture_v2_parts() -> (Vec<u8>, Vec<usize>, Vec<String>) {
        use crate::parser::decode_ix_data;
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/create_v2_mainnet.json"))
                .expect("fixture");
        let msg = &raw["result"]["transaction"]["message"];
        let keys: Vec<String> =
            serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        let pump = ids::PUMP_PROGRAM_ID.to_string();
        for ix in msg["instructions"].as_array().expect("ixs") {
            let pi = ix["programIdIndex"].as_u64().expect("pi") as usize;
            if keys.get(pi) != Some(&pump) {
                continue;
            }
            let data = decode_ix_data(ix["data"].as_str().expect("data")).expect("decode");
            if !data.starts_with(&ids::PUMP_CREATE_V2_DISCRIMINATOR) {
                continue;
            }
            let idx: Vec<usize> = serde_json::from_value(ix["accounts"].clone()).expect("accts");
            return (data, idx, keys);
        }
        panic!("no create_v2 in fixture");
    }

    fn proto_message(data: &[u8], idx: &[usize], keys: &[String]) -> (Message, Vec<u8>) {
        let account_keys: Vec<Vec<u8>> = keys
            .iter()
            .map(|k| Pubkey::from_str(k).expect("key").to_bytes().to_vec())
            .collect();
        let msg = Message {
            account_keys,
            instructions: vec![CompiledInstruction {
                program_id_index: keys
                    .iter()
                    .position(|k| k == &ids::PUMP_PROGRAM_ID.to_string())
                    .expect("pump idx") as u32,
                accounts: idx.iter().map(|i| *i as u8).collect(),
                data: data.to_vec(),
            }],
            ..Default::default()
        };
        (msg, vec![7u8; 64])
    }

    #[tokio::test]
    async fn geyser_path_emits_frozen_create_v2() {
        let (data, idx, keys) = fixture_v2_parts();
        let (message, sig) = proto_message(&data, &idx, &keys);
        let info = SubscribeUpdateTransactionInfo {
            signature: sig,
            is_vote: false,
            transaction: Some(Transaction {
                signatures: vec![vec![7u8; 64]],
                message: Some(message),
            }),
            meta: Some(TransactionStatusMeta::default()),
            ..Default::default()
        };
        let sub = GeyserSubscriber::new(
            ListenerConfig {
                geyser_url: "https://grr.example:443".to_string(),
                ..ListenerConfig::default()
            },
            None,
        )
        .expect("config");
        let (tx, mut rx) = mpsc::channel::<ListenerEvent>(16);
        sub.handle_transaction(info, 455573900, &tx).await;
        let ev = rx.recv().await.expect("one launch event");
        assert!(ev.mint.to_string().ends_with("pump"));
        assert_eq!(ev.slot, 455573900);
        assert_eq!(ev.name, "makemerich");
        assert_eq!(ev.symbol, "mmrich");
    }

    #[tokio::test]
    async fn geyser_path_skips_votes_and_failures() {
        let (data, idx, keys) = fixture_v2_parts();
        let (message, sig) = proto_message(&data, &idx, &keys);
        let sub = GeyserSubscriber::new(
            ListenerConfig {
                geyser_url: "https://grr.example:443".to_string(),
                ..ListenerConfig::default()
            },
            None,
        )
        .expect("config");
        let (tx, mut rx) = mpsc::channel::<ListenerEvent>(16);
        let vote = SubscribeUpdateTransactionInfo {
            signature: sig.clone(),
            is_vote: true,
            transaction: Some(Transaction {
                message: Some(message.clone()),
                ..Default::default()
            }),
            meta: Some(TransactionStatusMeta {
                ..Default::default()
            }),
            ..Default::default()
        };
        sub.handle_transaction(vote, 1, &tx).await;
        let failed = SubscribeUpdateTransactionInfo {
            signature: sig,
            is_vote: false,
            transaction: Some(Transaction {
                message: Some(message),
                ..Default::default()
            }),
            meta: Some(TransactionStatusMeta {
                err: Some(TransactionError { err: vec![1u8] }),
                ..Default::default()
            }),
            ..Default::default()
        };
        sub.handle_transaction(failed, 1, &tx).await;
        assert!(rx.try_recv().is_err(), "votes and failures stay silent");
    }

    #[test]
    fn inner_ix_layout_matches_top_level() {
        // Inner instructions carry the same (program, accounts, data)
        // triple, so the classifier needs no second code path.
        let (data, idx, _) = fixture_v2_parts();
        assert!(is_pump_create_v2_ix(&data));
        let inner = InnerInstruction {
            program_id_index: 1,
            accounts: idx.iter().map(|i| *i as u8).collect(),
            data: data.clone(),
            ..Default::default()
        };
        assert_eq!(inner.accounts.len(), idx.len());
        let _ = InnerInstructions {
            index: 0,
            instructions: vec![inner],
        };
    }
}
