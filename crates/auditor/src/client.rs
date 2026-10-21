//! RugCheck HTTP client with timeout and retries.
//!
//! Fail-closed: mint checked before URL build.
//! Report mint must match the request.

use crate::policy::{AuditPolicy, assess};
use crate::types::{RugCheckError, RugCheckReport};
use reqwest::{Client, StatusCode};
use std::time::Duration;
use tracing::{debug, warn};

/// Default API root.
pub const DEFAULT_BASE_URL: &str = "https://api.rugcheck.xyz/v1";
/// Default per-request timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 1500;
/// Default retry budget.
pub const DEFAULT_MAX_RETRIES: u32 = 2;
/// Retry ceiling (clamped at build).
pub const MAX_RETRIES_CAP: u32 = 5;
/// Response body cap.
pub const MAX_BODY_BYTES: u64 = 1_000_000;

/// Builder for [`RugCheckClient`].
#[derive(Debug, Clone)]
pub struct RugCheckClientBuilder {
    base_url: String,
    timeout: Duration,
    max_retries: u32,
    policy: AuditPolicy,
}

impl Default for RugCheckClientBuilder {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_retries: DEFAULT_MAX_RETRIES,
            policy: AuditPolicy::default(),
        }
    }
}

impl RugCheckClientBuilder {
    /// Override the API root.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the per-request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the retry budget.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the policy used by `evaluate_token`.
    /// Weakening the defaults bypasses fail-closed checks: debug builds
    /// assert every check stays on, so non-default policies are tests-only.
    pub fn policy(mut self, policy: AuditPolicy) -> Self {
        debug_assert!(
            policy.max_score <= AuditPolicy::default().max_score
                && policy.reject_danger
                && policy.reject_mint_authority
                && policy.reject_freeze_authority,
            "non-default audit policy disables fail-closed checks; tests only"
        );
        self.policy = policy;
        self
    }

    /// Build the client.
    pub fn build(self) -> Result<RugCheckClient, RugCheckError> {
        RugCheckClient::from_builder(self)
    }
}

/// RugCheck API client. Cheap to clone.
#[derive(Debug, Clone)]
pub struct RugCheckClient {
    client: Client,
    base_url: String,
    max_retries: u32,
    policy: AuditPolicy,
}

impl RugCheckClient {
    /// Build with defaults.
    pub fn new() -> Result<Self, RugCheckError> {
        RugCheckClientBuilder::default().build()
    }

    /// Start from defaults.
    pub fn builder() -> RugCheckClientBuilder {
        RugCheckClientBuilder::default()
    }

    fn from_builder(b: RugCheckClientBuilder) -> Result<Self, RugCheckError> {
        let base_url = b.base_url.trim_end_matches('/').to_string();
        match reqwest::Url::parse(&base_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(RugCheckError::BadUrl(base_url)),
        }
        let client = Client::builder()
            .timeout(b.timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base_url,
            max_retries: b.max_retries.min(MAX_RETRIES_CAP),
            policy: b.policy,
        })
    }

    /// Report URL for a mint. Mint must pass [`valid_mint`].
    fn report_url(&self, mint: &str) -> String {
        format!("{}/tokens/{}/report", self.base_url, mint)
    }

    /// Checked report URL: validates the mint before interpolation.
    fn report_url_checked(&self, mint: &str) -> Result<String, RugCheckError> {
        if !valid_mint(mint) {
            return Err(RugCheckError::SecurityRejection(format!(
                "invalid mint address '{mint}'"
            )));
        }
        Ok(self.report_url(mint))
    }

    /// Fetch the raw report. Private: callers must go through
    /// [`evaluate_token`](Self::evaluate_token) so the policy always runs.
    async fn fetch_report(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        let url = self.report_url_checked(mint)?;
        let mut attempt: u32 = 0;

        'retry: loop {
            let mut response = match self.client.get(&url).send().await {
                Ok(response) => response,
                Err(e) if is_transient_network_error(&e) && attempt < self.max_retries => {
                    warn!(mint, attempt, error = %e, "rugcheck request failed; retrying");
                    sleep_backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(RugCheckError::Network(e)),
            };

            let status = response.status();
            if status.is_success() {
                if response
                    .content_length()
                    .is_some_and(|len| len > MAX_BODY_BYTES)
                {
                    return Err(oversized_body());
                }
                // Incremental cap: reject mid-stream past MAX_BODY_BYTES.
                // `bytes()` would buffer an unbounded body before the length
                // check below, so a lying/missing Content-Length could OOM.
                let mut buf = Vec::new();
                loop {
                    match response.chunk().await {
                        Ok(Some(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            if buf.len() as u64 > MAX_BODY_BYTES {
                                return Err(oversized_body());
                            }
                        }
                        Ok(None) => break,
                        Err(e) if is_transient_network_error(&e) && attempt < self.max_retries => {
                            warn!(mint, attempt, error = %e, "rugcheck body stalled; retrying");
                            sleep_backoff(attempt).await;
                            attempt += 1;
                            continue 'retry;
                        }
                        Err(e) => return Err(RugCheckError::Network(e)),
                    }
                }
                return serde_json::from_slice::<RugCheckReport>(&buf)
                    .map_err(RugCheckError::Deserialization);
            }

            if is_retryable_status(status) && attempt < self.max_retries {
                warn!(mint, ?status, attempt, "rugcheck error status; retrying");
                sleep_backoff(attempt).await;
                attempt += 1;
                continue;
            }

            debug!(mint, ?status, "rugcheck non-success status");
            return Err(RugCheckError::HttpStatus(status));
        }
    }

    /// Fetch a report and enforce the policy.
    /// Report mint must match the request.
    pub async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        let report = self.fetch_report(mint).await?;
        if report.mint != mint {
            return Err(RugCheckError::SecurityRejection(format!(
                "report mint mismatch: asked '{mint}', got '{}'",
                report.mint
            )));
        }
        assess(&report, &self.policy).into_result()?;
        Ok(report)
    }
}

