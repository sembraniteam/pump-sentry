---
name: jito-mev-execution
description: Robust Jito JSON-RPC bundle building, dynamic tipping, and private transaction routing in Rust.
license: MIT
compatibility: opencode
metadata:
  domain: mev-execution
  language: rust
  provider: jito
---

## Purpose

Provides a production-grade Jito bundle builder and JSON-RPC dispatch client in Rust for front-running protection.

## Jito Block Engine Endpoints

- Amsterdam: `https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles`
- Frankfurt: `https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles`
- New York: `https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles`
- Tokyo: `https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles`

## Designated Jito Tip Accounts

Randomly select one tip recipient per bundle:

1. `96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5`
2. `HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe`
3. `Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY`
4. `ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49`

## Bundle Client Implementation (Rust)

```rust
use reqwest::Client;
use serde_json::json;
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    system_instruction,
};
use std::{str::FromStr, time::Duration};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum JitoError {
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("bad Jito endpoint: {0}")]
    BadUrl(String),
}

pub const JITO_TIP_ACCOUNTS: [&str; 4] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
];

pub struct JitoClient {
    client: Client,
    endpoint: String,
}

impl JitoClient {
    /// Returns the reason instead of `None` (fail fast at startup, never
    /// half-enable live dispatch).
    pub fn new(region_base_url: &str) -> Result<Self, JitoError> {
        match url::Url::parse(region_base_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(JitoError::BadUrl(region_base_url.to_string())),
        }
        let client = Client::builder()
            .timeout(Duration::from_millis(2000))
            .build()?;
        Ok(Self {
            client,
            endpoint: format!("{region_base_url}/api/v1/bundles"),
            tips: std::sync::Arc::new(std::sync::Mutex::new((
                JITO_TIP_ACCOUNTS.iter().map(|s| s.to_string()).collect(),
                0,
            ))),
        })
    }

    pub fn create_tip_instruction(
        payer: &Pubkey,
        tip_lamports: u64,
        account_idx: usize,
    ) -> Instruction {
        let tip_pubkey = Pubkey::from_str(
            JITO_TIP_ACCOUNTS[account_idx % JITO_TIP_ACCOUNTS.len()],
        ).unwrap();
        system_instruction::transfer(payer, &tip_pubkey, tip_lamports)
    }

    pub async fn send_bundle(&self, bundle_b64: Vec<String>) -> Result<String, JitoError> {
        // NOTE (verified live 2026-10-11): params[1] MUST carry
        // {"encoding": "base64"}. The engine decodes base58 by default, so
        // a bare [txs] params array makes it reject every bundle with
        // -32602 "transaction #0 could not be decoded".
        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [bundle_b64, {"encoding": "base64"}]
        });

        let res = self.client.post(&self.endpoint).json(&payload).send().await?;
        let body: serde_json::Value = res.json().await?;

        if let Some(err) = body.get("error") {
            return Err(JitoError::Rpc(err.to_string()));
        }

        Ok(body["result"].as_str().unwrap_or("").to_string())
    }
}

/// Tip accounts refresh hourly from `getTipAccounts` (2s budget, silent
/// fallback to the compiled rotation). Post to the SAME bundles endpoint —
/// Jito routes by the `method` field, not the path (posting to the bare base
/// URL returns nothing and silently pins the fallback forever). Trust-but-verify:
/// a fetched set with zero overlap against the compiled rotation is rejected
/// (hijack/stale guard) instead of poisoning tips for an hour.
```
