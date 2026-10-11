//! Paper pipeline binary: listen → audit → reserves → risk → simulate.
//!
//! Default mode is paper trading (no funds move). With `--live` (or
//! `IS_LIVE=true`) the same signals additionally dispatch real Jito-routed
//! swaps through `crates/executor` — read its README before enabling.
//!
//! Wiring only — all logic lives in [`settings`] (knobs), [`pipeline`]
//! (launch handling, tick exits), and [`live`] (live sizing/dispatch),
//! all unit-tested.

mod live;
mod pipeline;
mod settings;

use std::sync::Arc;
use std::time::Duration;

use alert::Alerter;
use auditor::RugCheckClient;
use curve::{CurveTicker, PriceTick};
use executor::{JitoClient, SwapExecutor, WalletGuard};
use listener::{DevScreenConfig, ListenerConfig, PumpFunListener, RpcScreener};
use risk_engine::RiskEngine;
use simulator::PaperPortfolio;
use tokio::sync::mpsc;
use tracing::{info, warn};

use live::LiveTrader;
use pipeline::Pipeline;
use settings::Settings;

#[tokio::main]
async fn main() {
    // Must run before any TLS use: the dependency tree enables both the
    // `ring` and `aws-lc-rs` rustls providers, so auto-detection panics.
    // `Err` only means another call already installed one — safe to ignore.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "info,solana_snipe_bot=debug",
        ))
        .init();

    let settings = Settings::from_env();

    // Mode gate: paper unless explicitly opted into live risk.
    let live_mode = std::env::args().any(|a| a == "--live")
        || std::env::var("IS_LIVE").is_ok_and(|v| v == "true");
    if live_mode {
        warn!("================================================================");
        warn!("LIVE MODE: real SOL will move via Jito bundles. Ctrl-C to abort.");
        warn!("================================================================");
    }

    let commitment = CurveTicker::parse_commitment(&settings.commitment);
    let ticker = match CurveTicker::new(
        &settings.http_url,
        commitment,
        Duration::from_millis(settings.tick_interval_ms),
        settings.sol_usd,
    ) {
        Ok(ticker) => ticker,
        Err(e) => {
            eprintln!("bad ticker config: {e}");
            std::process::exit(1);
        }
    };

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

    let mut engine_builder = RiskEngine::builder()
        .endpoint(&settings.inference_endpoint)
        .model(&settings.inference_model)
        .timeout(Duration::from_millis(settings.inference_timeout_ms));
    if let Some(key) = &settings.inference_api_key {
        engine_builder = engine_builder.api_key(key);
    }
    let engine = match engine_builder.build() {
        Ok(engine) => Some(engine),
        Err(e) => {
            warn!(error = %e, "bad inference endpoint; heuristic-only mode");
            None
        }
    };

    let start_balance = settings.start_balance;
    let fee_bps = settings.fee_bps;

    // Live stack (fatal on misconfiguration — never half-enable).
    let live = if live_mode {
        let wallet = match WalletGuard::load_from_env() {
            Ok(wallet) => wallet,
            Err(e) => {
                eprintln!("live mode requires SOLANA_PRIVATE_KEY: {e}");
                std::process::exit(1);
            }
        };
        info!(payer = %wallet.pubkey(), "live wallet loaded");
        let jito = match JitoClient::new(&settings.jito_endpoint) {
            Some(jito) => jito,
            None => {
                eprintln!("bad JITO_ENDPOINT: {}", settings.jito_endpoint);
                std::process::exit(1);
            }
        };
        let executor = match SwapExecutor::new(&settings.http_url, commitment, wallet) {
            Ok(executor) => executor.with_jito(jito),
            Err(e) => {
                eprintln!("bad executor config: {e}");
                std::process::exit(1);
            }
        };
        Some(LiveTrader::new(
            executor,
            settings.slippage_bps,
            settings.tip_lamports,
        ))
    } else {
        None
    };

    let screener = match RpcScreener::new(
        &settings.http_url,
        commitment,
        DevScreenConfig {
            min_age_secs: settings.dev_min_age_secs,
            max_prior_launches: settings.dev_max_prior_launches,
            sample_txs: settings.dev_screen_sample_txs,
            sig_limit: 100,
        },
    ) {
        Ok(screener) => screener,
        Err(e) => {
            eprintln!("bad screener config: {e}");
            std::process::exit(1);
        }
    };

    let gate_interval = Duration::from_millis(settings.gate_poll_ms);

    let alerts = settings.discord_alerts.then(Alerter::new);
    if alerts.is_none() {
        info!("discord alerts disabled (DISCORD_ALERTS=false)");
    }

    let pipeline = Pipeline::shared(
        settings,
        RugCheckClient::new(),
        engine,
        ticker.clone(),
        PaperPortfolio::new(start_balance, fee_bps),
        live,
        screener,
        alerts,
    );

    // Ticker loop: poll watched curves → auto-close on TP/SL.
    let (tick_tx, mut tick_rx) = mpsc::channel::<PriceTick>(1024);
    tokio::spawn(async move { ticker.run(tick_tx).await });

    // Gate loop: poll the watchlist for buyer quorum → confirmed entries.
    let gate_pipeline = Arc::clone(&pipeline);
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(gate_interval);
        loop {
            timer.tick().await;
            gate_pipeline.poll_gates().await;
        }
    });

    info!("pipeline live with auto-exits (Ctrl-C to stop)");
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                pipeline
                    .on_launch(
                        &event.mint.to_string(),
                        &event.bonding_curve.to_string(),
                        &event.associated_bonding_curve.to_string(),
                        &event.user.to_string(),
                        event.slot,
                    )
                    .await;
                let snap = stats.snapshot();
                let pending = pipeline.pending_count().await;
                info!(logs = snap.logs_seen, candidates = snap.candidates,
                    parse_ok = snap.parse_ok, dropped = snap.dropped_full,
                    watchlist = pending, "listener stats");
            },
            tick = tick_rx.recv() => {
                let Some(tick) = tick else { break };
                pipeline.on_tick(&tick).await;
            },
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                break;
            }
        }
    }

    warn!("streams ended; open positions need manual review");
}

#[cfg(test)]
mod tests {
    /// Guards the rustls dual-provider fix: without an explicitly
    /// installed default, `get_default` panics when both `ring` and
    /// `aws-lc-rs` features are enabled (see bot.log panic).
    #[test]
    fn crypto_provider_installs() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
