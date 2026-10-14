//! Jito bundles for frontrunning protection.
//!
//! Signed tx -> bincode -> base64 -> `sendBundle`. The tip is a plain
//! system transfer in the same tx, so a landed bundle pays.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use reqwest::Client;
use serde_json::json;
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
}

/// Tip-list TTL: 15 min; failures keep the old list.
pub const TIP_ACCOUNTS_TTL_SECS: u64 = 900;

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
        debug!(endpoint = %url, txs = payload["params"][0].as_array().map(|a| a.len()).unwrap_or(0), "submitting bundle");
        let res = self.client.post(&url).json(&payload).send().await?;
        if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            warn!(endpoint = %url, "bundle send rate-limited (1 req/s/IP/region)");
            return Err(JitoError::RateLimited);
        }
        let body: serde_json::Value = res.json().await?;
        if let Some(err) = body.get("error") {
            return Err(JitoError::Rpc(err.to_string()));
        }
        let uuid = body["result"].as_str().unwrap_or("").to_string();
        info!(bundle = %uuid, "bundle accepted");
        Ok(uuid)
    }

    /// Engine-side bundle state. `Unknown` on any transport/parse
    /// trouble: callers keep polling the signature instead.
    pub async fn inflight_status(&self, bundle_id: &str) -> BundleStatus {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getInflightBundleStatuses",
            "params": [[bundle_id]],
        });
        let res = match tokio::time::timeout(
            Duration::from_millis(2000),
            self.client.post(self.inflight_status_url()).json(&payload).send(),
        )
        .await
        {
            Ok(Ok(res)) => res,
            _ => return BundleStatus::Unknown,
        };
        let body: serde_json::Value = match res.json().await {
            Ok(body) => body,
            Err(_) => return BundleStatus::Unknown,
        };
        parse_inflight_status(&body, bundle_id)
    }
}

/// Build the `sendBundle` payload (pure). `params[1]` picks base64.
pub fn bundle_payload(bundle_b64: Vec<String>) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [bundle_b64, {"encoding": "base64"}],
    })
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
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTipAccounts",
            "params": [],
        });
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
        let body: serde_json::Value = match res.json().await {
            Ok(body) => body,
            Err(e) => {
                warn!(endpoint = %url, error = %e, "tip refresh decode failed; keeping cached list");
                return None;
            }
        };
        let accounts: Vec<String> = body
            .get("result")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
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

/// Map an inflight-status response to our bundle state (pure).
/// Entries for other bundles are skipped; anything unrecognized is
/// `Unknown` (callers keep their existing behavior, never terminal).
fn parse_inflight_status(body: &serde_json::Value, bundle_id: &str) -> BundleStatus {
    let entries = body
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_array());
    let Some(entries) = entries else {
        return BundleStatus::Unknown;
    };
    for entry in entries {
        if entry.get("bundle_id").and_then(|b| b.as_str()) != Some(bundle_id) {
            continue;
        }
        return match entry.get("status").and_then(|s| s.as_str()) {
            Some("Landed") => BundleStatus::Landed,
            Some("Pending") => BundleStatus::Pending,
            Some("Failed") => BundleStatus::Failed,
            Some("Invalid") => BundleStatus::Invalid,
            _ => BundleStatus::Unknown,
        };
    }
    BundleStatus::Unknown
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
        let payload = bundle_payload(vec!["dGVzdA==".to_string()]);
        assert_eq!(payload["method"], "sendBundle");
        let params = payload["params"].as_array().expect("params array");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0][0], "dGVzdA==");
        assert_eq!(params[1]["encoding"], "base64");
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
            let body: serde_json::Value =
                serde_json::from_str(&inflight_body(status)).expect("json");
            assert_eq!(parse_inflight_status(&body, "B1"), want, "{status}");
        }
        // Wrong bundle, unknown state, and schema breaks stay Unknown
        // (callers keep polling the signature, never terminal).
        let body: serde_json::Value =
            serde_json::from_str(&inflight_body("Pending")).expect("json");
        assert_eq!(parse_inflight_status(&body, "B9"), BundleStatus::Unknown);
        let body: serde_json::Value =
            serde_json::from_str(&inflight_body("Weird")).expect("json");
        assert_eq!(parse_inflight_status(&body, "B1"), BundleStatus::Unknown);
        let body: serde_json::Value = serde_json::from_str("{}").expect("json");
        assert_eq!(parse_inflight_status(&body, "B1"), BundleStatus::Unknown);
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
