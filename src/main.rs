//! Paper pipeline binary: listen → audit → reserves → risk → simulate.
//!
//! Wiring only — all logic lives in [`settings`] (knobs) and [`pipeline`]
//! (launch handling, tick exits), both unit-tested.

mod pipeline;
mod settings;

use std::time::Duration;

use auditor::RugCheckClient;
use curve::{CurveTicker, PriceTick};
use listener::{ListenerConfig, PumpFunListener};
use risk_engine::RiskEngine;
use simulator::PaperPortfolio;
use tokio::sync::mpsc;
use tracing::{info, warn};

use pipeline::Pipeline;
use settings::Settings;

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

    let ticker = match CurveTicker::new(
        &settings.http_url,
        CurveTicker::parse_commitment(&settings.commitment),
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

    let engine = match RiskEngine::builder()
        .endpoint(&settings.inference_endpoint)
        .model(&settings.inference_model)
        .timeout(Duration::from_millis(settings.inference_timeout_ms))
        .build()
    {
        Ok(engine) => Some(engine),
        Err(e) => {
            warn!(error = %e, "bad inference endpoint; heuristic-only mode");
            None
        }
    };

    let start_balance = settings.start_balance;
    let fee_bps = settings.fee_bps;
    let pipeline = Pipeline::shared(
        settings,
        RugCheckClient::new(),
        engine,
        ticker.clone(),
        PaperPortfolio::new(start_balance, fee_bps),
    );

    // Ticker loop: poll watched curves → auto-close on TP/SL.
    let (tick_tx, mut tick_rx) = mpsc::channel::<PriceTick>(1024);
    tokio::spawn(async move { ticker.run(tick_tx).await });

    info!("pipeline live with auto-exits (Ctrl-C to stop)");
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                pipeline
                    .on_launch(&event.mint.to_string(), &event.bonding_curve.to_string())
                    .await;
                let snap = stats.snapshot();
                info!(logs = snap.logs_seen, candidates = snap.candidates,
                    parse_ok = snap.parse_ok, dropped = snap.dropped_full, "listener stats");
            },
            tick = tick_rx.recv() => {
                let Some(tick) = tick else { break };
                pipeline.on_tick(&tick).await;
            }
        }
    }

    warn!("streams ended; open positions need manual review");
}
