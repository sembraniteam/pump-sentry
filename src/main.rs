//! Paper pipeline binary. Default is paper; `--live` moves real funds.

mod cli;
mod config;
mod follow;
mod gate;
mod live;
mod live_mirror;
mod pipeline;
mod settings;
mod wal;

use std::sync::Arc;
use std::time::Duration;

use alert::Alerter;
use auditor::RugCheckClient;
use curve::{CurveTicker, PriceTick};
use executor::{JitoClient, LiveFundingReport, SwapExecutor, WalletGuard};
use listener::{DevScreenConfig, ListenerConfig, PumpFunListener, RpcScreener};
use migration::{PoolTick, VaultTicker};
use risk_engine::RiskEngine;
use simulator::PaperPortfolio;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use live::LiveTrader;
use pipeline::{AlertSink, Pipeline};
use settings::Settings;

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match cli::parse_cli(&argv) {
        cli::CliAction::Run { live } => run_pipeline(live).await,
        cli::CliAction::Start { live } => cli::cmd_start(live, "start"),
        cli::CliAction::Restart { live } => cli::cmd_restart(live),
        cli::CliAction::Stop => cli::cmd_stop(),
        cli::CliAction::Status => cli::cmd_status(),
        cli::CliAction::Logs { lines, follow } => cli::cmd_logs(lines, follow).await,
        cli::CliAction::Sell { mint } => cli::cmd_sell(&mint),
        cli::CliAction::Upgrade => cli::cmd_upgrade(),
        cli::CliAction::InitConfig => {
            let path = config::resolve_config_path();
            match config::ensure_config_file(&path) {
                Ok(true) => println!("wrote default config to {}", path.display()),
                Ok(false) => println!("config already exists at {}", path.display()),
                Err(e) => {
                    eprintln!("cannot write config at {}: {e}", path.display());
                    std::process::exit(1);
                }
            }
        }
        cli::CliAction::Help { error } => {
            if let Some(cause) = error {
                eprintln!("error: {cause}\n");
            }
            eprintln!("{}", cli::USAGE);
            std::process::exit(2);
        }
    }
}

/// WAL path next to the log file.
fn live_wal_path() -> std::path::PathBuf {
    let (_, log_file) = cli::resolve_paths();
    log_file
        .parent()
        .map(|d| d.join("live-wal.jsonl"))
        .unwrap_or_else(|| "live-wal.jsonl".into())
}

/// Boot replay of the WAL (live only): restores stranded custody.
async fn replay_live_wal(
    pipeline: &Arc<Pipeline<RugCheckClient, CurveTicker, LiveTrader, RpcScreener, Alerter>>,
    ticker: &CurveTicker,
) {
    use crate::wal;
    let path = live_wal_path();
    let replay = wal::replay_file(&path);
    if replay.pending.is_empty() && replay.positions.is_empty() && replay.manual.is_empty() {
        return;
    }
    let mut watched: Vec<(String, String)> = Vec::new();
    let mut n_pending = 0usize;
    let mut n_positions = 0usize;
    let mut n_manual = 0usize;
    for p in &replay.pending {
        watched.push((p.position.mint.clone(), p.position.curve.clone()));
    }
    wal::compact_file(&path, &replay);
    if let Some(trader) = pipeline.exec.live.as_ref() {
        n_pending = trader.import_pending(replay.pending);
    }
    {
        let mut guard = pipeline.exec.live_positions.lock().await;
        for pos in replay.positions {
            watched.push((pos.mint.clone(), pos.curve.clone()));
            if guard.contains_key(&pos.mint) {
                let key = format!("{}#wal-{n_positions}", pos.mint);
                guard.insert(key, pos);
            } else {
                guard.insert(pos.mint.clone(), pos);
            }
            n_positions += 1;
        }
    }
    {
        let mut guard = pipeline.exec.live_manual.lock().await;
        for item in replay.manual {
            let pos = item.position;
            watched.push((pos.mint.clone(), pos.curve.clone()));
            warn!(mint = %pos.mint, note = %item.note, "WAL replay: manual bag needs human exit (lookup above on explorer)");
            if guard.contains_key(&pos.mint) {
                let key = format!("{}#wal-{n_manual}", pos.mint);
                guard.insert(key, pos);
            } else {
                guard.insert(pos.mint.clone(), pos);
            }
            n_manual += 1;
        }
    }
    // Re-watch before loops start. Failures keep bags tracked.
    let mut unwatched = 0usize;
    watched.sort();
    watched.dedup();
    for (mint, curve) in &watched {
        if let Err(e) = ticker.watch(mint, curve).await {
            unwatched += 1;
            warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "WAL replay: re-watch failed; bag tracked without exit feed (manual review)");
        }
    }
    warn!(
        pending = n_pending,
        positions = n_positions,
        manual = n_manual,
        unwatched,
        file = %path.display(),
        "WAL replay: recovered live custody from unclean shutdown; MANUAL REVIEW REQUIRED (delete the file to clear resolved bags)"
    );
}

