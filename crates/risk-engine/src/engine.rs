//! LLM-first risk engine with heuristic fallback.

use crate::fallback::{RUGCHECK_SCORE_CAP, evaluate_metrics_fallback};
use crate::schema::{RiskProfile, TokenMetrics, build_prompt};
use reqwest::Client;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use zeroize::Zeroizing;use std::sync::{
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
/// Response token cap. The budget is shared with the model's reasoning
/// tokens (reasoning models think before they answer), so a small cap
/// is spent entirely on reasoning: the content channel comes back
/// empty and JSON-mode endpoints may reject the completion outright.
/// The cap only truncates, it is never a generation target —
/// non-reasoning models still stop at EOS.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;
/// Max caller-configured timeout (clamped at build).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(2);
/// Min inference budget (clamped at build).
pub const MIN_TIMEOUT: Duration = Duration::from_millis(50);
/// Token budget bounds (clamped at build); reasoning can spend the
/// whole budget before the answer, so the ceiling leaves headroom
/// above the default.
pub const MIN_MAX_TOKENS: u32 = 16;
pub const MAX_MAX_TOKENS: u32 = 2048;

/// Engine failures unrelated to model trouble.
#[derive(Error, Debug)]
pub enum RiskEngineError {
    /// Endpoint URL is missing or invalid.
    #[error("invalid inference endpoint: {0}")]
    BadEndpoint(String),
    /// HTTP client cannot initialize (TLS backend unusable).
    #[error("failed to initialize HTTP client: {0}")]
    HttpClient(String),
    /// Extra header invalid or transport-owned (name only; values
    /// never enter logs — they may carry routing secrets).
    #[error(
        "invalid inference header '{0}': must be a valid 'Name: value' pair, not authorization/user-agent/content-type/content-length/host"
    )]
    BadHeader(String),
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
    /// Bearer token (zeroized on drop; never logged, only redacted).
    api_key: Option<Zeroizing<String>>,
    extra_headers: Vec<(String, String)>,
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
            .field(
                "extra_headers",
                &self
                    .extra_headers
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
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
            extra_headers: Vec::new(),
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
        self.api_key = Some(Zeroizing::new(api_key.into()));
        self
    }

    /// One extra header sent on every inference request (e.g.
    /// `x-opencode-session` for OpenCode Go routing/cache). Empty by
    /// default — providers that need none are unaffected. Refused at
    /// [`Self::build`]: `authorization` (use [`Self::api_key`]),
    /// `user-agent` (owned), and framing headers.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Bulk extra headers (config wiring). Same refusal rules as
    /// [`Self::header`]; duplicates resolve last-wins at [`Self::build`].
    pub fn headers<I, N, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: Into<String>,
    {
        for (name, value) in headers {
            self.extra_headers.push((name.into(), value.into()));
        }
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
    api_key: Option<Zeroizing<String>>,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
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
            .field(
                "extra_headers",
                &self
                    .extra_headers
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("timeout", &self.timeout)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("fallback_count", &self.fallback_count())
            .finish()
    }
}

