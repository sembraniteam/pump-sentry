//! RugCheck report types (see skill `solana-rugcheck-api`).
//!
//! Shapes follow the skill contract. `#[serde(default)]` keeps parsing
//! tolerant when the API omits fields; unknown fields are ignored.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Token audit failures — transport errors vs policy rejections.
#[derive(Error, Debug)]
pub enum RugCheckError {
    /// HTTP request failed (timeout, connect, TLS, retry budget spent).
    #[error("HTTP request failed: {0}")]
    Network(#[from] reqwest::Error),
    /// RugCheck API error status: {0}
    #[error("RugCheck API error status: {0}")]
    HttpStatus(reqwest::StatusCode),
    /// Failed to parse JSON response: {0}
    #[error("Failed to parse JSON response: {0}")]
    Deserialization(#[from] serde_json::Error),
    /// Token rejected: {0}
    #[error("Token rejected: {0}")]
    SecurityRejection(String),
}

/// Risk report for one mint, as returned by
/// `GET /tokens/{mint}/report`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RugCheckReport {
    /// Token mint address (base58).
    pub mint: String,
    /// Aggregate risk score; higher is riskier.
    #[serde(default)]
    pub score: u32,
    /// Individual risk findings.
    #[serde(default)]
    pub risks: Vec<RiskItem>,
    /// On-chain token metadata, when available.
    #[serde(default)]
    pub token: Option<TokenInfo>,
}

/// One risk finding inside a report.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RiskItem {
    /// Machine-readable risk name.
    pub name: String,
    /// Observed value that triggered the finding.
    #[serde(default)]
    pub value: String,
    /// Human-readable explanation.
    #[serde(default)]
    pub description: String,
    /// Risk contribution in points.
    #[serde(default)]
    pub score: u32,
    /// Severity: `warn` or `danger`.
    #[serde(default)]
    pub level: String,
}

/// On-chain token state relevant to rug risk.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenInfo {
    /// Mint authority address, if still enabled.
    #[serde(default)]
    pub mint_authority: Option<String>,
    /// Freeze authority address, if still enabled.
    #[serde(default)]
    pub freeze_authority: Option<String>,
    /// Total supply in base units.
    #[serde(default)]
    pub supply: u64,
    /// Mint decimals.
    #[serde(default)]
    pub decimals: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "mint": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
        "score": 120,
        "risks": [
            {"name": "Low Liquidity", "value": "500", "description": "Low LP", "score": 120, "level": "warn"}
        ],
        "token": {"mint_authority": null, "freeze_authority": null, "supply": 1000000000, "decimals": 6}
    }"#;

    #[test]
    fn deserializes_skill_shaped_report() {
        let report: RugCheckReport = serde_json::from_str(SAMPLE).expect("valid JSON");
        assert_eq!(report.score, 120);
        assert_eq!(report.risks.len(), 1);
        assert_eq!(report.risks[0].level, "warn");
        let token = report.token.expect("token present");
        assert!(token.mint_authority.is_none());
        assert_eq!(token.decimals, 6);
    }

    #[test]
    fn tolerates_missing_fields() {
        let report: RugCheckReport =
            serde_json::from_str(r#"{"mint": "ABC"}"#).expect("minimal JSON");
        assert_eq!(report.mint, "ABC");
        assert_eq!(report.score, 0);
        assert!(report.risks.is_empty());
        assert!(report.token.is_none());
    }
}
