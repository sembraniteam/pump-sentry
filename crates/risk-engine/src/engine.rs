//! LLM-first risk engine with heuristic fallback.

use crate::fallback::{RUGCHECK_SCORE_CAP, evaluate_metrics_fallback};
use crate::schema::{RiskProfile, TokenMetrics, build_prompt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, warn};

/// Inference deadline before fallback (300ms).
pub const FALLBACK_TIMEOUT: Duration = Duration::from_millis(300);
/// Sampling temperature for near-deterministic output.
pub const DEFAULT_TEMPERATURE: f32 = 0.1;
/// Response token cap.
pub const DEFAULT_MAX_TOKENS: u32 = 128;
/// Max caller-configured timeout (clamped at build).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(2);
/// Min inference budget (clamped at build).
pub const MIN_TIMEOUT: Duration = Duration::from_millis(50);
/// Token budget bounds (clamped at build).
pub const MIN_MAX_TOKENS: u32 = 16;
pub const MAX_MAX_TOKENS: u32 = 512;

/// Engine failures unrelated to model trouble.
#[derive(Error, Debug)]
pub enum RiskEngineError {
    /// Endpoint URL is missing or invalid.
    #[error("invalid inference endpoint: {0}")]
    BadEndpoint(String),
    /// HTTP client cannot initialize (TLS backend unusable).
    #[error("failed to initialize HTTP client: {0}")]
    HttpClient(String),
}

/// Where a [`RiskDecision`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// Model inference within budget with a valid profile.
    Model,
    /// Heuristic: model unreachable or over budget.
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
#[derive(Clone)]
pub struct RiskEngineBuilder {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
    temperature: f32,
    max_tokens: u32,
}

impl std::fmt::Debug for RiskEngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiskEngineBuilder")
            .field("endpoint", &log_endpoint(&self.endpoint))
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
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

    /// Bearer token; skipped for local servers.
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Inference deadline (clamped at build).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sampling temperature, clamped to `0.0..=1.0`.
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    /// Build, validating the endpoint URL.
    pub fn build(self) -> Result<RiskEngine, RiskEngineError> {
        RiskEngine::from_builder(self)
    }
}

/// Risk engine. Cheap to clone.
/// Custom [`std::fmt::Debug`]: token never appears in logs.
#[derive(Clone)]
pub struct RiskEngine {
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
    temperature: f32,
    max_tokens: u32,
    /// Heuristic-fallback count.
    fallback_count: Arc<AtomicU64>,
    /// Total `evaluate` calls (denominator for the fallback rate;
    /// deliberate heuristic-only calls via `evaluate_fallback` are
    /// excluded — this measures model trouble, not mode).
    decision_count: Arc<AtomicU64>,
    /// Timeouts only (subset of fallbacks; operator can tell
    /// whether the model is slow vs non-compliant).
    timeout_count: Arc<AtomicU64>,
    /// Non-compliant model outputs (undecodable or invalid;
    /// fail-closed to PASS, not heuristic fallback).
    invalid_count: Arc<AtomicU64>,
}

impl std::fmt::Debug for RiskEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiskEngine")
            .field("endpoint", &log_endpoint(&self.endpoint))
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("fallback_count", &self.fallback_count())
            .finish()
    }
}

/// Endpoint reduced to `scheme://host` for logs. Providers hide keys
/// in the query (Helius) or the path (Chainstack, dRPC); query-only
/// stripping still leaks path keys. Mirror of `ids::redact_endpoint`
/// (canonical, unit-tested there); kept local so risk-engine stays
/// dependency-light.
fn log_endpoint(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => return "<unparseable-endpoint>".to_string(),
    };
    let scheme = &url[..url.len() - rest.len() - 3];
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if scheme.is_empty() || host.is_empty() {
        return "<unparseable-endpoint>".to_string();
    }
    format!("{scheme}://{host}")
}

impl RiskEngine {
    /// Engine with defaults (local endpoint, 300ms budget).
    pub fn new() -> Result<Self, RiskEngineError> {
        RiskEngineBuilder::default().build()
    }

    /// Start from defaults for customization.
    pub fn builder() -> RiskEngineBuilder {
        RiskEngineBuilder::default()
    }

