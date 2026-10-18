//! Jito bundles for frontrunning protection.
//!
//! Signed tx -> bincode -> base64 -> `sendBundle`. The tip is a plain
//! system transfer in the same tx, so a landed bundle pays.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use reqwest::Client;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Designated Jito tip accounts (official `getTipAccounts` list).
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Engine bundle limit: at most 5 txs per bundle.
pub const MAX_BUNDLE_TXS: usize = 5;

/// System program transfer discriminator (u32 LE = 2).
const SYSTEM_TRANSFER_DISCRIMINATOR: u32 = 2;

/// Jito dispatch failures.
#[derive(Error, Debug)]
pub enum JitoError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    /// Block engine JSON-RPC error.
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("Bundle encoding failed: {0}")]
    Encode(String),
    /// Engine rate limit (HTTP 429: 1 req/s/IP/region). Retryable.
    #[error("Jito rate limited; back off and retry")]
    RateLimited,
    #[error("bad Jito endpoint: {0}")]
    BadUrl(String),
}

/// Engine-side bundle state (`getInflightBundleStatuses`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleStatus {
    /// Landed on-chain.
    Landed,
    /// Not failed, landed, or invalid: still in the auction.
    Pending,
    /// Every region marked it failed; never forwarded.
    Failed,
    /// Unknown to the engine (5-minute lookback).
    Invalid,
    /// Status unreadable (transport/parse, or no bundle client):
    /// callers keep their existing behavior.
    Unknown,
}

/// Regional block-engine bundle client.
#[derive(Clone)]
pub struct JitoClient {
    client: Client,
    /// Base URL without trailing slash; each method posts to its own
    /// documented path (`/api/v1/bundles`, `/api/v1/getTipAccounts`,
    /// `/api/v1/getInflightBundleStatuses`).
    base: String,
    /// `getTipAccounts` cache: (accounts, unix seconds fetched).
    tips: std::sync::Arc<std::sync::Mutex<(Vec<String>, u64)>>,
    /// Landed-tip floor cache: (lamports, unix seconds fetched).
    floor: std::sync::Arc<std::sync::Mutex<(u64, u64)>>,
}

/// Tip-list TTL: 15 min; failures keep the old list.
pub const TIP_ACCOUNTS_TTL_SECS: u64 = 900;

/// Landed-tip floor oracle (median of recently landed tips, SOL).
/// Lifts static tips in congestion; 0 when unknown.
pub const TIP_FLOOR_URL: &str = "https://bundles.jito.wtf/api/v1/bundles/tip_floor";
/// Floor TTL: 60s; failures keep the old value (stamped, so a dead
/// oracle costs one attempt per window, not per dispatch).
pub const TIP_FLOOR_TTL_SECS: u64 = 60;

/// Lamports per SOL (floor conversion). Single source is
/// `ids::LAMPORTS_PER_SOL`.
const LAMPORTS_PER_SOL_F: f64 = ids::LAMPORTS_PER_SOL as f64;