/// Headers the transport owns: auth rides `api_key`, the user agent
/// is owned (providers like OpenCode Go route on it), and framing
/// headers belong to reqwest/serde_json. Single source of truth for
/// both the builder and the binary's config parse (which reuses this
/// via `risk_engine::is_owned_inference_header`); matching is
/// case-insensitive so every casing is covered.
pub fn is_owned_inference_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "user-agent"
            | "content-type"
            | "content-length"
            | "host"
    )
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
        // Extra headers: validated once here so the hot path never
        // re-parses; duplicates resolve last-wins (config may repeat).
        let mut extra_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        for (name, value) in &b.extra_headers {
            let header_name = HeaderName::from_str(name.trim())
                .map_err(|_| RiskEngineError::BadHeader(name.trim().to_string()))?;
            if is_owned_inference_header(header_name.as_str()) {
                return Err(RiskEngineError::BadHeader(header_name.as_str().to_string()));
            }
            // Empty values are typos (`"x-sess:"` with a forgotten id),
            // not headers: refuse instead of sending a bare name.
            if value.trim().is_empty() {
                return Err(RiskEngineError::BadHeader(header_name.as_str().to_string()));
            }
            let header_value = HeaderValue::from_str(value.trim())
                .map_err(|_| RiskEngineError::BadHeader(header_name.as_str().to_string()))?;
            if let Some(pos) = extra_headers
                .iter()
                .position(|(n, _): &(HeaderName, HeaderValue)| *n == header_name)
            {
                extra_headers.remove(pos);
            }
            extra_headers.push((header_name, header_value));
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
            extra_headers,
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

    /// Transport/model-trouble fallback (unreachable, over budget,
    /// non-2xx, undecodable body): counts the fallback only. Invalid
    /// model OUTPUT is separate (`note_noncompliant`, fail-closed to
    /// PASS) and must never inflate this rate.
    fn note_fallback(&self) {
        self.fallback_count.fetch_add(1, Ordering::Relaxed);
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
        // Own user agent (never the generic SDK default): providers
        // that front multiple models (e.g. OpenCode Go) require callers
        // to identify themselves for routing/abuse monitoring.
        let mut req = self
            .client
            .post(&self.endpoint)
            .header(
                reqwest::header::USER_AGENT,
                format!("pump-sentry/{}", env!("CARGO_PKG_VERSION")),
            )
            .json(&request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key.as_str());
        }
        for (name, value) in &self.extra_headers {
            req = req.header(name.clone(), value.clone());
        }
        let response = match req.send().await {
            Ok(response) => response,
            Err(e) => {
                warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, error = %e, "inference request failed; using heuristic fallback");
                return None;
            }
        };
        if !response.status().is_success() {
            // Non-2xx bodies carry the reason (unknown model, bad param,
            // missing header): log it truncated. Error payloads echo the
            // failure, never our secrets (bearer travels in the header,
            // metrics are market data). A JSON-mode rejection with an
            // empty failed generation means the output budget ran out
            // before the answer channel produced anything: raise the
            // cap, not the prompt.
            let status = response.status();
            let detail = response
                .text()
                .await
                .map(|b| truncate500(&b))
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            warn!(endpoint = %log_endpoint(&self.endpoint), model = %self.model, status = ?status, detail = %detail, "inference non-2xx status; using heuristic fallback");
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

/// Minimal chat response shape (only what is consumed). `Serialize` is
/// for the test harness: canned HTTP bodies are built from this DTO, so
/// a canned body can never carry a shape the decoder rejects.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ChatChoice {
    message: ChatContent,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ChatContent {
    #[serde(default)]
    content: String,
}

/// Cap model output at 200 chars for logs.
fn truncate200(s: &str) -> String {
    truncate_chars(s, 200)
}

/// Cap error bodies at 500 chars for logs. Shared core so the two
/// caps cannot drift apart.
fn truncate500(s: &str) -> String {
    truncate_chars(s, 500)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
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
            venue_fee_bps: None,
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

    /// Canned chat-completion wire body carrying `inner_json` as the
    /// message content (built through the decoder's own DTO).
    fn chat_body(inner_json: &str) -> String {
        serde_json::to_string(&ChatResponse {
            choices: vec![ChatChoice {
                message: ChatContent {
                    content: inner_json.to_string(),
                },
            }],
        })
        .expect("chat body serializes")
    }

    fn fast_engine(base: &str) -> RiskEngine {
        RiskEngine::builder()
            .endpoint(base)
            .timeout(Duration::from_millis(300))
            .build()
            .expect("engine")
    }

    /// Canned server that captures the raw request head (headers) for
    /// one connection, then replays `(status, body)`.
    async fn serve_capturing(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let reason = if status == 200 { "OK" } else { "Error" };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn default_timeout_is_300ms() {
        assert_eq!(FALLBACK_TIMEOUT, Duration::from_millis(300));
        let engine = RiskEngineBuilder::default().build().expect("engine");
        assert_eq!(engine.timeout, FALLBACK_TIMEOUT);
    }

    #[test]
    fn default_token_budget_fits_reasoning_models() {
        // A 128-token cap truncated reasoning models before the answer
        // channel opened: the completion came back empty and JSON-mode
        // endpoints rejected it. The default budget must fit reasoning
        // + answer, and the ceiling must stay above it.
        let engine = RiskEngineBuilder::default().build().expect("engine");
        assert_eq!(engine.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(engine.max_tokens >= 512, "{}", engine.max_tokens);
        assert!(engine.max_tokens > MIN_MAX_TOKENS);
        assert!(engine.max_tokens < MAX_MAX_TOKENS);
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

    #[test]
    fn owned_headers_refused_case_insensitively() {
        // Single source both the builder and the config parse share:
        // every casing of each owned header refuses.
        for name in [
            "authorization",
            "Authorization",
            "AUTHORIZATION",
            "proxy-authorization",
            "Proxy-Authorization",
            "user-agent",
            "User-Agent",
            "content-type",
            "Content-Length",
            "HOST",
        ] {
            assert!(is_owned_inference_header(name), "{name}");
        }
        for name in ["x-opencode-session", "x-custom-flag", "accept"] {
            assert!(!is_owned_inference_header(name), "{name}");
        }
    }

    #[test]
    fn bad_extra_header_refused_at_build() {
        // Transport-owned headers break routing/auth/framing; garbage
        // names/values must refuse loudly, never ride silently.
        for (name, value) in [
            ("authorization", "s3cr3t"),
            ("Authorization", "s3cr3t"),
            ("user-agent", "spoof"),
            ("content-type", "text/plain"),
            ("content-length", "12"),
            ("host", "spoof.example"),
            ("", "v"),
            ("bad name!", "v"),
            ("x-ok", "bad\nvalue"),
            ("x-ok", ""),
        ] {
            let err = RiskEngine::builder()
                .header(name, value)
                .build()
                .expect_err("must fail");
            assert!(matches!(err, RiskEngineError::BadHeader(_)), "{name}");
        }
        // The refusal names the header, never its value.
        let err = RiskEngine::builder()
            .header("authorization", "s3cr3t")
            .build()
            .expect_err("must fail");
        assert!(!format!("{err}").contains("s3cr3t"), "{err}");
    }

    #[test]
    fn debug_redacts_header_values() {
        let engine = RiskEngine::builder()
            .header("x-opencode-session", "sess-secret")
            .build()
            .expect("engine");
        let dbg = format!("{engine:?}");
        assert!(dbg.contains("x-opencode-session"), "{dbg}");
        assert!(!dbg.contains("sess-secret"), "{dbg}");
        let builder = format!(
            "{:?}",
            RiskEngine::builder().header("x-opencode-session", "sess-secret")
        );
        assert!(builder.contains("x-opencode-session"), "{builder}");
        assert!(!builder.contains("sess-secret"), "{builder}");
    }

    #[tokio::test]
    async fn extra_headers_ride_alongside_owned_user_agent() {
        // OpenCode Go shape: routing header required, owned user agent
        // intact (the provider requires callers to identify themselves).
        let inner = r#"{"action":"PASS","tp_percent":0.0,"sl_percent":0.0,"trailing_stop":false,"timeout_seconds":0}"#;
        let leaked: &'static str = Box::leak(chat_body(inner).into_boxed_str());
        let (base, captured) = serve_capturing(200, leaked).await;
        let engine = RiskEngine::builder()
            .endpoint(&base)
            .header("X-OpenCode-Session", "sess-123")
            .timeout(Duration::from_millis(300))
            .build()
            .expect("engine");
        let decision = engine.evaluate(&metrics()).await;
        assert_eq!(decision.source, DecisionSource::Model);
        let raw = captured.await.expect("request captured").to_lowercase();
        assert!(
            raw.contains("x-opencode-session: sess-123"),
            "routing header missing: {raw}"
        );
        assert!(
            raw.contains("user-agent: pump-sentry/"),
            "owned user agent missing: {raw}"
        );
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
            // Reasoning model that burned the whole budget before the
            // answer channel (empty content; JSON-mode endpoints may
            // reject this upstream as a 400 instead).
            "",
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
        // Transport trouble is not model non-compliance: the invalid
        // counter stays put (only `note_noncompliant` moves it).
        assert_eq!(engine.invalid_count(), 0);
        assert_eq!(engine.timeout_count(), 0);
    }

    #[test]
    fn truncate200_caps_long_output() {
        let long = "x".repeat(500);
        assert_eq!(super::truncate200(&long).chars().count(), 201);
        assert_eq!(super::truncate200("short"), "short");
    }
}