    fn from_builder(b: RiskEngineBuilder) -> Result<Self, RiskEngineError> {
        let endpoint = b.endpoint.trim().to_string();
        match reqwest::Url::parse(&endpoint) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(RiskEngineError::BadEndpoint(b.endpoint)),
        }
        // Clamp budgets to protect the hot path.
        let timeout = b.timeout.clamp(MIN_TIMEOUT, MAX_TIMEOUT);
        let max_tokens = b.max_tokens.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS);
        let client = Client::builder()
            .timeout(timeout.saturating_add(Duration::from_millis(100)))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| RiskEngineError::HttpClient(e.to_string()))?;
        Ok(Self {
            client,
            endpoint,
            model: b.model,
            api_key: b.api_key,
            timeout,
            temperature: b.temperature.clamp(0.0, 1.0),
            max_tokens,
            fallback_count: Arc::new(AtomicU64::new(0)),
            decision_count: Arc::new(AtomicU64::new(0)),
            timeout_count: Arc::new(AtomicU64::new(0)),
            invalid_count: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Heuristic-fallback count since construction.
    pub fn fallback_count(&self) -> u64 {
        self.fallback_count.load(Ordering::Relaxed)
    }

    /// Total `evaluate` calls since construction.
    pub fn decision_count(&self) -> u64 {
        self.decision_count.load(Ordering::Relaxed)
    }

    /// Timeout fallbacks since construction (subset of [`Self::fallback_count`]).
    pub fn timeout_count(&self) -> u64 {
        self.timeout_count.load(Ordering::Relaxed)
    }

    /// Non-compliant model outputs since construction.
    pub fn invalid_count(&self) -> u64 {
        self.invalid_count.load(Ordering::Relaxed)
    }

    fn note_fallback(&self) {
        self.fallback_count.fetch_add(1, Ordering::Relaxed);
        self.invalid_count.fetch_add(1, Ordering::Relaxed);
    }

    fn note_noncompliant(&self) {
        self.invalid_count.fetch_add(1, Ordering::Relaxed);
    }

    fn note_timeout(&self) {
        self.fallback_count.fetch_add(1, Ordering::Relaxed);
        self.timeout_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Model inference within budget, else heuristic.
    /// Split is deliberate: unreachable/over-budget model falls back
    /// (entries must not halt when the model is down; single attempt,
    /// no retry on the entry path), but model output that is
    /// undecodable or invalid fails closed to PASS — an explicit
    /// valid BUY is required to buy, never a heuristic re-evaluation
    /// of garbled output.
    pub async fn evaluate(&self, metrics: &TokenMetrics) -> RiskDecision {
        self.decision_count.fetch_add(1, Ordering::Relaxed);
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
            Ok(Some(profile)) => {
                // In-crate score gate (not prompt advice): high-risk
                // mints never buy on any path, even on a model BUY.
                // The heuristic already PASSes above the cap itself.
                let profile = if profile.action == crate::schema::Action::Buy
                    && metrics.rugcheck_score > RUGCHECK_SCORE_CAP
                {
                    warn!(
                        endpoint = %log_endpoint(&self.endpoint), model = %self.model,
                        score = metrics.rugcheck_score,
                        "model BUY re-gated to PASS on audit score"
                    );
                    RiskProfile::pass()
                } else {
                    profile
                };
                RiskDecision {
                    profile,
                    source: DecisionSource::Model,
                }
            }
            Ok(None) => {
                self.note_fallback();
                // `infer` already warned the cause; stay at debug here.
                debug!(
                    endpoint = %log_endpoint(&self.endpoint), model = %self.model,
                    fallbacks = self.fallback_count(),
                    "inference unusable; using heuristic fallback"
                );
                RiskDecision {
                    profile: evaluate_metrics_fallback(metrics),
                    source: DecisionSource::Fallback,
                }
            }
            Err(_) => {
                self.note_timeout();
                warn!(
                    endpoint = %log_endpoint(&self.endpoint), model = %self.model,
                    timeout = ?self.timeout,
                    fallbacks = self.fallback_count(),
                    "inference timed out; using heuristic fallback"
                );
                RiskDecision {
                    profile: evaluate_metrics_fallback(metrics),
                    source: DecisionSource::Fallback,
                }
            }
        }
    }

    /// Heuristic-only evaluation (no I/O). Does not touch the
    /// model-trouble counters: this path is deliberate, not a failure.
    pub fn evaluate_fallback(&self, metrics: &TokenMetrics) -> RiskDecision {
        tracing::debug!(?metrics, "explicit heuristic fallback (no model call)");
        RiskDecision {
            profile: evaluate_metrics_fallback(metrics),
            source: DecisionSource::Fallback,
        }
    }

    /// One inference attempt. `None` means fall back.
    async fn infer(&self, request: ChatRequest) -> Option<RiskProfile> {
        let mut req = self.client.post(&self.endpoint).json(&request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = match req.send().await {
            Ok(response) => response,
            Err(e) => {
                warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, error = %e, "inference request failed; using heuristic fallback");
                return None;
            }
        };
        if !response.status().is_success() {
            warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, status = ?response.status(), "inference non-2xx status; using heuristic fallback");
            return None;
        }
        let body: ChatResponse = match response.json().await {
            Ok(body) => body,
            Err(e) => {
                warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, error = %e, "inference body decode failed; using heuristic fallback");
                return None;
            }
        };
        let content = body.choices.first()?.message.content.trim();
        // Tolerate fenced output from chatty models.
        let json = strip_code_fence(content);
        let profile: RiskProfile = match serde_json::from_str(json) {
            Ok(profile) => profile,
            Err(e) => {
                // Undecodable output fails closed to PASS: a garbled
                // BUY (truncation, wrong fence slice) must never
                // become a heuristic BUY.
                self.note_noncompliant();
                warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, error = %e, content = %truncate200(json), "inference output not in schema; holding PASS");
                return Some(RiskProfile::pass());
            }
        };
        if !profile.is_valid() {
            // Malformed PASS (e.g. non-zeroed timeout) is still a safe
            // intent: hold PASS instead of re-evaluating to a BUY.
            if profile.action == crate::schema::Action::Pass {
                self.note_noncompliant();
                warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, ?profile, "inference PASS malformed; holding PASS");
                return Some(RiskProfile::pass());
            }
            // Invalid BUY fails closed like undecodable output.
            self.note_noncompliant();
            warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, ?profile, "inference profile failed validation; holding PASS");
            return Some(RiskProfile::pass());
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

