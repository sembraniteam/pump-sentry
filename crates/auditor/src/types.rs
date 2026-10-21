//! RugCheck report types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Audit failures: transport vs policy.
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
    /// Response body exceeded [`MAX_BODY_BYTES`]. Never retryable.
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
    /// Retryable throttle (429): indexing latency, not policy.
    /// Helper for pipeline stubs; production surfaces this from HTTP.
    pub fn transient_throttle() -> Self {
        Self::HttpStatus(reqwest::StatusCode::TOO_MANY_REQUESTS)
    }

    /// Retryable at the watchlist. HTTP 400 on a new
    /// mint means "not indexed yet". Deserialization is terminal:
    /// a schema break is permanent, not a transient to re-poll every 3s.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::SecurityRejection(_)
            | Self::BadUrl(_)
            | Self::ResponseTooLarge
            | Self::Deserialization(_) => false,
            Self::Network(_) => true,
            Self::HttpStatus(s) => {
                s.is_server_error()
                    || *s == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || *s == reqwest::StatusCode::REQUEST_TIMEOUT
                    || *s == reqwest::StatusCode::BAD_REQUEST
            }
        }
    }
}

/// Risk report for one mint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RugCheckReport {
    /// Token mint address (base58).
    pub mint: String,
    /// Aggregate risk score. Higher is riskier.
    /// Required: missing score fails the fetch.
    pub score: u32,
    /// RugCheck's own rugged flag. Reject on true.
    #[serde(default)]
    pub rugged: bool,
    /// Individual risk findings.
    #[serde(default)]
    pub risks: Vec<RiskItem>,
    /// On-chain token metadata, when present.
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
    /// Severity (`warn` or `danger`). Unknown levels fail at policy.
    #[serde(default)]
    pub level: String,
}

/// On-chain token state relevant to rug risk.
/// Live API keys are camelCase (`mintAuthority`, ...).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    /// Mint authority address, if still enabled.
    #[serde(default)]
    pub mint_authority: Option<String>,
    /// Freeze authority address, if still enabled.
    #[serde(default)]
    pub freeze_authority: Option<String>,
    /// Total supply in base units (informational only: policy and
    /// sizing never read it; decimals come from RPC `token_decimals`).
    #[serde(default)]
    pub supply: u64,
    /// Mint decimals (informational only, see `supply`).
    #[serde(default)]
    pub decimals: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "mint": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
        "score": 120,
        "rugged": false,
        "risks": [
            {"name": "Low Liquidity", "value": "500", "description": "Low LP", "score": 120, "level": "warn"}
        ],
        "token": {"mintAuthority": null, "freezeAuthority": null, "supply": 1000000000, "decimals": 6, "isInitialized": true}
    }"#;

    #[test]
    fn deserializes_live_shaped_report() {
        let report: RugCheckReport = serde_json::from_str(SAMPLE).expect("valid JSON");
        assert_eq!(report.score, 120);
        assert!(!report.rugged);
        assert_eq!(report.risks.len(), 1);
        assert_eq!(report.risks[0].level, "warn");
        let token = report.token.expect("token present");
        assert!(token.mint_authority.is_none());
        assert_eq!(token.decimals, 6);
    }

    #[test]
    fn live_authority_keys_land_in_fields() {
        // Regression: snake_case fixtures silently parsed live
        // camelCase authorities as None (fail-open at policy).
        let report: RugCheckReport = serde_json::from_str(
            r#"{"mint": "M", "score": 0,
                "token": {"mintAuthority": "Auth111", "freezeAuthority": null,
                          "supply": 0, "decimals": 9, "isInitialized": true}}"#,
        )
        .expect("valid JSON");
        let token = report.token.expect("token present");
        assert_eq!(token.mint_authority.as_deref(), Some("Auth111"));
        assert!(token.freeze_authority.is_none());
    }

    #[test]
    fn rugged_flag_defaults_absent() {
        let flagged: RugCheckReport =
            serde_json::from_str(r#"{"mint": "M", "score": 0, "rugged": true}"#)
                .expect("valid JSON");
        assert!(flagged.rugged);
        let missing: RugCheckReport =
            serde_json::from_str(r#"{"mint": "M", "score": 0}"#).expect("valid JSON");
        assert!(!missing.rugged);
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
        let err =
            serde_json::from_str::<RugCheckReport>(r#"{"mint": "ABC"}"#).expect_err("must fail");
        assert!(err.to_string().contains("score"));
    }

    #[test]
    fn retryable_covers_indexing_latency_not_policy() {
        use reqwest::StatusCode;
        assert!(RugCheckError::HttpStatus(StatusCode::BAD_REQUEST).is_retryable());
        assert!(RugCheckError::HttpStatus(StatusCode::TOO_MANY_REQUESTS).is_retryable());
        assert!(RugCheckError::HttpStatus(StatusCode::REQUEST_TIMEOUT).is_retryable());
        assert!(RugCheckError::HttpStatus(StatusCode::INTERNAL_SERVER_ERROR).is_retryable());
        // Schema breaks are permanent: terminal, never re-polled.
        assert!(
            !RugCheckError::Deserialization(
                serde_json::from_str::<u32>("bogus").expect_err("must fail")
            )
            .is_retryable()
        );
        // Policy, config, and oversize bodies: drop at once.
        assert!(!RugCheckError::SecurityRejection("rug".to_string()).is_retryable());
        assert!(!RugCheckError::BadUrl(":::".to_string()).is_retryable());
        assert!(!RugCheckError::ResponseTooLarge.is_retryable());
        assert!(!RugCheckError::HttpStatus(StatusCode::FORBIDDEN).is_retryable());
    }
}