/// Consecutive schema failures parsing the tip-floor oracle (metric:
/// a rising count means the oracle shape drifted, not just blipped).
static TIP_FLOOR_PARSE_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Schema-failure count for [`parse_tip_floor_lamports`].
pub fn tip_floor_parse_failures() -> u64 {
    TIP_FLOOR_PARSE_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Seconds since epoch (0 on clock trouble).
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl JitoClient {
    /// Build against a block-engine base URL. Bad URLs fail fast.
    pub fn new(region_base_url: &str) -> Result<Self, JitoError> {
        match url::Url::parse(region_base_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(JitoError::BadUrl(region_base_url.to_string())),
        }
        let base = region_base_url.trim_end_matches('/').to_string();
        match url::Url::parse(&base) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(JitoError::BadUrl(region_base_url.to_string())),
        }
        let client = Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()
            .map_err(JitoError::Network)?;
        Ok(Self {
            client,
            base,
            tips: std::sync::Arc::new(std::sync::Mutex::new((
                JITO_TIP_ACCOUNTS.iter().map(|s| s.to_string()).collect(),
                0,
            ))),
            floor: std::sync::Arc::new(std::sync::Mutex::new((0, 0))),
        })
    }

    /// Base URL in use (logs/tests). Method paths hang off this.
    #[cfg(test)]
    pub fn endpoint(&self) -> &str {
        &self.base
    }

    /// Documented per-method paths.
    fn bundles_url(&self) -> String {
        format!("{}/api/v1/bundles", self.base)
    }

    fn tip_accounts_url(&self) -> String {
        format!("{}/api/v1/getTipAccounts", self.base)
    }

    fn inflight_status_url(&self) -> String {
        format!("{}/api/v1/getInflightBundleStatuses", self.base)
    }

    /// Submit pre-encoded base64 txs. Returns the bundle UUID.
    ///
    /// `params[1]` (`{"encoding": "base64"}`) is mandatory: without it the
    /// engine assumes base58 and rejects with `-32602`. An `Ok` UUID is
    /// receipt only, not landing: confirm via [`Self::inflight_status`].
    pub async fn send_bundle(&self, bundle_b64: Vec<String>) -> Result<String, JitoError> {
        let payload = bundle_payload(bundle_b64);
        let url = self.bundles_url();
        debug!(endpoint = %url, txs = payload.params.0.len(), "submitting bundle");
        let res = self.client.post(&url).json(&payload).send().await?;
        if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            warn!(endpoint = %url, "bundle send rate-limited (1 req/s/IP/region)");
            return Err(JitoError::RateLimited);
        }
        let body: SendBundleResponse = res.json().await?;
        if let Some(err) = body.error {
            return Err(JitoError::Rpc(err.render()));
        }
        let uuid = body.result.unwrap_or_default();
        info!(bundle = %uuid, "bundle accepted");
        Ok(uuid)
    }

    /// Engine-side bundle state. `Unknown` on any transport/parse
    /// trouble: callers keep polling the signature instead.
    pub async fn inflight_status(&self, bundle_id: &str) -> BundleStatus {
        let payload = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getInflightBundleStatuses",
            params: [[bundle_id]],
        };
        let res = match tokio::time::timeout(
            Duration::from_millis(2000),
            self.client.post(self.inflight_status_url()).json(&payload).send(),
        )
        .await
        {
            Ok(Ok(res)) => res,
            _ => return BundleStatus::Unknown,
        };
        let body = match res.text().await {
            Ok(body) => body,
            Err(_) => return BundleStatus::Unknown,
        };
        parse_inflight_status(&body, bundle_id)
    }

    /// Landed-tip floor in lamports. 0 when the oracle is unreachable:
    /// callers fall back to the configured tip.
    pub async fn tip_floor_lamports(&self) -> u64 {
        let now = unix_secs();
        let stale = self.tip_floor_stale_at(now);
        if stale {
            let fresh = self.fetch_tip_floor().await.unwrap_or(0);
            if fresh == 0 {
                // Rare path (oracle down): one warn per TTL window —
                // fetches only happen when stale, so this cannot spam.
                // Floor 0 means the base tip rules unmodified.
                tracing::warn!("jito tip-floor oracle unreachable; using configured tip (underland risk under congestion)");
            }
            if let Ok(mut guard) = self.floor.lock() {
                if fresh > 0 {
                    guard.0 = fresh;
                }
                guard.1 = now;
            }
        }
        self.floor.lock().map(|g| g.0).unwrap_or(0)
    }

    /// True when the cached floor is past [`TIP_FLOOR_TTL_SECS`]
    /// (or never fetched): the next `tip_floor_lamports` refetches.
    /// `TipEscalator` (this module) drives every refetch, so this is
    /// the only caller of the timestamp comparison.
    fn tip_floor_stale_at(&self, now: u64) -> bool {
        self.floor
            .lock()
            .map(|g| now.saturating_sub(g.1) >= TIP_FLOOR_TTL_SECS)
            .unwrap_or(true)
    }

    /// GET the floor oracle. `None` on any trouble.
    async fn fetch_tip_floor(&self) -> Option<u64> {
        let res = match tokio::time::timeout(
            Duration::from_millis(2000),
            self.client.get(TIP_FLOOR_URL).send(),
        )
        .await
        {
            Ok(Ok(res)) if res.status().is_success() => res,
            _ => return None,
        };
        let body = res.text().await.ok()?;
        let floor = parse_tip_floor_lamports(&body);
        if floor == 0 { None } else { Some(floor) }
    }
}

/// One landed-tip percentile row of the floor oracle.
#[derive(Debug, serde::Deserialize)]
struct TipFloorSample {
    #[serde(default)]
    landed_tips_50th_percentile: Option<f64>,
}