/// One chat message.
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

/// Minimal chat response shape (only what is consumed).
#[derive(Debug, Clone, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatChoice {
    message: ChatContent,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatContent {
    #[serde(default)]
    content: String,
}

/// Cap model output at 200 chars for logs.
fn truncate200(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Strip one layer of markdown fencing plus any chatty preamble:
/// finds the outer `{...}` object so `"Here is JSON:\n```json\n{...}\n```"`
/// still parses instead of falling back.
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    let inner = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let inner = inner.strip_suffix("```").unwrap_or(inner).trim();
    // Preamble-tolerant: slice to the outer JSON object when present.
    if let (Some(start), Some(end)) = (inner.find('{'), inner.rfind('}'))
        && end > start
    {
        return inner[start..=end].trim();
    }
    inner
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
            creation_slot_buys: None,
            cluster_pct: None,
            curve_progress: None,
        }
    }

    /// Canned HTTP server: replays `(delay_ms, status, body)` per connection.
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
    fn debug_redacts_api_key() {
        let engine = RiskEngine::builder()
            .api_key("super-secret")
            .build()
            .expect("engine");
        let dbg = format!("{engine:?}");
        assert!(!dbg.contains("super-secret"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        let builder = format!("{:?}", RiskEngine::builder().api_key("super-secret"));
        assert!(!builder.contains("super-secret"), "{builder}");
    }

    #[test]
    fn debug_and_logs_redact_endpoint_query() {
        // Provider keys live in `?` or the path: Debug and log lines
        // keep scheme://host and drop the rest.
        let engine = RiskEngine::builder()
            .endpoint("https://llm.example/v1?key=K9")
            .build()
            .expect("engine");
        let dbg = format!("{engine:?}");
        assert!(!dbg.contains("K9"), "{dbg}");
        assert!(dbg.contains("https://llm.example"), "{dbg}");
        assert_eq!(log_endpoint("https://x/y?a=b"), "https://x");
        assert_eq!(log_endpoint("https://x/y"), "https://x");
        assert_eq!(
            log_endpoint("wss://solana-mainnet.core.chainstack.com/eeb0secret"),
            "wss://solana-mainnet.core.chainstack.com"
        );
        // Transport keeps the raw URL.
        assert_eq!(engine.endpoint, "https://llm.example/v1?key=K9");
    }

    #[test]
    fn bad_endpoint_rejected() {
        for bad in ["   ", ":::not-a-url", "ftp://example.com", "https://"] {
            let err = RiskEngine::builder()
                .endpoint(bad)
                .build()
                .expect_err("must fail");
            assert!(matches!(err, RiskEngineError::BadEndpoint(_)), "{bad}");
        }
    }

    #[test]
    fn caller_budgets_clamped() {
        let engine = RiskEngine::builder()
            .timeout(Duration::from_secs(3600))
            .build()
            .expect("engine");
        assert_eq!(engine.timeout, MAX_TIMEOUT);
        let tiny = RiskEngine::builder()
            .timeout(Duration::ZERO)
            .build()
            .expect("engine");
        assert_eq!(tiny.timeout, MIN_TIMEOUT);
        let tokens = RiskEngineBuilder {
            max_tokens: u32::MAX,
            ..RiskEngineBuilder::default()
        }
        .build()
        .expect("engine");
        assert_eq!(tokens.max_tokens, MAX_MAX_TOKENS);
    }

    #[test]
    fn temperature_clamped() {
        let engine = RiskEngine::builder()
            .temperature(99.0)
            .build()
            .expect("engine");
        assert_eq!(engine.temperature, 1.0);
    }

    #[test]
    fn preamble_stripped_to_object() {
        let raw = "Here is JSON:\n```json\n{\"action\":\"PASS\",\"tp_percent\":0.0,\"sl_percent\":0.0,\"trailing_stop\":false,\"timeout_seconds\":0}\n```";
        let stripped = strip_code_fence(raw);
        let profile: RiskProfile = serde_json::from_str(stripped).expect("parses");
        assert!(profile.is_valid());
        assert_eq!(profile.action, Action::Pass);
    }

    #[test]
    fn pass_constructor_is_valid() {
        assert!(RiskProfile::pass().is_valid());
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
        assert_eq!(decision.profile.tp_percent, 60.0);
        assert_eq!(decision.profile.action, Action::Buy);
    }

    #[tokio::test]
    async fn undecodable_or_invalid_output_holds_pass() {
        // Garbled output must never become a heuristic BUY on these
        // hot metrics: only an explicit valid BUY buys.
        for bad in [
            r#"{"action":"BUY","tp_percent":"lots","sl_percent":-8.0}"#,
            r#"{"action":"BUY","tp_percent":-5.0,"sl_percent":3.0,"trailing_stop":false,"timeout_seconds":90}"#,
            r#"{"action":"BUY","tp_percent":90.0,"sl_percent":-8.0,"trailing_stop":true"#,
            "not json at all",
        ] {
            let leaked: &'static str = Box::leak(chat_body(bad).into_boxed_str());
            let base = serve_scripted(vec![(0, 200, leaked)]).await;
            let engine = fast_engine(&base);
            let decision = engine.evaluate(&metrics()).await;
            assert_eq!(decision.source, DecisionSource::Model, "input: {bad}");
            assert_eq!(decision.profile.action, Action::Pass, "input: {bad}");
            assert_eq!(engine.invalid_count(), 1);
        }
    }

    #[tokio::test]
    async fn malformed_pass_holds_pass_instead_of_fallback_buy() {
        // Malformed PASS (non-zeroed timeout): safe intent must hold,
        // never re-evaluate to a heuristic BUY on these hot metrics.
        let inner = r#"{"action":"PASS","tp_percent":0.0,"sl_percent":0.0,"trailing_stop":false,"timeout_seconds":90}"#;
        let leaked: &'static str = Box::leak(chat_body(inner).into_boxed_str());
        let base = serve_scripted(vec![(0, 200, leaked)]).await;
        let decision = fast_engine(&base).evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Model);
        assert_eq!(decision.profile.action, Action::Pass);
        assert_eq!(decision.profile.timeout_seconds, 0);
    }

    #[tokio::test]
    async fn model_buy_above_score_cap_regates_to_pass() {
        // In-crate score gate: a valid model BUY on a high-risk mint
        // holds PASS (source stays Model: the model was consulted).
        let inner = r#"{"action":"BUY","tp_percent":90.0,"sl_percent":-8.0,"trailing_stop":true,"timeout_seconds":180}"#;
        let leaked: &'static str = Box::leak(chat_body(inner).into_boxed_str());
        let base = serve_scripted(vec![(0, 200, leaked)]).await;
        let mut risky = metrics();
        risky.rugcheck_score = 380;
        let decision = fast_engine(&base).evaluate(&risky).await;
        assert_eq!(decision.source, DecisionSource::Model);
        assert_eq!(decision.profile.action, Action::Pass);
        assert_eq!(decision.profile.timeout_seconds, 0);
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

    #[tokio::test]
    async fn fallback_count_tracks_model_trouble() {
        let base = serve_scripted(vec![(0, 500, "{}")]).await;
        let engine = fast_engine(&base);
        assert_eq!(engine.fallback_count(), 0);
        let decision = engine.evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Fallback);
        assert_eq!(engine.fallback_count(), 1);
    }

    #[test]
    fn truncate200_caps_long_output() {
        let long = "x".repeat(500);
        assert_eq!(super::truncate200(&long).chars().count(), 201);
        assert_eq!(super::truncate200("short"), "short");
    }
}
