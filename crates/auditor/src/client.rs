//! Async RugCheck HTTP client with timeout + bounded retries.
//!
//! [`RugCheckClient::fetch_report`] performs the raw `GET
//! /tokens/{mint}/report` call; [`RugCheckClient::evaluate_token`]
//! additionally enforces [`AuditPolicy`] and maps violations to
//! [`RugCheckError::SecurityRejection`], matching the skill contract.

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

    /// Override the retry budget for transient failures.
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
        let client = Client::builder()
            .timeout(b.timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base_url: b.base_url,
            max_retries: b.max_retries,
            policy: b.policy,
        })
    }

    /// Report URL for a mint (factored out for testability).
    fn report_url(&self, mint: &str) -> String {
        format!("{}/tokens/{}/report", self.base_url, mint)
    }

    /// Fetch the raw report without enforcing the security policy.
    pub async fn fetch_report(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        let url = self.report_url(mint);
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
                let text = response.text().await?;
                return serde_json::from_str::<RugCheckReport>(&text)
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
    /// [`RugCheckError::SecurityRejection`].
    pub async fn evaluate_token(&self, mint: &str) -> Result<RugCheckReport, RugCheckError> {
        let report = self.fetch_report(mint).await?;
        assess(&report, &self.policy).into_result()?;
        Ok(report)
    }
}

/// Network errors worth retrying: timeouts and connection failures.
/// Decode/body failures are deterministic (corrupt payload, not a bad
/// network) and return at once instead of burning the retry budget plus
/// backoff on the hot launch path. Non-transient builder/config errors
/// are not retried either.
fn is_transient_network_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Retry server-side failures and rate limits; client errors (4xx other
/// than 429) are terminal.
fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
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
            client.report_url("MINT123"),
            "https://api.rugcheck.xyz/v1/tokens/MINT123/report"
        );
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

    const CLEAN_BODY: &str = r#"{
        "mint": "MINT123",
        "score": 50,
        "risks": [],
        "token": {"mint_authority": null, "freeze_authority": null, "supply": 1000, "decimals": 9}
    }"#;

    const RISKY_BODY: &str = r#"{
        "mint": "MINT123",
        "score": 900,
        "risks": [{"name": "X", "value": "", "description": "bad", "score": 900, "level": "danger"}],
        "token": {"mint_authority": "A", "freeze_authority": null, "supply": 1000, "decimals": 9}
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
            .fetch_report("MINT123")
            .await
            .expect("fetch");
        assert_eq!(report.score, 50);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let base = serve_canned(vec![(500, ""), (500, ""), (200, CLEAN_BODY)]).await;
        let report = test_client(&base)
            .fetch_report("MINT123")
            .await
            .expect("fetch");
        assert_eq!(report.mint, "MINT123");
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
        let err = client.fetch_report("MINT123").await.expect_err("must fail");
        assert!(matches!(err, RugCheckError::HttpStatus(_)));
        // 2 retries => ~200ms + ~400ms backoff floor.
        assert!(start.elapsed() >= Duration::from_millis(500));
    }

    #[tokio::test]
    async fn client_error_is_terminal() {
        let base = serve_canned(vec![(404, "not found")]).await;
        let err = test_client(&base)
            .fetch_report("NOPE")
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
            .evaluate_token("MINT123")
            .await
            .expect("allow");
        assert_eq!(report.score, 50);
    }

    #[tokio::test]
    async fn evaluate_rejects_risky_token() {
        let base = serve_canned(vec![(200, RISKY_BODY)]).await;
        let err = test_client(&base)
            .evaluate_token("MINT123")
            .await
            .expect_err("reject");
        match err {
            RugCheckError::SecurityRejection(msg) => assert!(msg.contains("900")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