/// Pure: median landed tip (SOL) → lamports. 0 on any schema trouble
/// (counted in [`tip_floor_parse_failures`], warned: a silent 0 would
/// hide oracle-shape drift as "no congestion"). An undecodable body
/// counts as drift too, never as a silent blip.
fn parse_tip_floor_lamports(body: &str) -> u64 {
    let sol = serde_json::from_str::<Vec<TipFloorSample>>(body)
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|row| row.landed_tips_50th_percentile)
        .unwrap_or(0.0);
    if !sol.is_finite() || sol <= 0.0 {
        TIP_FLOOR_PARSE_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!("jito tip-floor schema unparsable; treating floor as 0 (unknown)");
        return 0;
    }
    (sol * LAMPORTS_PER_SOL_F) as u64
}

/// Max of the configured base tip and the landed-tip floor.
/// Warns when the floor is unknown (0) while a positive base tip
/// rules: landing under congestion is then unproven.
pub fn apply_tip_floor(base_tip_lamports: u64, floor_lamports: u64) -> u64 {
    if floor_lamports == 0 {
        if base_tip_lamports > 0 {
            tracing::warn!(
                base = base_tip_lamports,
                "tip floor 0 with positive base tip; congestion underland risk unmeasured"
            );
        }
        return base_tip_lamports;
    }
    base_tip_lamports.max(floor_lamports)
}

/// `sendBundle` request. `params[1]` (`{"encoding": "base64"}`) is
/// mandatory: without it the engine assumes base58 and rejects with
/// `-32602`.
#[derive(Debug, serde::Serialize)]
pub struct SendBundleRequest {
    jsonrpc: &'static str,
    id: u8,
    method: &'static str,
    params: (Vec<String>, BundleEncoding),
}

/// The mandatory `params[1]` of `sendBundle`.
#[derive(Debug, serde::Serialize)]
pub struct BundleEncoding {
    encoding: &'static str,
}

/// Build the `sendBundle` request (pure). `params[1]` picks base64.
pub fn bundle_payload(bundle_b64: Vec<String>) -> SendBundleRequest {
    SendBundleRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "sendBundle",
        params: (bundle_b64, BundleEncoding { encoding: "base64" }),
    }
}

/// JSON-RPC request envelope (the wire shape lives here only).
#[derive(Debug, serde::Serialize)]
struct RpcRequest<P: serde::Serialize> {
    jsonrpc: &'static str,
    id: u8,
    method: &'static str,
    params: P,
}

/// `sendBundle` response: engine error object or the bundle UUID.
#[derive(Debug, serde::Deserialize)]
struct SendBundleResponse {
    #[serde(default)]
    error: Option<RpcErrorPayload>,
    #[serde(default)]
    result: Option<String>,
}

/// JSON-RPC error object. Only `code`/`message` are rendered
/// ([`JitoError::Rpc`] text); unknown extras are ignored.
#[derive(Debug, serde::Deserialize)]
struct RpcErrorPayload {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

impl RpcErrorPayload {
    /// Rendered text for [`JitoError::Rpc`] (logs/ops only).
    fn render(&self) -> String {
        match (&self.code, &self.message) {
            (Some(code), Some(message)) => format!("code {code}: {message}"),
            (Some(code), None) => format!("code {code}"),
            (None, Some(message)) => message.clone(),
            (None, None) => "unrecognized error payload".to_string(),
        }
    }
}
/// Pick a tip account from `seed` (no RNG; callers pass nanos/counter).
pub fn select_tip_account(seed: u64) -> Pubkey {
    let idx = (seed as usize) % JITO_TIP_ACCOUNTS.len();
    Pubkey::from_str_const(JITO_TIP_ACCOUNTS[idx])
}

impl JitoClient {
    /// Tip account for this dispatch: live set when fresh, else constants.
    pub async fn tip_account(&self, seed: u64) -> Pubkey {
        self.maybe_refresh_tips().await;
        let guard = self.tips.lock().unwrap_or_else(|e| e.into_inner());
        let list = &guard.0;
        if list.is_empty() {
            return select_tip_account(seed);
        }
        let account = &list[(seed as usize) % list.len()];
        // One bad entry falls back; never panics.
        account
            .parse::<Pubkey>()
            .unwrap_or_else(|_| select_tip_account(seed))
    }

    /// Refresh the tip set when stale. Any failure keeps the old list.
    async fn maybe_refresh_tips(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stale = self
            .tips
            .lock()
            .map(|g| now.saturating_sub(g.1) > TIP_ACCOUNTS_TTL_SECS)
            .unwrap_or(true);
        if !stale {
            return;
        }
        if let Some(fresh) = self.fetch_tip_accounts().await
            && let Ok(mut guard) = self.tips.lock()
        {
            *guard = (fresh, now);
        }
    }

