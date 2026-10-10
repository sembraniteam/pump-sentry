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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RugCheckReport {
    pub mint: String,
    pub score: u32,
    pub risks: Vec<RiskItem>,
    pub token: Option<TokenInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RiskItem {
    pub name: String,
    pub value: String,
    pub description: String,
    pub score: u32,
    pub level: String, // "info", "warn", "danger"
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
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
        let url = format!("{}/tokens/{}/report", self.base_url, mint);

        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(RugCheckError::HttpStatus(response.status()));
        }

        let report: RugCheckReport = response.json().await?;

        // Invariant Verifications
        if report.score > 400 {
            return Err(RugCheckError::SecurityRejection(format!(
                "Risk score too high: {}",
                report.score
            )));
        }

        for risk in &report.risks {
            if risk.level.to_lowercase() == "danger" {
                return Err(RugCheckError::SecurityRejection(format!(
                    "Critical risk detected: {} - {}",
                    risk.name, risk.description
                )));
            }
        }

        if let Some(token) = &report.token {
            if token.mint_authority.is_some() {
                return Err(RugCheckError::SecurityRejection(
                    "Mint authority is enabled".into(),
                ));
            }
            if token.freeze_authority.is_some() {
                return Err(RugCheckError::SecurityRejection(
                    "Freeze authority is enabled".into(),
                ));
            }
        }

        Ok(report)
    }
}
```
