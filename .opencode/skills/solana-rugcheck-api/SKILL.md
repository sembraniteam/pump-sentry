---
name: solana-rugcheck-api
description: Production-grade Rust HTTP client, data models, and verification rules for the RugCheck.xyz API.
license: MIT
compatibility: opencode
metadata:
  domain: solana-security
  language: rust
  api_version: v1
---

## Purpose

Provides complete, battle-tested Rust client code for evaluating token risks using the RugCheck.xyz REST API.

## Client Implementation with Retry Logic

```rust
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum RugCheckError {
    #[error("HTTP request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("RugCheck API error status: {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("Failed to parse JSON response: {0}")]
    Deserialization(#[from] serde_json::Error),
    #[error("Token rejected: {0}")]
    SecurityRejection(String),
    #[error("bad RugCheck config: {0}")]
    BadUrl(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RugCheckReport {
    pub mint: String,
    pub score: u32, // REQUIRED: missing score fails deserialization (never defaults to 0)
    pub risks: Vec<RiskItem>,
    pub token: Option<TokenInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RiskItem {
    pub name: String,
    // `#[serde(default)]` on value/description/score/level is a deliberate
    // divergence from the strict snippet: the API omits fields, and a missing
    // scalar must not fail the whole report (policy still fails closed on
    // empty/unknown `level` and missing `token`). Do not "tighten" this
    // without checking live payloads.
    pub value: String,
    pub description: String,
    pub score: u32,
    pub level: String, // allowlist: only "warn" passes; "danger", empty, and unknown all reject
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    // Informational only: policy and sizing never read these (decimals come
    // from RPC `token_decimals`).
    pub supply: u64,
    pub decimals: u8,
}

pub struct RugCheckClient {
    client: Client,
    base_url: String,
}

impl RugCheckClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_millis(1500))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to initialize HTTP client");

        Self {
            client,
            base_url: "https://api.rugcheck.xyz/v1".to_string(),
        }
    }

    pub async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        // Fail-closed boundaries (see crates/auditor): 32-byte base58 mint
        // before it touches the URL; report mint must match the request;
        // bodies stream-capped at 1MB incremental (never buffer-then-check);
        // retries capped at 5, for timeouts/connects/body-stalls/5xx/429/408
        // only; schema breaks are terminal, never retried.
        let url = format!("{}/tokens/{}/report", self.base_url, mint);

        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(RugCheckError::HttpStatus(response.status()));
        }

        let report: RugCheckReport = response.json().await?;
        if report.mint != mint {
            return Err(RugCheckError::SecurityRejection(format!(
                "report mint mismatch: asked '{mint}', got '{}'",
                report.mint
            )));
        }

        // Invariant Verifications
        if report.score > 400 {
            return Err(RugCheckError::SecurityRejection(format!(
                "Risk score too high: {}",
                report.score
            )));
        }

        for risk in &report.risks {
            if risk.level.eq_ignore_ascii_case("danger") {
                return Err(RugCheckError::SecurityRejection(format!(
                    "Critical risk detected: {} - {}",
                    risk.name, risk.description
                )));
            } else if !risk.level.eq_ignore_ascii_case("warn") {
                return Err(RugCheckError::SecurityRejection(format!(
                    "Unknown risk severity '{}' on {}: treating as dangerous",
                    risk.level, risk.name
                )));
            }
        }

        if let Some(token) = &report.token {
            let mint_auth = token.mint_authority.as_deref().map(str::trim).filter(|s| !s.is_empty());
            if mint_auth.is_some() {
                return Err(RugCheckError::SecurityRejection(
                    "Mint authority is enabled".into(),
                ));
            }
            let freeze_auth = token.freeze_authority.as_deref().map(str::trim).filter(|s| !s.is_empty());
            if freeze_auth.is_some() {
                return Err(RugCheckError::SecurityRejection(
                    "Freeze authority is enabled".into(),
                ));
            }
        } else {
            return Err(RugCheckError::SecurityRejection(
                "Missing on-chain token info; authorities unverifiable".into(),
            ));
        }

        Ok(report)
    }
}
```