    /// Query `getTipAccounts` on its documented path. `None` on any
    /// failure (warned: a silent pin would hide a dead refresh).
    async fn fetch_tip_accounts(&self) -> Option<Vec<String>> {
        let payload = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "getTipAccounts",
            params: [0u8; 0],
        };
        let url = self.tip_accounts_url();
        let res = match tokio::time::timeout(
            Duration::from_millis(2000),
            self.client.post(&url).json(&payload).send(),
        )
        .await
        {
            Ok(Ok(res)) => res,
            _ => {
                warn!(endpoint = %url, "tip refresh request failed; keeping cached list");
                return None;
            }
        };
        if !res.status().is_success() {
            warn!(endpoint = %url, status = ?res.status(), "tip refresh non-2xx; keeping cached list");
            return None;
        }
        let body: TipAccountsResponse = match res.json().await {
            Ok(body) => body,
            Err(e) => {
                warn!(endpoint = %url, error = %e, "tip refresh decode failed; keeping cached list");
                return None;
            }
        };
        let raw_accounts = body.result?;
        let accounts: Vec<String> = raw_accounts
            .into_iter()
            // Skip bad entries; empty keeps the old list.
            .filter(|s| s.parse::<Pubkey>().is_ok_and(|p| p.to_string() == *s))
            .collect();
        if accounts.is_empty() {
            return None;
        }
        // A fully rotated but valid list still heals.
        let overlaps = accounts
            .iter()
            .any(|a| JITO_TIP_ACCOUNTS.contains(&a.as_str()));
        if !overlaps {
            warn!("getTipAccounts rotated fully; adopting fresh list");
        }
        debug!(count = accounts.len(), "refreshed Jito tip accounts");
        Some(accounts)
    }
}

/// `getTipAccounts` response: the validated-address list.
#[derive(Debug, serde::Deserialize)]
struct TipAccountsResponse {
    #[serde(default)]
    result: Option<Vec<String>>,
}

/// Map an inflight-status response to our bundle state (pure).
/// Entries for other bundles are skipped; anything unrecognized is
/// `Unknown` (callers keep their existing behavior, never terminal).
fn parse_inflight_status(body: &str, bundle_id: &str) -> BundleStatus {
    let Ok(response) = serde_json::from_str::<InflightBundleStatuses>(body) else {
        return BundleStatus::Unknown;
    };
    let Some(entries) = response.result.and_then(|r| r.value) else {
        return BundleStatus::Unknown;
    };
    let Some(entry) = entries
        .iter()
        .find(|e| e.bundle_id.as_deref() == Some(bundle_id))
    else {
        return BundleStatus::Unknown;
    };
    match entry.status.as_deref() {
        Some("Landed") => BundleStatus::Landed,
        Some("Pending") => BundleStatus::Pending,
        Some("Failed") => BundleStatus::Failed,
        Some("Invalid") => BundleStatus::Invalid,
        _ => BundleStatus::Unknown,
    }
}

/// `getInflightBundleStatuses` response.
#[derive(Debug, serde::Deserialize)]
struct InflightBundleStatuses {
    #[serde(default)]
    result: Option<InflightBundleStatusesResult>,
}

#[derive(Debug, serde::Deserialize)]
struct InflightBundleStatusesResult {
    #[serde(default)]
    value: Option<Vec<InflightBundleStatus>>,
}

/// One engine bundle record (`landed_slot` and friends are ignored).
#[derive(Debug, serde::Deserialize)]
struct InflightBundleStatus {
    #[serde(default)]
    bundle_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// System transfer ix: from writable+signer.
pub fn tip_instruction(payer: &Pubkey, tip_account: &Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&SYSTEM_TRANSFER_DISCRIMINATOR.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: crate::instructions::SYSTEM_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*tip_account, false),
        ],
        data,
    }
}

/// Encode signed txs for `sendBundle`: bincode -> base64.
/// Rejects empty and over-limit bundles (engine takes at most 5).
pub fn encode_bundle(txs: &[Transaction]) -> Result<Vec<String>, JitoError> {
    if txs.is_empty() || txs.len() > MAX_BUNDLE_TXS {
        return Err(JitoError::Encode(format!(
            "bundle holds {} txs; engine limit is {MAX_BUNDLE_TXS}",
            txs.len()
        )));
    }
    txs.iter()
        .map(|tx| {
            bincode::serialize(tx)
                .map(|bytes| BASE64_STANDARD.encode(bytes))
                .map_err(|e| JitoError::Encode(e.to_string()))
        })
        .collect()
}

