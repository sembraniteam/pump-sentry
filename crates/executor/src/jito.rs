//! Jito bundle dispatch for frontrunning protection (skill
//! `jito-mev-execution`).
//!
//! Flow: fully-signed transaction → bincode → base64 → `sendBundle`
//! against a regional block engine. The tip from
//! [`BuyOrder::tip_lamports`](crate::BuyOrder::tip_lamports) travels as a
//! plain system transfer to a randomly selected tip account *inside* the
//! same transaction, so a landed bundle always pays its tip.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use reqwest::Client;
use serde_json::json;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, info};

/// Designated Jito tip accounts (skill `jito-mev-execution`).
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
    /// HTTP transport failure.
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    /// Block engine returned a JSON-RPC error.
    #[error("RPC error: {0}")]
    Rpc(String),
    /// Transaction serialization failed.
    #[error("Bundle encoding failed: {0}")]
    Encode(String),
}

/// Regional block-engine bundle client.
#[derive(Clone)]
pub struct JitoClient {
    client: Client,
    endpoint: String,
}

impl JitoClient {
    /// Build against a regional base URL, e.g.
    /// `https://frankfurt.mainnet.block-engine.jito.wtf`.
    /// Returns `None` on malformed URLs (fail fast at startup).
    pub fn new(region_base_url: &str) -> Option<Self> {
        match url::Url::parse(region_base_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return None,
        }
        let client = Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()
            .ok()?;
        Some(Self {
            client,
            endpoint: format!("{region_base_url}/api/v1/bundles"),
        })
    }

    /// Bundle endpoint in use (for logs).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Submit pre-encoded base64 transactions as one bundle.
    /// Returns the engine's bundle UUID on acceptance.
    pub async fn send_bundle(&self, bundle_b64: Vec<String>) -> Result<String, JitoError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [bundle_b64],
        });
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

/// Pick a tip account deterministically from `seed` (no RNG dependency;
/// callers pass nanos or a counter).
pub fn select_tip_account(seed: u64) -> Pubkey {
    let idx = (seed as usize) % JITO_TIP_ACCOUNTS.len();
    Pubkey::from_str_const(JITO_TIP_ACCOUNTS[idx])
}

/// System transfer instruction (built by hand to avoid another sysvar
/// crate): `[u32 LE 2, u64 LE lamports]`, from writable+signer.
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

/// Encode signed transactions the way `sendBundle` expects: bincode →
/// base64 (standard alphabet).
pub fn encode_bundle(txs: &[Transaction]) -> Result<Vec<String>, JitoError> {
    txs.iter()
        .map(|tx| {
            bincode::serialize(tx)
                .map(|bytes| BASE64_STANDARD.encode(bytes))
                .map_err(|e| JitoError::Encode(e.to_string()))
        })
        .collect()
}

/// Nanos seed for [`select_tip_account`] (mirrors the backoff jitter).
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
    fn tip_accounts_are_valid_pubkeys() {
        for account in JITO_TIP_ACCOUNTS {
            select_tip_account(0);
            assert!(Pubkey::from_str_const(account).to_string() == account);
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
        assert!(JitoClient::new("notaurl").is_none());
        assert!(JitoClient::new("wss://example.com").is_none());
        let client =
            JitoClient::new("https://frankfurt.mainnet.block-engine.jito.wtf").expect("ok");
        assert!(client.endpoint().ends_with("/api/v1/bundles"));
    }
}
