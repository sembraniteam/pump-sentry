//! Paper pipeline binary: listen → audit → risk → simulate.
//!
//! Flow per Pump.fun launch event:
//! 1. [`auditor`] rejects rugs (fail-closed: any audit error skips).
//! 2. [`risk_engine`] prices TP/SL via LLM, else 300ms heuristic fallback.
//! 3. [`simulator`] opens a paper position at `QUOTE_USD` when set.
//!
//! TODO(live-feeds): `LIQUIDITY_USD` / `BUY_VELOCITY_5S` / `DEV_REPUTATION`
//! are env stubs until real on-chain feeds land; defaults are PASS-safe.
//! TODO(price-feed): without `QUOTE_USD` no position is opened; a polling
//! loop with TP/SL exits is the next step once quotes stream in.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use auditor::{RugCheckClient, RugCheckError};
use listener::{ListenerConfig, PumpFunListener};
use risk_engine::{Action, RiskEngine, TokenMetrics, evaluate_heuristic_fallback};
use simulator::{PaperPortfolio, Side};
use tracing::{info, warn};

/// Runtime knobs, all overridable via env.
struct Settings {
    ws_url: String,
    http_url: String,
    commitment: String,
    start_balance: f64,
    fee_bps: f64,
    notional_usd: f64,
    trail_percent: f64,
    liquidity_usd: f64,
    buy_velocity_5s: u32,
    dev_reputation: f64,
    quote_usd: Option<f64>,
    inference_endpoint: String,
    inference_model: String,
    inference_timeout_ms: u64,
}

impl Settings {
    fn from_env() -> Self {
        Self {
            ws_url: env_string("WS_URL", "wss://api.mainnet-beta.solana.com"),
            http_url: env_string("HTTP_URL", "https://api.mainnet-beta.solana.com"),
            commitment: env_string("COMMITMENT", "confirmed"),
            start_balance: env_float("START_BALANCE", 100.0),
            fee_bps: env_float("FEE_BPS", 30.0),
            notional_usd: env_float("NOTIONAL_USD", 10.0),
            trail_percent: env_float("TRAIL_PERCENT", 5.0),
            liquidity_usd: env_float("LIQUIDITY_USD", 0.0),
            buy_velocity_5s: env_int("BUY_VELOCITY_5S", 0),
            dev_reputation: env_float("DEV_REPUTATION", 0.0),
            quote_usd: std::env::var("QUOTE_USD").ok().and_then(|v| v.parse().ok()),
            inference_endpoint: env_string(
                "INFERENCE_ENDPOINT",
                "http://127.0.0.1:11434/v1/chat/completions",
            ),
            inference_model: env_string("INFERENCE_MODEL", "llama3.2:1b"),
            inference_timeout_ms: env_int("INFERENCE_TIMEOUT_MS", 300),
        }
    }
}

fn env_string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_float(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_int<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "info,solana_snipe_bot=debug",
        ))
        .init();

    let settings = Settings::from_env();
    if settings.dev_reputation == 0.0 {
        warn!("DEV_REPUTATION unset: heuristic defaults to PASS until the live feed lands (TODO)");
    }

    let (mut rx, stats, _handle) = match PumpFunListener::subscribe(ListenerConfig {
        ws_url: settings.ws_url.clone(),
        http_url: settings.http_url.clone(),
        commitment: settings.commitment.clone(),
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to subscribe: {e}");
            std::process::exit(1);
        }
    };

    let auditor = RugCheckClient::new();
    let engine = match RiskEngine::builder()
        .endpoint(&settings.inference_endpoint)
        .model(&settings.inference_model)
        .timeout(std::time::Duration::from_millis(
            settings.inference_timeout_ms,
        ))
        .build()
    {
        Ok(engine) => Some(engine),
        Err(e) => {
            warn!(error = %e, "bad inference endpoint; heuristic-only mode");
            None
        }
    };
    let mut portfolio = PaperPortfolio::new(settings.start_balance, settings.fee_bps);

    info!("pipeline live (Ctrl-C to stop)");
    while let Some(event) = rx.recv().await {
        let mint = event.mint.to_string();

        // 1. Audit — fail closed.
        let report = match auditor.evaluate_token(&mint).await {
            Ok(report) => report,
            Err(RugCheckError::SecurityRejection(reason)) => {
                info!(%mint, "audit reject: {reason}");
                continue;
            }
            Err(e) => {
                warn!(%mint, error = %e, "audit error; skipping (fail-closed)");
                continue;
            }
        };

        // 2. Risk — model with heuristic fallback.
        let metrics = TokenMetrics {
            liquidity_usd: settings.liquidity_usd,
            buy_velocity_5s: settings.buy_velocity_5s,
            dev_reputation: settings.dev_reputation,
            rugcheck_score: report.score,
        };
        let profile = match &engine {
            Some(engine) => engine.evaluate(&metrics).await.profile,
            None => evaluate_heuristic_fallback(
                metrics.liquidity_usd,
                metrics.buy_velocity_5s,
                metrics.dev_reputation,
                metrics.rugcheck_score,
            ),
        };
        if profile.action != Action::Buy {
            info!(%mint, score = report.score, "risk PASS");
            continue;
        }

        // 3. Simulate — needs an entry quote.
        let Some(quote) = settings.quote_usd else {
            info!(%mint, tp = profile.tp_percent, sl = profile.sl_percent,
                "BUY signal but QUOTE_USD unset; position not opened (TODO price-feed)");
            continue;
        };
        match portfolio.open_position(
            &mint,
            Side::Long,
            quote,
            settings.notional_usd,
            settings.liquidity_usd,
            profile.tp_percent,
            profile.sl_percent,
            profile.trailing_stop,
            settings.trail_percent,
            profile.timeout_seconds,
            now_secs(),
        ) {
            Ok(id) => {
                let snap = portfolio.snapshot(&HashMap::new());
                info!(%mint, id, tp = profile.tp_percent, sl = profile.sl_percent,
                    equity = format!("{:.2}", snap.equity),
                    "paper position opened");
            }
            Err(e) => warn!(%mint, error = %e, "paper open failed"),
        }

        let snap = stats.snapshot();
        info!(
            logs = snap.logs_seen,
            candidates = snap.candidates,
            parse_ok = snap.parse_ok,
            dropped = snap.dropped_full,
            "listener stats"
        );
    }

    // Channel closed — positions stay open; settle them once the price
    // feed (TODO) can mark them.
    warn!(
        "event stream ended; {} position(s) left open for review",
        portfolio.position_ids().len()
    );
}
