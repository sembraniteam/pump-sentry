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

/// Designated Jito tip accounts.
pub const JITO_TIP_ACCOUNTS: [&str; 4] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
];

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
    #[error("bad Jito endpoint: {0}")]
    BadUrl(String),
}

/// Regional block-engine bundle client.
#[derive(Clone)]
pub struct JitoClient {
    client: Client,
    endpoint: String,
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
        let client = Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()
            .map_err(JitoError::Network)?;
        Ok(Self {
            client,
            endpoint: format!("{region_base_url}/api/v1/bundles"),
            tips: std::sync::Arc::new(std::sync::Mutex::new((
                JITO_TIP_ACCOUNTS.iter().map(|s| s.to_string()).collect(),
                0,
            ))),
        })
    }

    /// Bundle endpoint in use (for logs). Test-only: production code
    /// logs `self.endpoint` via the `send_bundle` debug line instead.
    #[cfg(test)]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Submit pre-encoded base64 txs. Returns the bundle UUID.
    ///
    /// `params[1]` (`{"encoding": "base64"}`) is mandatory: without it the
    /// engine assumes base58 and rejects with `-32602`.
    pub async fn send_bundle(&self, bundle_b64: Vec<String>) -> Result<String, JitoError> {
        let payload = bundle_payload(bundle_b64);
        debug!(endpoint = %self.endpoint, txs = payload["params"][0].as_array().map(|a| a.len()).unwrap_or(0), "submitting bundle");
        let res = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await?;
        let body: serde_json::Value = res.json().await?;
        if let Some(err) = body.get("error") {
            return Err(JitoError::Rpc(err.to_string()));
        }
        let uuid = body["result"].as_str().unwrap_or("").to_string();
        info!(bundle = %uuid, "bundle accepted");
        Ok(uuid)
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

    /// Query `getTipAccounts`. `None` on any failure.
    /// Posts to the same endpoint: Jito routes by `method`, not path.
    async fn fetch_tip_accounts(&self) -> Option<Vec<String>> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTipAccounts",
            "params": [],
        });
        let res = match tokio::time::timeout(
            Duration::from_millis(2000),
            self.client.post(&self.endpoint).json(&payload).send(),
        )
        .await
        {
            Ok(Ok(res)) => res,
            _ => return None,
        };
        let body: serde_json::Value = res.json().await.ok()?;
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
pub fn encode_bundle(txs: &[Transaction]) -> Result<Vec<String>, JitoError> {
    txs.iter()
        .map(|tx| {
            bincode::serialize(tx)
                .map(|bytes| BASE64_STANDARD.encode(bytes))
                .map_err(|e| JitoError::Encode(e.to_string()))
        })
        .collect()
}

/// Nanos seed for [`select_tip_account`].
pub fn nanos_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut seen = [false; 4];
        for seed in 0..8u64 {
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
        assert!(client.endpoint().ends_with("/api/v1/bundles"));
    }
}
