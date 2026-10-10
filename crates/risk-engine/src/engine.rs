//! Inference engine: LLM first, heuristic fallback on any failure.
//!
//! [`RiskEngine::evaluate`] wraps the model call in
//! `tokio::time::timeout(FALLBACK_TIMEOUT)` (300ms per the skill).
//! Timeout, transport error, non-2xx status, invalid JSON, or an
//! out-of-schema profile all resolve to [`evaluate_heuristic_fallback`],
//! so the hot path never blocks and never errors on model trouble.

use crate::fallback::evaluate_metrics_fallback;
use crate::schema::{RiskProfile, TokenMetrics, build_prompt};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, warn};

/// Skill-mandated inference deadline: fall back after 300ms.
pub const FALLBACK_TIMEOUT: Duration = Duration::from_millis(300);
/// Low temperature keeps model output near-deterministic.
pub const DEFAULT_TEMPERATURE: f32 = 0.1;
/// Small cap: the output schema fits in a few dozen tokens.
pub const DEFAULT_MAX_TOKENS: u32 = 128;

/// Engine failures that are *not* model trouble (i.e. caller bugs).
#[derive(Error, Debug)]
pub enum RiskEngineError {
    /// Endpoint URL is missing or invalid.
    #[error("invalid inference endpoint: {0}")]
    BadEndpoint(String),
}

/// Where a [`RiskDecision`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// LLM inference within budget with a valid profile.
    Model,
    /// Deterministic heuristic (timeout, error, or invalid output).
    Fallback,
}

/// Profile plus its provenance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RiskDecision {
    /// TP/SL profile to act on.
    pub profile: RiskProfile,
    /// Whether the model or the heuristic produced it.
    pub source: DecisionSource,
}

/// Builder for [`RiskEngine`].
#[derive(Debug, Clone)]
pub struct RiskEngineBuilder {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
    temperature: f32,
    max_tokens: u32,
}

impl Default for RiskEngineBuilder {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:11434/v1/chat/completions".to_string(),
            model: "llama3.2:1b".to_string(),
            api_key: None,
            timeout: FALLBACK_TIMEOUT,
            temperature: DEFAULT_TEMPERATURE,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

impl RiskEngineBuilder {
    /// OpenAI-compatible chat-completions endpoint.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Model identifier sent in the request body.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Bearer token; omitted for local (keyless) servers.
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Inference deadline before falling back (default 300ms).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sampling temperature, clamped to `0.0..=1.0` at build.
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    /// Build, validating the endpoint URL eagerly.
    pub fn build(self) -> Result<RiskEngine, RiskEngineError> {
        RiskEngine::from_builder(self)
    }
}

/// Quantitative risk engine. Cheap to clone (pooled HTTP client).
#[derive(Debug, Clone)]
pub struct RiskEngine {
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
    temperature: f32,
    max_tokens: u32,
}

impl RiskEngine {
    /// Engine with skill defaults (local endpoint, 300ms budget).
    pub fn new() -> Result<Self, RiskEngineError> {
        RiskEngineBuilder::default().build()
    }

    /// Start from defaults for customization.
    pub fn builder() -> RiskEngineBuilder {
        RiskEngineBuilder::default()
    }

    fn from_builder(b: RiskEngineBuilder) -> Result<Self, RiskEngineError> {
        if b.endpoint.trim().is_empty() {
            return Err(RiskEngineError::BadEndpoint(b.endpoint));
        }
        let client = Client::builder()
            .timeout(b.timeout.saturating_add(Duration::from_millis(100)))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to initialize HTTP client");
        Ok(Self {
            client,
            endpoint: b.endpoint,
            model: b.model,
            api_key: b.api_key,
            timeout: b.timeout,
            temperature: b.temperature.clamp(0.0, 1.0),
            max_tokens: b.max_tokens,
        })
    }