/// Foreground pipeline.
async fn run_pipeline(live_arg: bool) {
    // Must run before TLS: dual providers panic on auto-detect.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let daemon = std::env::var("BOT_DAEMON").is_ok_and(|v| v == "1");
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(!daemon)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,pump_sentry=debug")
            }),
        )
        .init();

    // SIGHUP reopens the log for logrotate.
    #[cfg(unix)]
    {
        tokio::spawn(async {
            let mut hup = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::hangup(),
            ) {
                Ok(s) => s,
                Err(_) => return,
            };
            loop {
                hup.recv().await;
                let (_, log_file) = cli::resolve_paths();
                match cli::reopen_log_file(&log_file) {
                    Ok(file) => {
                        if let Err(e) = cli::redirect_stdout_stderr_to(&file) {
                            eprintln!("log reopen failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("log reopen failed: {e}"),
                }
            }
        });
    }

    // File config: categorized config.toml is the only source for
    // tunables (INFERENCE_API_KEY stays env-only). The file is
    // auto-created (template with every tunable); a missing file
    // simply means defaults rule. Parse errors are fatal — a typo'd
    // value must never silently become a default.
    let config_path = config::resolve_config_path();
    match config::ensure_config_file(&config_path) {
        Ok(true) => println!("wrote default config to {}", config_path.display()),
        Ok(false) => {}
        Err(e) => {
            eprintln!("cannot set up config at {}: {e}", config_path.display());
            std::process::exit(1);
        }
    }
    let table = match config::read_table(&config_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bad config: {e}");
            std::process::exit(1);
        }
    };
    let settings = Settings::from_file(table.as_ref());
    // Scrub the bearer token from the process environment now that
    // Settings owns a copy: same rule as the trading keypair — secrets
    // live in guarded memory, not in `/proc` environ or later children.
    // (Test-only: tests set their own maps and never rely on this.)
    unsafe { std::env::remove_var("INFERENCE_API_KEY") };

    if CurveTicker::parse_commitment(&settings.commitment).is_none() {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
    }

    // Paper unless explicitly live.
    let live_mode = live_arg || std::env::var("IS_LIVE").is_ok_and(|v| v == "true");
    // Only exact `true` enables live.
    if !live_mode
        && let Ok(v) = std::env::var("IS_LIVE")
        && !v.is_empty()
        && v != "false"
        && v != "0"
        && v != "no"
    {
        warn!("config: IS_LIVE='{v}' ignored (only the exact value 'true' enables live; paper mode)");
    }
    {
        let invocation = std::env::var("BOT_INVOCATION").unwrap_or_else(|_| "run".to_string());
        let first_run = std::env::var("BOT_FIRST_RUN").is_ok_and(|v| v == "1");
        let kind = match (invocation.as_str(), first_run) {
            ("restart", _) => "restarted",
            ("start", true) => "started (first run)",
            ("start", false) => "started (re-run)",
            _ => "started (foreground)",
        };
        info!(
            "bot {kind} — pid {} mode {} unix_ts={}",
            std::process::id(),
            if live_mode { "live" } else { "paper" },
            settings::now_secs(),
        );
    }
    if live_mode {
        warn!("================================================================");
        warn!("LIVE MODE: real SOL will move via Jito bundles.");
        warn!("Starting in 5s — Ctrl-C now to abort (no confirmation prompt,");
        warn!("so automation never hangs; the delay is the abort window).");
        warn!("================================================================");
        // Ctrl-C inside the window exits before boot.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            _ = crate::cli::shutdown_signal() => {
                warn!("aborted during live startup window; exiting before boot");
                std::process::exit(0);
            }
        }
    }

    // Surface bad config (typos fall back silently).
    for warning in settings.warnings() {
        warn!("config: {warning}");
    }
    // Fatal misconfig refuses in every mode (not just live).
    if let Err(e) = settings.validate() {
        eprintln!("bad config: {e}");
        std::process::exit(1);
    }
    // Live gates: real funds never run on defaults.
    if live_mode {
        if !(settings.max_daily_loss_usd.is_finite() && settings.max_daily_loss_usd > 0.0) {
            eprintln!("live mode requires MAX_DAILY_LOSS_USD>0 (portfolio circuit breaker)");
            std::process::exit(1);
        }
        if !settings.sim_strict {
            eprintln!("live mode requires SIM_STRICT=true (unproven sims must refuse, not warn-and-proceed)");
            std::process::exit(1);
        }
        if settings.tip_lamports == 0 {
            eprintln!("live mode requires JITO_TIP_LAMPORTS>0 (tipless bundles may never land)");
            std::process::exit(1);
        }
        if settings.commitment == "processed" {
            eprintln!("live mode refuses COMMITMENT=processed (reorgable prints drive exits); use confirmed/finalized");
            std::process::exit(1);
        }
        if settings.quote_usd.is_some() {
            eprintln!("live mode refuses QUOTE_USD override (static fallback is paper-only by construction)");
            std::process::exit(1);
        }
        // Split-brain clusters can never land. Enforced when both identify.
        fn cluster_of(url: &str) -> Option<&'static str> {
            let lower = url.to_ascii_lowercase();
            if lower.contains("devnet") {
                Some("devnet")
            } else if lower.contains("testnet") {
                Some("testnet")
            } else if lower.contains("mainnet") {
                Some("mainnet")
            } else {
                None
            }
        }
        match (
            cluster_of(&settings.http_url),
            cluster_of(&settings.jito_endpoint),
        ) {
            (Some(rpc), Some(jito)) if rpc != jito => {
                eprintln!(
                    "live mode refuses split-brain clusters: RPC is {rpc}, Jito is {jito} (tips would never land)"
                );
                std::process::exit(1);
            }
            (None, _) | (_, None) => {
                warn!("config: RPC/Jito cluster unidentifiable from URLs; ensure both serve the same cluster");
            }
            _ => {}
        }
        if settings.priority_fee_micro_lamports == 0 {
            warn!("config: live without PRIORITY_FEE_MICRO_LAMPORTS — landing odds drop under congestion");
        }
    }

    let commitment = CurveTicker::parse_commitment(&settings.commitment).unwrap_or_else(|| {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
    });
    // One SOL/USD feed for pricing plus sizing.
    let sol_feed = curve::SolUsdFeed::with_defaults();
    let sol_handle = sol_feed.clone().spawn();
    let ticker = match CurveTicker::new(
        &settings.http_url,
        commitment,
        Duration::from_millis(settings.tick_interval_ms),
        Arc::clone(&sol_feed),
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
            warn!(error = %ids::strip_error_url(&e.to_string()), "bad inference endpoint; heuristic-only mode");
            None
        }
    };

    let start_balance = settings.start_balance;
    let fee_bps = settings.fee_bps;

    // Live stack (fatal on misconfig).
    let live = if live_mode {
        let wallet = match WalletGuard::load_from_env() {
            Ok(wallet) => wallet,
            Err(e) => {
                eprintln!("live mode requires SOLANA_PRIVATE_KEY: {e}");
                std::process::exit(1);
            }
        };
        info!(payer = %wallet.pubkey(), "live wallet loaded");
        if settings.sim_strict {
            info!("strict preflight on: unproven simulations refuse to dispatch");
        }
        let jito = match JitoClient::new(&settings.jito_endpoint) {
            Ok(jito) => jito,
            Err(e) => {
                eprintln!("bad JITO_ENDPOINT '{}': {e}", ids::redact_endpoint(&settings.jito_endpoint));
                std::process::exit(1);
            }
        };
        let executor = match SwapExecutor::new(&settings.http_url, commitment, wallet) {
            Ok(executor) => executor
                .with_jito(jito)
                .with_compute_budget(settings.compute_unit_limit, settings.priority_fee_micro_lamports)
                .with_strict_sim(settings.sim_strict),
            Err(e) => {
                eprintln!("bad executor config: {e}");
                std::process::exit(1);
            }
        };
        // Funding gate (read-only): native shortfall fails closed.
        {
            let report: LiveFundingReport =
                executor.live_funding_report(settings.tip_lamports).await;
            match report.native_ok() {
                Some(true) => info!(payer = %report.payer, balance = report.native_balance, required = report.native_required, "live native SOL covers tip + reserve"),
                Some(false) => {
                    eprintln!(
                        "live wallet {} short native SOL: have {} lamports, need {} (tip + 0.05 reserve); fund before live",
                        report.payer,
                        report.native_balance.unwrap_or(0),
                        report.native_required
                    );
                    std::process::exit(1);
                }
                None => warn!("live native balance unreadable; per-trade solvency decides"),
            }
            match report.wsol_funded() {
                Some(true) => info!(ata = %report.wsol_ata, held = report.wsol_balance, "live WSOL float covers token-paired quotes"),
                _ => {
                    warn!("no WSOL float at {} — token-paired quotes need one (SOL-pair buys pay native SOL and are unaffected).", report.wsol_ata);
                }
            }
        }
        Some(LiveTrader::new(
            executor,
            settings.slippage_bps,
            settings.tip_lamports,
        ).with_dispatch_fees(settings.compute_unit_limit, settings.priority_fee_micro_lamports).with_adaptive_slippage(settings.slippage_vol_window_secs, settings.slippage_vol_extra_cap_bps).with_wal(live_wal_path()))
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
            sig_limit: settings.dev_sig_limit.max(settings.dev_screen_sample_txs).clamp(1, 1000),
        },
    ) {
        Ok(screener) => screener,
        Err(e) => {
            eprintln!("bad screener config: {e}");
            std::process::exit(1);
        }
    };

    let gate_interval = Duration::from_millis(settings.gate_poll_ms);

    // Zero intervals panic or spin: fail closed.
    if settings.gate_poll_ms == 0 {
        eprintln!("bad GATE_POLL_MS=0: gate poll interval must be positive");
        std::process::exit(1);
    }
    if settings.tick_interval_ms == 0 {
        eprintln!("bad TICK_INTERVAL_MS=0: tick interval must be positive");
        std::process::exit(1);
    }
    if settings.vault_tick_interval_ms == 0 {
        eprintln!("bad VAULT_TICK_INTERVAL_MS=0: vault tick interval must be positive");
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
        Duration::from_millis(settings.vault_tick_interval_ms),
        Arc::clone(&sol_feed),
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
        sol_feed,
    );

    // Crash replay (live only).
    if live_mode {
        replay_live_wal(&pipeline, &ticker).await;
    }

    // Ticker loop: poll curves, auto-close.
    let (tick_tx, mut tick_rx) = mpsc::channel::<PriceTick>(1024);
    let tick_handle = tokio::spawn(async move { ticker.run(tick_tx).await });

    // Pool loop: Raydium prices, spike alerts.
    let (pool_tx, mut pool_rx) = mpsc::channel::<PoolTick>(1024);
    let pool_handle = tokio::spawn(async move { vault_ticker.run(pool_tx).await });

    // Gate loop: overlaps skip, never double-confirm.
    let mut gate_timer = tokio::time::interval(gate_interval);
    gate_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    /// Heartbeat cadence.
    const HEARTBEAT_SECS: u64 = 6 * 3_600;
    let boot_ts = settings::now_secs();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // In-flight tasks never cancel mid-commit. Bounded; drops loudly.
    let mut in_flight = tokio::task::JoinSet::new();
    const MAX_IN_FLIGHT: usize = 32;
    /// Manual-sell drain cadence in gate rounds.
    const MANUAL_SELL_DRAIN_EVERY: u64 = 15;
    let mut gate_rounds: u64 = 0;
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let stats_task = Arc::clone(&stats);
    info!("pipeline live with auto-exits (Ctrl-C to stop)");
    let mut clean_shutdown = false;
    loop {
        // Reap finished units.
        while in_flight.try_join_next().is_some() {}
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                let pipeline = Arc::clone(&pipeline);
                let stats = Arc::clone(&stats_task);
                let permits = Arc::clone(&permits);
                in_flight.spawn(async move {
                    let Ok(_permit) = permits.try_acquire_owned() else {
                        debug!(mint = %event.mint, "launch dropped: dispatch saturated");
                        return;
                    };
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
                    let Ok(_permit) = permits.try_acquire_owned() else {
                        // Tick drops are warn (launches stay debug).
                        warn!(mint = %tick.mint, "tick dropped: dispatch saturated; exit delayed");
                        return;
                    };
                    pipeline.on_tick(&tick).await;
                });
            }
            pool = pool_rx.recv() => {
                let Some(pool) = pool else { break };
                let pipeline = Arc::clone(&pipeline);
                let permits = Arc::clone(&permits);
                in_flight.spawn(async move {
                    let Ok(_permit) = permits.try_acquire_owned() else {
                        debug!(mint = %pool.mint, "pool tick dropped: dispatch saturated");
                        return;
                    };
                    pipeline.on_pool_tick(&pool).await;
                });
            }
            _ = heartbeat.tick() => {
                let snap = stats_task.snapshot();
                let pending = pipeline.pending_count().await;
                let open = pipeline.open_position_mints().await.len();
                let manual = pipeline.exec.manual_keys().await.len();
                let diverged = pipeline.exec.diverged_keys().await.len();
                let pending_buys = pipeline.exec.live_pending_count();
                let (cash, equity) = pipeline.paper_cash_equity().await;
                let (breaker_halted, day_net) = {
                    let mut daily = pipeline.daily.lock().await;
                    let halted = daily.tripped(
                        pipeline.settings.max_daily_loss_usd,
                        settings::now_secs(),
                    );
                    (halted, daily.day_net)
                };
                let card = Alerter::format_heartbeat(&alert::HeartbeatReport {
                    mode: if live_mode { "live" } else { "paper" },
                    uptime_secs: settings::now_secs().saturating_sub(boot_ts),
                    logs_seen: snap.logs_seen,
                    candidates: snap.candidates,
                    parse_ok: snap.parse_ok,
                    dropped: snap.dropped_full.saturating_add(snap.dropped_throttled),
                    watchlist_pending: pending,
                    open_positions: open,
                    manual_bags: manual,
                    diverged,
                    pending_buys,
                    breaker_halted,
                    day_net,
                    strategy: pipeline.strategy.as_str(),
                    cash_usd: cash,
                    equity_usd: equity,
                });
                info!("heartbeat: logs={} candidates={} watchlist={} open={} manual={} diverged={} pending_buys={} equity={:.2}",
                    snap.logs_seen, snap.candidates, pending, open, manual, diverged, pending_buys, equity);
                // Burn-in thresholds (all 0 = off): log-only, never cards.
                let thresholds = &pipeline.settings;
                if thresholds.alert_diverged_warn_n > 0 && diverged as u64 >= thresholds.alert_diverged_warn_n {
                    warn!("alert: {diverged} diverged mints (paper without live cover) >= ALERT_DIVERGED_WARN_N={}; review funding/dispatch, or accept paper-only", thresholds.alert_diverged_warn_n);
                }
                if let Some(engine) = pipeline.engine.as_ref() {
                    let decisions = engine.decision_count();
                    let fallbacks = engine.fallback_count();
                    if thresholds.alert_risk_fallback_warn_pct > 0 && decisions > 0 {
                        let pct = fallbacks.saturating_mul(100).saturating_div(decisions);
                        if pct >= thresholds.alert_risk_fallback_warn_pct {
                            warn!("alert: risk-engine fallback rate {pct}% ({fallbacks}/{decisions}) >= ALERT_RISK_FALLBACK_WARN_PCT={}; model may be down, entries run heuristic", thresholds.alert_risk_fallback_warn_pct);
                        }
                    }
                }
                if thresholds.alert_giveup_burst_n > 0 {
                    let now = settings::now_secs();
                    let burst = pipeline.exec.giveups_since(thresholds.alert_giveup_window_secs, now).await;
                    if burst as u64 >= thresholds.alert_giveup_burst_n {
                        warn!("alert: {burst} GiveUps in trailing {}s >= ALERT_GIVEUP_BURST_N={}; systemic dispatch trouble, check RPC/Jito before funding more", thresholds.alert_giveup_window_secs, thresholds.alert_giveup_burst_n);
                    }
                }
                if let Some(alerts) = pipeline.alerts.as_ref() {
                    alerts.alert(card);
                }
            }
            _ = gate_timer.tick() => {
                // Manual-sell drain every 15th round.
                gate_rounds = gate_rounds.wrapping_add(1);
                if gate_rounds.is_multiple_of(MANUAL_SELL_DRAIN_EVERY) {
                    let (_, log_file) = cli::resolve_paths();
                    for (mint, path) in cli::pending_sell_requests(&log_file) {
                        // Body must name the file mint; else drop.
                        let body_ok = std::fs::read_to_string(&path)
                            .ok()
                            .and_then(|b| cli::sell_request_body_mint(&b))
                            .is_some_and(|m| m == mint);
                        if !body_ok {
                            warn!(%mint, path = %path.display(), "manual sell request body mismatch; dropping request");
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                        if pipeline.manual_sell(&mint).await {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                if pipeline.try_begin_poll().await {
                    let pipeline = Arc::clone(&pipeline);
                    in_flight.spawn(async move {
                        pipeline.poll_gates().await;
                        pipeline.end_poll().await;
                    });
                }
            }
            _ = cli::shutdown_signal() => {
                info!("shutdown requested (signal)");
                clean_shutdown = true;
                break;
            }
        }
    }

    // Shutdown: stop producers, snapshot, drain bounded.
    rx.close();
    tick_handle.abort();
    pool_handle.abort();
    listener_handle.abort();
    sol_handle.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), listener_handle).await;

    // Snapshot BEFORE the drain.
    let snap_ts = settings::now_secs();
    let paper_open = pipeline.open_position_mints().await;
    let live_tracked = pipeline.live_tracked_mints().await;
    let watchlist_pending = pipeline.pending_count().await;
    let mut summary = cli::ShutdownSummary {
        ts: snap_ts,
        pid: std::process::id(),
        live: live_mode,
        paper_open: paper_open.clone(),
        live_tracked: live_tracked.clone(),
        watchlist_pending,
        clean: false,
    };
    let snap_path = {
        let (_, log_file) = cli::resolve_paths();
        log_file
            .parent()
            .map(|d| d.join(format!("shutdown-{snap_ts}.json")))
            .unwrap_or_else(|| format!("shutdown-{snap_ts}.json").into())
    };
    let write_snapshot = |summary: &cli::ShutdownSummary| {
        if let Err(e) = cli::write_private_file(&snap_path, summary.to_json().as_bytes()) {
            warn!(
                "shutdown snapshot unwritable ({}): {e}",
                snap_path.display()
            );
        } else {
            info!("shutdown snapshot: {}", snap_path.display());
        }
    };
    write_snapshot(&summary);
    if !paper_open.is_empty() {
        warn!(
            "shutdown with {} open paper position(s): {:?}",
            paper_open.len(),
            paper_open
        );
    }
    if !live_tracked.is_empty() {
        warn!(
            "shutdown with {} tracked live fill(s): {:?} — MANUAL REVIEW REQUIRED",
            live_tracked.len(),
            live_tracked
        );
    }
    if watchlist_pending > 0 {
        info!("shutdown with {watchlist_pending} watchlisted mint(s) pending quorum (dropped)");
    }

    let drain = async {
        while in_flight.join_next().await.is_some() {}
    };
    // Second signal force-aborts.
    let drained = tokio::select! {
        out = tokio::time::timeout(Duration::from_secs(30), drain) => out.is_ok(),
        _ = cli::shutdown_signal() => {
            warn!("second shutdown signal; force-aborting in-flight units");
            in_flight.abort_all();
            false
        }
    };
    if !drained {
        warn!("shutdown drain timed out; aborting in-flight units");
        in_flight.abort_all();
    }
    // Re-snapshot AFTER the drain.
    let paper_open = pipeline.open_position_mints().await;
    let live_tracked = pipeline.live_tracked_mints().await;
    let watchlist_pending = pipeline.pending_count().await;
    summary.paper_open = paper_open.clone();
    summary.live_tracked = live_tracked.clone();
    summary.watchlist_pending = watchlist_pending;
    summary.clean = drained && clean_shutdown;
    write_snapshot(&summary);
    if !paper_open.is_empty() || !live_tracked.is_empty() {
        warn!(
            "shutdown final state: {} open paper, {} tracked live {:?} — MANUAL REVIEW REQUIRED",
            paper_open.len(),
            live_tracked.len(),
            live_tracked
        );
    }
    if !drained {
        warn!("in-flight units were aborted; any live dispatch mid-confirm has an UNKNOWN fill — check the explorer for the tracked mints above");
    }

    if clean_shutdown {
        info!("shutdown complete (drained={drained})");
    } else {
        // Streams ended: exit nonzero so supervisors notice.
        warn!("streams ended; open positions need manual review");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    /// Guards the rustls fix: a default must be installed.
    #[test]
    fn crypto_provider_installs() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    /// Heuristic cap must stay within the auditor gate.
    #[test]
    fn heuristic_cap_within_audit_cap() {
        assert!(
            risk_engine::fallback::RUGCHECK_SCORE_CAP <= auditor::AuditPolicy::default().max_score
        );
    }
}
