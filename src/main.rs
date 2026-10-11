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
use migration::{PoolTick, VaultTicker};
use risk_engine::RiskEngine;
use simulator::PaperPortfolio;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,solana_snipe_bot=debug")
            }),
        )
        .init();

    let settings = Settings::from_env();

    // Fail closed on unknown commitment (no silent coercion to confirmed).
    if !matches!(settings.commitment.as_str(), "processed" | "confirmed" | "finalized") {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
    }

    // Mode gate: paper unless explicitly opted into live risk.
    let live_mode = std::env::args().any(|a| a == "--live")
        || std::env::var("IS_LIVE").is_ok_and(|v| v == "true");
    if live_mode {
        warn!("================================================================");
        warn!("LIVE MODE: real SOL will move via Jito bundles.");
        warn!("Starting in 5s — Ctrl-C now to abort (no confirmation prompt,");
        warn!("so automation never hangs; the delay is the abort window).");
        warn!("================================================================");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Surface suspicious config (typos fall back to defaults silently).
    for warning in settings.warnings() {
        warn!("config: {warning}");
    }
    if live_mode && !(settings.max_daily_loss_usd.is_finite() && settings.max_daily_loss_usd > 0.0) {
        warn!("config: live mode without MAX_DAILY_LOSS_USD — no portfolio circuit breaker; set one");
    }

    let commitment = CurveTicker::parse_commitment(&settings.commitment).unwrap_or_else(|| {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
    });
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

    let (mut rx, stats, listener_handle) = match PumpFunListener::subscribe(ListenerConfig {
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
            Ok(jito) => jito,
            Err(e) => {
                eprintln!("bad JITO_ENDPOINT '{}': {e}", settings.jito_endpoint);
                std::process::exit(1);
            }
        };
        let executor = match SwapExecutor::new(&settings.http_url, commitment, wallet) {
            Ok(executor) => {
                let executor = executor
                    .with_jito(jito)
                    .with_compute_budget(settings.compute_unit_limit, settings.priority_fee_micro_lamports);
                match &settings.fee_recipient {
                    Some(recipient) => match executor::parse_pubkey(recipient) {
                        Ok(pubkey) => executor.with_fee_recipient(pubkey),
                        Err(e) => {
                            eprintln!("bad FEE_RECIPIENT: {e}");
                            std::process::exit(1);
                        }
                    },
                    None => executor,
                }
            }
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

    // Zero poll intervals panic (`tokio::time::interval(ZERO)`) or spin
    // the RPC hot: fail closed instead of crashing or burning quota.
    if settings.gate_poll_ms == 0 {
        eprintln!("bad GATE_POLL_MS=0: gate poll interval must be positive");
        std::process::exit(1);
    }
    if settings.tick_interval_ms == 0 {
        eprintln!("bad TICK_INTERVAL_MS=0: tick interval must be positive");
        std::process::exit(1);
    }

    let alerts = settings.discord_alerts.then(|| match &settings.discord_channel {
        Some(channel) => {
            info!(%channel, "discord alerts routed to channel");
            Alerter::new().with_channel(channel)
        }
        None => Alerter::new(),
    });
    if alerts.is_none() {
        info!("discord alerts disabled (DISCORD_ALERTS=false)");
    }

    let vault_ticker = match VaultTicker::new(
        &settings.http_url,
        commitment,
        Duration::from_millis(settings.tick_interval_ms),
        settings.sol_usd,
    ) {
        Ok(ticker) => ticker,
        Err(e) => {
            eprintln!("bad vault ticker config: {e}");
            std::process::exit(1);
        }
    };

    let auditor = match RugCheckClient::new() {
        Ok(auditor) => auditor,
        Err(e) => {
            eprintln!("bad auditor config: {e}");
            std::process::exit(1);
        }
    };

    let pipeline = Pipeline::shared(
        settings,
        auditor,
        engine,
        ticker.clone(),
        PaperPortfolio::new(start_balance, fee_bps),
        live,
        screener,
        alerts,
        vault_ticker.clone(),
    );

    // Ticker loop: poll watched curves → auto-close on TP/SL.
    let (tick_tx, mut tick_rx) = mpsc::channel::<PriceTick>(1024);
    let tick_handle = tokio::spawn(async move { ticker.run(tick_tx).await });

    // Pool loop: post-migration Raydium prices → spike alerts.
    let (pool_tx, mut pool_rx) = mpsc::channel::<PoolTick>(1024);
    let pool_handle = tokio::spawn(async move { vault_ticker.run(pool_tx).await });

    // Gate loop: poll the watchlist for buyer quorum → confirmed entries.
    // Each round runs as a JoinSet unit; overlapping rounds are skipped
    // so two polls can never confirm the same mint twice.
    let mut gate_timer = tokio::time::interval(gate_interval);
    gate_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // In-flight units run as JoinSet tasks so Ctrl-C never cancels a
    // half-applied launch/exit mid-await (paper-open without live mirror
    // and similar partial commits). Shutdown drains them bounded.
    //
    // Concurrency is bounded by a semaphore: every event fans out to
    // RPC-heavy work, and an unbounded spawn lets a Pump.fun burst open
    // thousands of concurrent RPCs (rate-limit + FD exhaustion). Tasks
    // hold a permit for their whole run, so at most 32 dispatch at once;
    // the rest wait without touching the network.
    let mut in_flight = tokio::task::JoinSet::new();
    const MAX_IN_FLIGHT: usize = 32;
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let stats_task = Arc::clone(&stats);
    info!("pipeline live with auto-exits (Ctrl-C to stop)");
    // Tracks a clean Ctrl-C exit: any other loop end (closed channels,
    // dead streams) exits nonzero so supervisors notice.
    let mut clean_shutdown = false;
    loop {
        // Reap finished units every iteration: JoinSet retains completed
        // outputs until joined, so without this the set grows without
        // bound over a long run (slow leak at Pump.fun event rates).
        while in_flight.try_join_next().is_some() {}
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                let pipeline = Arc::clone(&pipeline);
                let stats = Arc::clone(&stats_task);
                let permits = Arc::clone(&permits);
                in_flight.spawn(async move {
                    let Ok(_permit) = permits.acquire_owned().await else { return };
                    // Owned first: borrowing `.as_deref()` temporaries
                    // across the `.await` below would self-borrow the
                    // future (rejected); named locals live in the future
                    // state like any other capture.
                    let creator = event.creator.as_ref().map(|c| c.to_string());
                    pipeline
                        .on_launch(
                            &event.mint.to_string(),
                            &event.bonding_curve.to_string(),
                            &event.associated_bonding_curve.to_string(),
                            &event.user.to_string(),
                            creator.as_deref(),
                            event.slot,
                        )
                        .await;
                    let snap = stats.snapshot();
                    let pending = pipeline.pending_count().await;
                    debug!(logs = snap.logs_seen, candidates = snap.candidates,
                        parse_ok = snap.parse_ok, dropped = snap.dropped_full,
                        throttled = snap.dropped_throttled,
                        watchlist = pending, "listener stats");
                });
            },
            tick = tick_rx.recv() => {
                let Some(tick) = tick else { break };
                let pipeline = Arc::clone(&pipeline);
                let permits = Arc::clone(&permits);
                in_flight.spawn(async move {
                    let Ok(_permit) = permits.acquire_owned().await else { return };
                    pipeline.on_tick(&tick).await;
                });
            }
            pool = pool_rx.recv() => {
                let Some(pool) = pool else { break };
                let pipeline = Arc::clone(&pipeline);
                let permits = Arc::clone(&permits);
                in_flight.spawn(async move {
                    let Ok(_permit) = permits.acquire_owned().await else { return };
                    pipeline.on_pool_tick(&pool).await;
                });
            }
            _ = gate_timer.tick() => {
                if pipeline.try_begin_poll().await {
                    let pipeline = Arc::clone(&pipeline);
                    in_flight.spawn(async move {
                        pipeline.poll_gates().await;
                        pipeline.end_poll().await;
                    });
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                clean_shutdown = true;
                break;
            }
        }
    }

    // Stop producers first, then let in-flight units finish (bounded).
    // The listener loop exits on its own once the event channel closes...
    // except it only checks between sessions, so close the channel AND
    // abort the task, then await it briefly for a clean join.
    rx.close();
    tick_handle.abort();
    pool_handle.abort();
    listener_handle.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), listener_handle).await;
    let drain = async {
        while in_flight.join_next().await.is_some() {}
    };
    if tokio::time::timeout(Duration::from_secs(30), drain)
        .await
        .is_err()
    {
        warn!("shutdown drain timed out; abandoning in-flight units");
    }

    if clean_shutdown {
        info!("shutdown complete");
    } else {
        // Streams ended without Ctrl-C (dead RPC, closed channels):
        // exit nonzero so supervisors and systemd notice the outage.
        warn!("streams ended; open positions need manual review");
        std::process::exit(1);
    }
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

    /// Cross-crate invariant: the risk-engine heuristic cap must stay at
    /// or below the auditor's hard gate, or scores 351–400 would resolve
    /// to BUY-side profiles the auditor would have rejected outright.
    #[test]
    fn heuristic_cap_within_audit_cap() {
        assert!(
            risk_engine::fallback::RUGCHECK_SCORE_CAP <= auditor::AuditPolicy::default().max_score
        );
    }
}