    /// Evaluate metrics: model inference within budget, else heuristic.
    ///
    /// Never fails on model trouble — the fallback always produces a
    /// valid profile. Only builder-level misconfiguration can error,
    /// and that is caught at [`RiskEngineBuilder::build`].
    pub async fn evaluate(&self, metrics: &TokenMetrics) -> RiskDecision {
        let (system, user) = build_prompt(metrics);
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            response_format: ResponseFormat {
                r#type: "json_object",
            },
        };

        match tokio::time::timeout(self.timeout, self.infer(request)).await {
            Ok(Some(profile)) => RiskDecision {
                profile,
                source: DecisionSource::Model,
            },
            Ok(None) => {
                debug!("inference unusable; using heuristic fallback");
                RiskDecision {
                    profile: evaluate_metrics_fallback(metrics),
                    source: DecisionSource::Fallback,
                }
            }
            Err(_) => {
                warn!(
                    "inference exceeded {:?}; using heuristic fallback",
                    self.timeout
                );
                RiskDecision {
                    profile: evaluate_metrics_fallback(metrics),
                    source: DecisionSource::Fallback,
                }
            }
        }
    }

    /// Synchronous heuristic-only evaluation (no I/O; for tests/tools).
    pub fn evaluate_fallback(&self, metrics: &TokenMetrics) -> RiskDecision {
        RiskDecision {
            profile: evaluate_metrics_fallback(metrics),
            source: DecisionSource::Fallback,
        }
    }

    /// One inference attempt. `None` = unusable (transport, status,
    /// shape) — the caller falls back.
    async fn infer(&self, request: ChatRequest) -> Option<RiskProfile> {
        let mut req = self.client.post(&self.endpoint).json(&request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = match req.send().await {
            Ok(response) => response,
            Err(e) => {
                debug!(error = %e, "inference request failed");
                return None;
            }
        };
        if response.status() != StatusCode::OK {
            debug!(status = ?response.status(), "inference non-200 status");
            return None;
        }
        let body: ChatResponse = match response.json().await {
            Ok(body) => body,
            Err(e) => {
                debug!(error = %e, "inference body decode failed");
                return None;
            }
        };
        let content = body.choices.first()?.message.content.trim();
        // Tolerate fenced output (` ```json ... ``` `) from chatty models.
        let json = strip_code_fence(content);
        let profile: RiskProfile = match serde_json::from_str(json) {
            Ok(profile) => profile,
            Err(e) => {
                debug!(error = %e, content, "inference output not in schema");
                return None;
            }
        };
        if !profile.is_valid() {
            debug!(?profile, "inference profile failed validation");
            return None;
        }
        Some(profile)
    }
}

/// OpenAI-compatible chat request body.
#[derive(Debug, Clone, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    response_format: ResponseFormat,
}

/// Single chat message.
#[derive(Debug, Clone, Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

/// JSON-mode hint.
#[derive(Debug, Clone, Copy, Serialize)]
struct ResponseFormat {
    r#type: &'static str,
}

/// Minimal chat response shape (only what we consume).
#[derive(Debug, Clone, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

/// Minimal chat response shape (only what we consume).
#[derive(Debug, Clone, Deserialize)]
struct ChatChoice {
    message: ChatContent,
}

/// Minimal chat response shape (only what we consume).
#[derive(Debug, Clone, Deserialize)]
struct ChatContent {
    #[serde(default)]
    content: String,
}