/// Nanos seed for [`select_tip_account`], mixed with a process
/// counter so two dispatches in the same sub-second slice spread
/// across tip accounts instead of colliding.
pub fn nanos_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    nanos ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Canned HTTP server: records request lines, replays `responses`.
    async fn serve_scripted(
        paths: Arc<Mutex<Vec<String>>>,
        responses: Vec<(u16, &'static str)>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                paths.lock().expect("paths").push(line);
                let reason = match status {
                    200 => "OK",
                    429 => "Too Many Requests",
                    _ => "Error",
                };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn inflight_body(status: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","result":{{"context":{{"slot":1}},"value":[{{"bundle_id":"B1","status":"{status}","landed_slot":null}}]}},"id":1}}"#
        )
    }

    #[test]
    fn bundle_payload_carries_base64_encoding() {
        // Without params[1] the engine decodes base58 and refuses (-32602).
        let payload =
            serde_json::to_value(bundle_payload(vec!["dGVzdA==".to_string()])).expect("wire");
        assert_eq!(payload["method"], "sendBundle");
        let params = payload["params"].as_array().expect("params array");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0][0], "dGVzdA==");
        assert_eq!(params[1]["encoding"], "base64");
    }

    #[test]
    fn bundle_payload_preserves_all_txs_with_encoding() {
        // Multi-tx bundles must keep order and still carry the mandatory
        // base64 marker (the skill doc once omitted it for every bundle).
        let txs = vec!["eDE=".to_string(), "eDI=".to_string(), "eDM=".to_string()];
        let payload = serde_json::to_value(bundle_payload(txs.clone())).expect("wire");
        let params = payload["params"].as_array().expect("params array");
        assert_eq!(params.len(), 2);
        let wire = params[0].as_array().expect("tx array");
        assert_eq!(wire.len(), 3);
        for (i, tx) in txs.iter().enumerate() {
            assert_eq!(&wire[i], tx, "tx order must survive payload build");
        }
        assert_eq!(params[1]["encoding"], "base64");
        // Empty bundle keeps the shape (dispatch rejects empty upstream).
        let empty = serde_json::to_value(bundle_payload(Vec::new())).expect("wire");
        assert_eq!(empty["params"][1]["encoding"], "base64");
    }

    #[test]
    fn tip_accounts_are_valid_pubkeys() {
        for account in JITO_TIP_ACCOUNTS {
            select_tip_account(0);
            assert_eq!(Pubkey::from_str_const(account).to_string(), account);
        }
    }

    #[test]
    fn selection_covers_all_accounts() {
        let mut seen = [false; 8];
        for seed in 0..16u64 {
            let picked = select_tip_account(seed);
            let idx = JITO_TIP_ACCOUNTS
                .iter()
                .position(|a| *a == picked.to_string())
                .expect("known");
            seen[idx] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    #[test]
    fn tip_instruction_shape() {
        let payer = Pubkey::new_from_array([1; 32]);
        let tip = select_tip_account(2);
        let ix = tip_instruction(&payer, &tip, 100_000);
        assert_eq!(ix.program_id, crate::instructions::SYSTEM_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 2);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert!(!ix.accounts[1].is_signer && ix.accounts[1].is_writable);
        assert_eq!(ix.data.len(), 12);
        assert_eq!(u32::from_le_bytes(ix.data[..4].try_into().expect("u32")), 2);
        assert_eq!(
            u64::from_le_bytes(ix.data[4..].try_into().expect("u64")),
            100_000
        );
    }

    #[test]
    fn rejects_bad_endpoint() {
        assert!(matches!(
            JitoClient::new("notaurl"),
            Err(JitoError::BadUrl(_))
        ));
        assert!(matches!(
            JitoClient::new("wss://example.com"),
            Err(JitoError::BadUrl(_))
        ));
        let client =
            JitoClient::new("https://mainnet.block-engine.jito.wtf").expect("ok");
        // One base, one documented path per method.
        assert!(client.bundles_url().ends_with("/api/v1/bundles"));
        assert!(client.tip_accounts_url().ends_with("/api/v1/getTipAccounts"));
        assert!(
            client
                .inflight_status_url()
                .ends_with("/api/v1/getInflightBundleStatuses")
        );
    }

    #[test]
    fn encode_bundle_rejects_empty_and_oversize() {
        assert!(matches!(
            encode_bundle(&[]),
            Err(JitoError::Encode(_))
        ));
        let six = vec![Transaction::default(); 6];
        assert!(matches!(
            encode_bundle(&six),
            Err(JitoError::Encode(_))
        ));
        let one = vec![Transaction::default(); 1];
        assert_eq!(encode_bundle(&one).expect("one tx").len(), 1);
    }

    #[test]
    fn inflight_response_maps_to_status() {
        for (status, want) in [
            ("Landed", BundleStatus::Landed),
            ("Pending", BundleStatus::Pending),
            ("Failed", BundleStatus::Failed),
            ("Invalid", BundleStatus::Invalid),
        ] {
            assert_eq!(
                parse_inflight_status(&inflight_body(status), "B1"),
                want,
                "{status}"
            );
        }
        // Wrong bundle, unknown state, and schema breaks stay Unknown
        // (callers keep polling the signature, never terminal).
        assert_eq!(
            parse_inflight_status(&inflight_body("Pending"), "B9"),
            BundleStatus::Unknown
        );
        assert_eq!(
            parse_inflight_status(&inflight_body("Weird"), "B1"),
            BundleStatus::Unknown
        );
        assert_eq!(parse_inflight_status("{}", "B1"), BundleStatus::Unknown);
    }

    #[test]
    fn tip_floor_parses_median_sol_to_lamports() {
        assert_eq!(
            parse_tip_floor_lamports(
                r#"[{"time":"2024-09-01T12:58:00Z","landed_tips_25th_percentile":6e-06,"landed_tips_50th_percentile":1e-05,"landed_tips_75th_percentile":3.6e-05}]"#,
            ),
            10_000
        );
        for bad in [
            "[]",
            "[{}]",
            "{}",
            "null",
            r#"[{"landed_tips_50th_percentile":-1.0}]"#,
            r#"[{"landed_tips_50th_percentile":"much"}]"#,
        ] {
            assert_eq!(parse_tip_floor_lamports(bad), 0, "{bad}");
        }
    }

    #[tokio::test]
    async fn inflight_status_posts_to_own_path() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let pending = inflight_body("Pending");
        let failed = inflight_body("Failed");
        let leaked: Vec<(u16, &'static str)> = vec![
            (200, Box::leak(pending.into_boxed_str())),
            (200, Box::leak(failed.into_boxed_str())),
        ];
        let base = serve_scripted(Arc::clone(&paths), leaked).await;
        let client = JitoClient::new(&base).expect("client");
        assert_eq!(client.inflight_status("B1").await, BundleStatus::Pending);
        assert_eq!(client.inflight_status("B1").await, BundleStatus::Failed);
        let paths = paths.lock().expect("paths");
        assert_eq!(paths.len(), 2);
        for line in paths.iter() {
            assert!(
                line.contains("POST /api/v1/getInflightBundleStatuses "),
                "unexpected path: {line}"
            );
        }
    }

    #[tokio::test]
    async fn send_bundle_maps_429_to_rate_limited() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let base = serve_scripted(Arc::clone(&paths), vec![(429, "slow down")]).await;
        let client = JitoClient::new(&base).expect("client");
        let err = client
            .send_bundle(vec!["eA==".to_string()])
            .await
            .expect_err("must fail");
        assert!(matches!(err, JitoError::RateLimited));
        let paths = paths.lock().expect("paths");
        assert_eq!(paths.len(), 1);
        assert!(paths[0].contains("POST /api/v1/bundles "), "{}", paths[0]);
    }

    #[tokio::test]
    async fn tip_refresh_posts_to_own_path_and_heals() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let body = format!(
            r#"{{"jsonrpc":"2.0","result":["{}","{}"],"id":1}}"#,
            JITO_TIP_ACCOUNTS[0], JITO_TIP_ACCOUNTS[1]
        );
        let base = serve_scripted(
            Arc::clone(&paths),
            vec![(200, Box::leak(body.into_boxed_str()))],
        )
        .await;
        let client = JitoClient::new(&base).expect("client");
        // Fresh client cache is stale (fetched=0): refresh fires.
        let picked = client.tip_account(1).await.to_string();
        assert!(
            picked == JITO_TIP_ACCOUNTS[0] || picked == JITO_TIP_ACCOUNTS[1],
            "unexpected tip: {picked}"
        );
        let paths = paths.lock().expect("paths");
        assert_eq!(paths.len(), 1);
        assert!(paths[0].contains("POST /api/v1/getTipAccounts "), "{}", paths[0]);
    }
}
