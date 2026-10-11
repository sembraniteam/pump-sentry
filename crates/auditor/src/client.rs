//! Async RugCheck HTTP client with timeout + bounded retries.
//!
//! [`RugCheckClient::fetch_report`] performs the raw `GET
//! /tokens/{mint}/report` call; [`RugCheckClient::evaluate_token`]
//! additionally enforces [`AuditPolicy`] and maps violations to
//! [`RugCheckError::SecurityRejection`], matching the skill contract.
//!
//! Fail-closed boundaries: the mint is validated as a 32-byte base58
//! address before it touches the URL (no path/parameter injection), the
//! report's mint must match the request (no cache-poisoned mismatch),
//! and response bodies are capped so a rogue server cannot OOM the bot.

use crate::policy::{AuditPolicy, assess};
use crate::types::{RugCheckError, RugCheckReport};
use reqwest::{Client, StatusCode};
use std::time::Duration;
use tracing::{debug, warn};

/// Default API root.
pub const DEFAULT_BASE_URL: &str = "https://api.rugcheck.xyz/v1";
/// Default per-request timeout (matches the skill).
pub const DEFAULT_TIMEOUT_MS: u64 = 1500;
/// Default retry budget for transient failures.
pub const DEFAULT_MAX_RETRIES: u32 = 2;
/// Hard ceiling for the retry budget: an unbounded `max_retries` would
/// hang the hot launch path (backoff-capped 2s sleeps, forever).
pub const MAX_RETRIES_CAP: u32 = 5;
/// Response body cap: a rogue or compromised server must not OOM the
/// bot by streaming gigabytes into the JSON buffer.
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
    /// Override the API root (useful for tests / mirrors).
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the per-request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the retry budget for transient failures (clamped to
    /// [`MAX_RETRIES_CAP`] at build time).
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the acceptance policy used by `evaluate_token`.
    pub fn policy(mut self, policy: AuditPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Build the client (fails only when the TLS backend is unusable).
    pub fn build(self) -> Result<RugCheckClient, RugCheckError> {
        RugCheckClient::from_builder(self)
    }
}

/// RugCheck API client. Cheap to clone (wraps a pooled `reqwest::Client`).
#[derive(Debug, Clone)]
pub struct RugCheckClient {
    client: Client,
    base_url: String,
    max_retries: u32,
    policy: AuditPolicy,
}

impl RugCheckClient {
    /// Build with skill defaults: 1500ms timeout, 30s pool idle, live API.
    pub fn new() -> Result<Self, RugCheckError> {
        RugCheckClientBuilder::default().build()
    }

    /// Start from the default builder for customization.
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

    /// Report URL for a mint (factored out for testability). The mint
    /// must already be validated by [`valid_mint`] — base58 carries no
    /// `/`, `?`, or `#`, so interpolation cannot escape the path.
    fn report_url(&self, mint: &str) -> String {
        format!("{}/tokens/{}/report", self.base_url, mint)
    }

    /// Checked report URL: validates the mint before interpolation
    /// (see [`valid_mint`]); factored out for testability.
    fn report_url_checked(&self, mint: &str) -> Result<String, RugCheckError> {
        if !valid_mint(mint) {
            return Err(RugCheckError::SecurityRejection(format!(
                "invalid mint address '{mint}'"
            )));
        }
        Ok(self.report_url(mint))
    }

    /// Fetch the raw report without enforcing the security policy.
    pub async fn fetch_report(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        let url = self.report_url_checked(mint)?;
        let mut attempt: u32 = 0;

        loop {
            let response = match self.client.get(&url).send().await {
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
                let bytes = response.bytes().await?;
                if bytes.len() as u64 > MAX_BODY_BYTES {
                    return Err(oversized_body());
                }
                return serde_json::from_slice::<RugCheckReport>(&bytes)
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

    /// Fetch a report and enforce the acceptance policy.
    ///
    /// Returns the report when every invariant holds; otherwise returns
    /// [`RugCheckError::SecurityRejection`]. The report's mint must match
    /// the request — a mismatched body smells like a server or cache bug
    /// laundering one token's verdict onto another.
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

/// A mint is usable in the report path only as a 32-byte base58 address.
fn valid_mint(mint: &str) -> bool {
    bs58::decode(mint)
        .into_vec()
        .map(|v| v.len() == 32)
        .unwrap_or(false)
}

/// Capped-body rejection (see [`MAX_BODY_BYTES`]).
fn oversized_body() -> RugCheckError {
    RugCheckError::Deserialization(serde::de::Error::custom(format!(
        "response body exceeds {MAX_BODY_BYTES} bytes"
    )))
}

/// Network errors worth retrying: timeouts and connection failures.
/// Decode/body failures are deterministic (corrupt payload, not a bad
/// network) and return at once instead of burning the retry budget plus
/// backoff on the hot launch path. Non-transient builder/config errors
/// are not retried either.
fn is_transient_network_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Retry server-side failures, rate limits, and request timeouts;
/// other client errors (4xx) are terminal.
fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
}

/// Exponential backoff: 200ms × 2^attempt, capped at 2s. No extra RNG
/// dependency; jitter is unnecessary at this request rate.
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
            RugCheckClient::builder().base_url("ftp://example.com").build(),
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
        // Validation precedes any I/O: no server needed, never queried.
        let client = RugCheckClient::new().expect("client");
        for mint in ["", "MINT123", "not a mint!!", "MINT/123?x=1#frag"] {
            match client.report_url_checked(mint) {
                Err(RugCheckError::SecurityRejection(_)) => {}
                other => panic!("expected rejection for {mint:?}, got {other:?}"),
            }
        }
        assert!(client.report_url_checked(TEST_MINT).is_ok());
    }

    /// Minimal canned HTTP server: replays `responses` in order, then hangs
    /// up. Each response is a full status line + body pair.
    async fn serve_canned(responses: Vec<(u16, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                // Best-effort request read; ignore errors (client may retry).
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

    /// Real 32-byte base58 mint (accepts [`valid_mint`]); every network
    /// test must use it — short strings are now rejected pre-flight.
    const TEST_MINT: &str = "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU";

    const CLEAN_BODY: &str = r#"{
        "mint": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
        "score": 50,
        "risks": [],
        "token": {"mint_authority": null, "freeze_authority": null, "supply": 1000, "decimals": 9}
    }"#;

    const RISKY_BODY: &str = r#"{
        "mint": "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU",
        "score": 900,
        "risks": [{"name": "X", "value": "", "description": "bad", "score": 900, "level": "danger"}],
        "token": {"mint_authority": "A", "freeze_authority": null, "supply": 1000, "decimals": 9}
    }"#;

    const WRONG_MINT_BODY: &str = r#"{
        "mint": "DC4X9szDJk2wVm2NAGsshGFydKAahBg6sAAjmxvSJZGh",
        "score": 50,
        "risks": [],
        "token": {"mint_authority": null, "freeze_authority": null, "supply": 1000, "decimals": 9}
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
        // Right shape, wrong mint: one token's verdict must never
        // launder onto another (server/cache bug fail-closed).
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
        assert!(matches!(err, RugCheckError::Deserialization(_)));
    }
}