/// Remove one layer of markdown fencing, if present.
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    let inner = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    inner.strip_suffix("```").unwrap_or(inner).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Action;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn metrics() -> TokenMetrics {
        TokenMetrics {
            liquidity_usd: 30_000.0,
            buy_velocity_5s: 40,
            dev_reputation: 0.8,
            rugcheck_score: 100,
        }
    }

    /// Canned HTTP server: each connection gets the next `(delay_ms,
    /// status, body)` tuple, then the socket closes.
    async fn serve_scripted(script: Vec<(u64, u16, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for (delay_ms, status, body) in script {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                let reason = if status == 200 { "OK" } else { "Error" };
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

    fn chat_body(inner_json: &str) -> String {
        serde_json::json!({
            "choices": [{"message": {"content": inner_json}}]
        })
        .to_string()
    }

    fn fast_engine(base: &str) -> RiskEngine {
        RiskEngine::builder()
            .endpoint(base)
            .timeout(Duration::from_millis(300))
            .build()
            .expect("engine")
    }

    #[test]
    fn default_timeout_is_300ms() {
        assert_eq!(FALLBACK_TIMEOUT, Duration::from_millis(300));
        let engine = RiskEngineBuilder::default().build().expect("engine");
        assert_eq!(engine.timeout, FALLBACK_TIMEOUT);
    }

    #[test]
    fn bad_endpoint_rejected() {
        let err = RiskEngine::builder()
            .endpoint("   ")
            .build()
            .expect_err("must fail");
        assert!(matches!(err, RiskEngineError::BadEndpoint(_)));
    }

    #[test]
    fn temperature_clamped() {
        let engine = RiskEngine::builder()
            .temperature(99.0)
            .build()
            .expect("engine");
        assert_eq!(engine.temperature, 1.0);
    }

    #[tokio::test]
    async fn uses_model_when_fast_and_valid() {
        let inner = r#"{"action":"BUY","tp_percent":90.0,"sl_percent":-8.0,"trailing_stop":true,"timeout_seconds":180}"#;
        let leaked: &'static str = Box::leak(chat_body(inner).into_boxed_str());
        let base = serve_scripted(vec![(0, 200, leaked)]).await;
        let decision = fast_engine(&base).evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Model);
        assert_eq!(decision.profile.tp_percent, 90.0);
        assert!(decision.profile.trailing_stop);
    }

    #[tokio::test]
    async fn falls_back_on_slow_model() {
        let inner = r#"{"action":"BUY","tp_percent":90.0,"sl_percent":-8.0,"trailing_stop":true,"timeout_seconds":180}"#;
        let leaked: &'static str = Box::leak(chat_body(inner).into_boxed_str());
        let base = serve_scripted(vec![(1000, 200, leaked)]).await;
        let engine = RiskEngine::builder()
            .endpoint(&base)
            .timeout(Duration::from_millis(50))
            .build()
            .expect("engine");
        let decision = engine.evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Fallback);
        // Heuristic for these metrics: high momentum.
        assert_eq!(decision.profile.tp_percent, 75.0);
        assert_eq!(decision.profile.action, Action::Buy);
    }

    #[tokio::test]
    async fn falls_back_on_invalid_model_output() {
        for bad in [
            r#"{"action":"BUY","tp_percent":"lots","sl_percent":-8.0}"#,
            r#"{"action":"BUY","tp_percent":-5.0,"sl_percent":3.0,"trailing_stop":false,"timeout_seconds":90}"#,
            "not json at all",
        ] {
            let leaked: &'static str = Box::leak(chat_body(bad).into_boxed_str());
            let base = serve_scripted(vec![(0, 200, leaked)]).await;
            let decision = fast_engine(&base).evaluate(&metrics()).await;
            assert_eq!(decision.source, DecisionSource::Fallback, "input: {bad}");
        }
    }

    #[tokio::test]
    async fn falls_back_on_error_status() {
        let base = serve_scripted(vec![(0, 500, "{}")]).await;
        let decision = fast_engine(&base).evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Fallback);
    }

    #[tokio::test]
    async fn falls_back_when_server_unreachable() {
        let engine = RiskEngine::builder()
            .endpoint("http://127.0.0.1:1")
            .timeout(Duration::from_millis(100))
            .build()
            .expect("engine");
        let decision = engine.evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Fallback);
        assert!(decision.profile.is_valid());
    }

    #[tokio::test]
    async fn accepts_fenced_model_output() {
        let inner = "```json\n{\"action\":\"PASS\",\"tp_percent\":0.0,\"sl_percent\":0.0,\"trailing_stop\":false,\"timeout_seconds\":0}\n```";
        let leaked: &'static str = Box::leak(chat_body(inner).into_boxed_str());
        let base = serve_scripted(vec![(0, 200, leaked)]).await;
        let decision = fast_engine(&base).evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Model);
        assert_eq!(decision.profile.action, Action::Pass);
    }
}
