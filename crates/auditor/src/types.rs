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
    /// Response body exceeded [`MAX_BODY_BYTES`]: a rogue server must not
    /// turn the audit path into a retry loop. Never retryable (unlike
    /// [`RugCheckError::Deserialization`], which covers genuinely
    /// unparseable-but-small bodies during indexing latency).
    #[error("RugCheck response body exceeds size cap")]
    ResponseTooLarge,
    /// Token rejected: {0}
    #[error("Token rejected: {0}")]
    SecurityRejection(String),
    /// Client misconfiguration (bad API root): {0}
    #[error("bad RugCheck config: {0}")]
    BadUrl(String),
}

impl RugCheckError {
    /// Retryable at the pipeline watchlist: transport trouble, rate
    /// limits, server errors, unparseable bodies, and HTTP 400. A 400
    /// on a seconds-old mint almost always means "not indexed yet"
    /// (our requests are fixed-format, so a malformed-request 400 is
    /// not a live case) — dropping it instantly burns real launches.
    /// Only policy rejections and client misconfiguration drop at once.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::SecurityRejection(_) | Self::BadUrl(_) | Self::ResponseTooLarge => false,
            Self::Network(_) | Self::Deserialization(_) => true,
            Self::HttpStatus(s) => {
                s.is_server_error()
                    || *s == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || *s == reqwest::StatusCode::REQUEST_TIMEOUT
                    || *s == reqwest::StatusCode::BAD_REQUEST
            }
        }
    }
}

/// Risk report for one mint, as returned by
/// `GET /tokens/{mint}/report`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RugCheckReport {
    /// Token mint address (base58).
    pub mint: String,
    /// Aggregate risk score; higher is riskier. Required: a missing
    /// score must fail the fetch (fail-closed), never parse as 0 and
    /// sail through the score gate.
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
    /// Severity: `warn` or `danger`. Required in spirit but tolerant in
    /// parsing: an unknown level parses fine and is rejected by policy
    /// (fail-closed there, where the allowlist lives).
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
            serde_json::from_str(r#"{"mint": "ABC", "score": 0}"#).expect("minimal JSON");
        assert_eq!(report.mint, "ABC");
        assert_eq!(report.score, 0);
        assert!(report.risks.is_empty());
        assert!(report.token.is_none());
    }

    #[test]
    fn missing_score_fails_closed() {
        // No score must not parse as 0 (a free pass through the gate).
        let err = serde_json::from_str::<RugCheckReport>(r#"{"mint": "ABC"}"#).expect_err("must fail");
        assert!(err.to_string().contains("score"));
    }

    #[test]
    fn retryable_covers_indexing_latency_not_policy() {
        use reqwest::StatusCode;
        // Fresh-mint 400s, rate limits, server errors, transport and
        // unparseable bodies: retry at the watchlist, never drop at 0s.
        assert!(RugCheckError::HttpStatus(StatusCode::BAD_REQUEST).is_retryable());
        assert!(RugCheckError::HttpStatus(StatusCode::TOO_MANY_REQUESTS).is_retryable());
        assert!(RugCheckError::HttpStatus(StatusCode::REQUEST_TIMEOUT).is_retryable());
        assert!(RugCheckError::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR).is_retryable());
        assert!(RugCheckError::Deserialization(
            serde_json::from_str::<u32>("bogus").expect_err("must fail")
        )
        .is_retryable());
        // Policy, config, and oversize bodies: drop at once, never queue.
        assert!(!RugCheckError::SecurityRejection("rug".to_string()).is_retryable());
        assert!(!RugCheckError::BadUrl(":::".to_string()).is_retryable());
        assert!(!RugCheckError::ResponseTooLarge.is_retryable());
        assert!(!RugCheckError::HttpStatus(StatusCode::FORBIDDEN).is_retryable());
    }
}