/// A mint is usable only as a 32-byte base58 address.
fn valid_mint(mint: &str) -> bool {
    bs58::decode(mint)
        .into_vec()
        .map(|v| v.len() == 32)
        .unwrap_or(false)
}

/// Capped-body rejection.
fn oversized_body() -> RugCheckError {
    RugCheckError::ResponseTooLarge
}

/// Network errors worth retrying.
/// Body stalls mid-stream retry like timeouts; decode failures return at once.
fn is_transient_network_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_body()
}

/// Retry server errors, rate limits, and timeouts.
/// Other 4xx are terminal here.
fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
}

/// Backoff: 200ms x 2^attempt, capped at 2s.
async fn sleep_backoff(attempt: u32) {
    let shift = attempt.min(4);
    let ms = 200u64.saturating_mul(1u64 << shift).min(2000);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn report_url_format() {
        let client = RugCheckClient::new().expect("client");
        assert_eq!(
            client.report_url(TEST_MINT),
            format!("https://api.rugcheck.xyz/v1/tokens/{TEST_MINT}/report")
        );
    }

    #[test]
    fn rejects_bad_base_url() {
        assert!(matches!(
            RugCheckClient::builder().base_url("notaurl").build(),
            Err(RugCheckError::BadUrl(_))
        ));
        assert!(matches!(
            RugCheckClient::builder()
                .base_url("ftp://example.com")
                .build(),
            Err(RugCheckError::BadUrl(_))
        ));
        assert!(matches!(
            RugCheckClient::builder().base_url("https://").build(),
            Err(RugCheckError::BadUrl(_))
        ));
    }

    #[test]
    fn max_retries_clamped() {
        let client = RugCheckClient::builder()
            .max_retries(u32::MAX)
            .build()
            .expect("build");
        assert_eq!(client.max_retries, MAX_RETRIES_CAP);
    }

    #[test]
    fn rejects_bad_mint_without_network() {
        // Validation precedes any I/O: no server is queried.
        let client = RugCheckClient::new().expect("client");
        for mint in ["", "MINT123", "not a mint!!", "MINT/123?x=1#frag"] {
            match client.report_url_checked(mint) {
                Err(RugCheckError::SecurityRejection(_)) => {}
                other => panic!("expected rejection for {mint:?}, got {other:?}"),
            }
        }
        assert!(client.report_url_checked(TEST_MINT).is_ok());
    }

    /// Minimal canned HTTP server: replays `responses` in order, then hangs up.
    async fn serve_canned(responses: Vec<(u16, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let reason = match status {
                    200 => "OK",
                    408 => "Request Timeout",
                    429 => "Too Many Requests",
                    _ => "Internal Server Error",
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

    /// Real 32-byte base58 mint for network tests (short strings are
    /// rejected pre-flight).
    const TEST_MINT: &str = "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU";

    const CLEAN_BODY: &str = r#"{
        "mint": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
        "score": 50,
        "rugged": false,
        "risks": [],
        "token": {"mintAuthority": null, "freezeAuthority": null, "supply": 1000, "decimals": 9}
    }"#;

    const RISKY_BODY: &str = r#"{
        "mint": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
        "score": 900,
        "rugged": false,
        "risks": [{"name": "X", "value": "", "description": "bad", "score": 900, "level": "danger"}],
        "token": {"mintAuthority": "A", "freezeAuthority": null, "supply": 1000, "decimals": 9}
    }"#;

    const WRONG_MINT_BODY: &str = r#"{
        "mint": "DC4X9szDJk2wVm2NAGsshGFydKAahBg6sAAjmxvSJZGh",
        "score": 50,
        "rugged": false,
        "risks": [],
        "token": {"mintAuthority": null, "freezeAuthority": null, "supply": 1000, "decimals": 9}
    }"#;

    fn test_client(base_url: &str) -> RugCheckClient {
        RugCheckClient::builder()
            .base_url(base_url)
            .timeout(Duration::from_secs(5))
            .max_retries(3)
            .build()
            .expect("test client")
    }

    #[tokio::test]
    async fn fetch_report_ok() {
        let base = serve_canned(vec![(200, CLEAN_BODY)]).await;
        let report = test_client(&base)
            .fetch_report(TEST_MINT)
            .await
            .expect("fetch");
        assert_eq!(report.score, 50);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let base = serve_canned(vec![(500, ""), (500, ""), (200, CLEAN_BODY)]).await;
        let report = test_client(&base)
            .fetch_report(TEST_MINT)
            .await
            .expect("fetch");
        assert_eq!(report.mint, TEST_MINT);
    }

    #[tokio::test]
    async fn gives_up_after_retry_budget() {
        let base = serve_canned(vec![(500, ""), (500, ""), (500, ""), (500, "")]).await;
        let client = RugCheckClient::builder()
            .base_url(&base)
            .timeout(Duration::from_secs(5))
            .max_retries(2)
            .build()
            .expect("test client");
        let start = Instant::now();
        let err = client.fetch_report(TEST_MINT).await.expect_err("must fail");
        assert!(matches!(err, RugCheckError::HttpStatus(_)));
        // 2 retries => ~200ms + ~400ms backoff floor.
        assert!(start.elapsed() >= Duration::from_millis(500));
    }

    #[tokio::test]
    async fn client_error_is_terminal() {
        let base = serve_canned(vec![(404, "not found")]).await;
        let err = test_client(&base)
            .fetch_report(TEST_MINT)
            .await
            .expect_err("must fail");
        match err {
            RugCheckError::HttpStatus(s) => assert_eq!(s, StatusCode::NOT_FOUND),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_request_is_terminal_at_client_layer() {
        // HTTP 400 is owned by the pipeline watchlist retry; a retry here
        // would consume the scripted 200 and wrongly succeed.
        let base = serve_canned(vec![(400, "not indexed"), (200, CLEAN_BODY)]).await;
        let err = test_client(&base)
            .fetch_report(TEST_MINT)
            .await
            .expect_err("must fail");
        match err {
            RugCheckError::HttpStatus(s) => assert_eq!(s, StatusCode::BAD_REQUEST),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn evaluate_accepts_clean_token() {
        let base = serve_canned(vec![(200, CLEAN_BODY)]).await;
        let report = test_client(&base)
            .evaluate_token(TEST_MINT)
            .await
            .expect("allow");
        assert_eq!(report.score, 50);
    }

    #[tokio::test]
    async fn evaluate_rejects_risky_token() {
        let base = serve_canned(vec![(200, RISKY_BODY)]).await;
        let err = test_client(&base)
            .evaluate_token(TEST_MINT)
            .await
            .expect_err("reject");
        match err {
            RugCheckError::SecurityRejection(msg) => assert!(msg.contains("900")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_timeout_is_retried() {
        let base = serve_canned(vec![(408, ""), (200, CLEAN_BODY)]).await;
        let report = test_client(&base)
            .fetch_report(TEST_MINT)
            .await
            .expect("fetch");
        assert_eq!(report.mint, TEST_MINT);
    }

    #[tokio::test]
    async fn evaluate_rejects_mint_mismatch() {
        let base = serve_canned(vec![(200, WRONG_MINT_BODY)]).await;
        let err = test_client(&base)
            .evaluate_token(TEST_MINT)
            .await
            .expect_err("reject");
        match err {
            RugCheckError::SecurityRejection(msg) => assert!(msg.contains("mismatch")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let big = " ".repeat(1_100_000);
        let body: &'static str = Box::leak(big.into_boxed_str());
        let base = serve_canned(vec![(200, body)]).await;
        let err = test_client(&base)
            .fetch_report(TEST_MINT)
            .await
            .expect_err("must fail");
        assert!(matches!(err, RugCheckError::ResponseTooLarge));
        assert!(!err.is_retryable());
    }
}
