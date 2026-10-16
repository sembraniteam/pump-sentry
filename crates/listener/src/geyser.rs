//! Yellowstone gRPC ingestion: same `ListenerEvent` channel as WS.
//!
//! Selected when `ListenerConfig.geyser_url` is non-empty; WS is used
//! otherwise — never both at once (two feeds would double-parse the
//! same chain). Full transactions arrive inline, so there is no
//! `getTransaction` fetch, no fetch semaphore, and no saturation
//! drops on this path. Gap replay rides `from_slot` (parse-only cost,
//! no REST burn); the channel stays drop-on-full like the WS path.

use crate::parser::{
    PUMP_PROGRAM_ID_STR, is_pump_create_ix, is_pump_create_v2_ix, parse_pump_create,
    parse_pump_create_v2,
};
use crate::stream::backoff_duration;
use crate::types::{EVENT_CHANNEL_CAPACITY, ListenerConfig, ListenerEvent, ListenerStats};
use futures_util::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
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
        })
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
        let sub = GeyserSubscriber::new(config, token)?;
        let stats = sub.stats();
        let stats_out = Arc::clone(&stats);
        let (tx, rx) = mpsc::channel::<ListenerEvent>(EVENT_CHANNEL_CAPACITY.max(1));
        let handle: JoinHandle<()> = tokio::spawn(async move {
            sub.run(tx).await;
        });
        Ok((rx, stats_out, handle))
    }

    /// Reconnect loop. Same backoff + healthy-reset policy as WS.
    async fn run(self, tx: mpsc::Sender<ListenerEvent>) {
        let mut attempt: u32 = 0;
        let mut from_slot: Option<u64> = None;
        loop {
            if tx.is_closed() {
                info!("downstream closed; geyser subscriber shutting down");
                return;
            }
            let seen_before = self.stats.logs_seen();
            let session_start = std::time::Instant::now();
            match self.run_once(&tx, from_slot).await {
                Ok(last_slot) => {
                    // Clean stream end: reconnect (server closed it).
                    warn!("geyser stream ended; reconnecting");
                    // Replay from the last seen slot so the gap is not
                    // blind. Parse-only cost; dedup + drop-on-full hold
                    // the line on long outages.
                    from_slot = last_slot;
                }
                Err(e) => {
                    warn!(error = %e, attempt, "geyser error; backing off");
                }
            }
            let healthy = self.stats.logs_seen().saturating_sub(seen_before) > 1000
                || session_start.elapsed() > Duration::from_secs(30);
            attempt = if healthy { 0 } else { attempt.saturating_add(1) };
            let wait = backoff_duration(attempt);
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
        let request = subscribe_request(commitment, from_slot);
        let (mut sink, mut stream) = client
            .subscribe_with_request(Some(request))
            .await
            .map_err(|e| GeyserError::Stream(e.to_string()))?;
        info!(
            endpoint = %ids::redact_endpoint(&self.config.geyser_url),
            commitment = %self.config.commitment,
            replay = from_slot.is_some(),
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
