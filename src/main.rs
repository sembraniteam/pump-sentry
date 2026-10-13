//! Paper pipeline binary: listen → audit → reserves → risk → simulate.
//!
//! Default mode is paper trading (no funds move). With `--live` (or
//! `IS_LIVE=true`) the same signals additionally dispatch real Jito-routed
//! swaps through `crates/executor` — read its README before enabling.
//!
//! The binary never runs bare: it only acts through an explicit
//! subcommand (`start` / `stop` / `restart` / `status` / `logs` / `run`).
//! Anything else prints usage and exits without touching the pipeline.
//!
//! Wiring only — all logic lives in [`settings`] (knobs), [`pipeline`]
//! (launch handling, tick exits), [`live`] (live sizing/dispatch), and
//! [`cli`] (supervisor commands, daemon files, shutdown signals),
//! all unit-tested.

mod cli;
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
        cli::CliAction::Help { error } => {
            if let Some(cause) = error {
                eprintln!("error: {cause}\n");
            }
            eprintln!("{}", cli::USAGE);
            std::process::exit(2);
        }
    }
}

/// WAL path next to the log file (same dir as shutdown snapshots).
fn live_wal_path() -> std::path::PathBuf {
    let (_, log_file) = cli::resolve_paths();
    log_file
        .parent()
        .map(|d| d.join("live-wal.jsonl"))
        .unwrap_or_else(|| "live-wal.jsonl".into())
}

/// Boot replay of the live WAL (live mode only): restores custody that a
/// SIGKILL/abort stranded mid-dispatch. Pending buys re-enter the recheck
/// loop, tracked/manual bags regain exit automation via re-watch (the
/// ticker watch set is in-memory and starts empty). Loud by design: any
/// recovery means the last shutdown was unclean.
async fn replay_live_wal(
    pipeline: &Arc<Pipeline<RugCheckClient, CurveTicker, LiveTrader, RpcScreener, Alerter>>,
    ticker: &CurveTicker,
) {
    use crate::live::wal;
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
    // Compact first: the file is rewritten to the surviving state before
    // any in-memory import, so a crash mid-import replays identically.
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
    // Re-watch BEFORE the loops start (no ticks otherwise, no exits).
    // A failed watch keeps the bag tracked + reported (same posture as
    // the open-time watch failure: custody first, feed second).
    let mut unwatched = 0usize;
    watched.sort();
    watched.dedup();
    for (mint, curve) in &watched {
        if let Err(e) = ticker.watch(mint, curve).await {
            unwatched += 1;
            warn!(%mint, error = %e, "WAL replay: re-watch failed; bag tracked without exit feed (manual review)");
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

/// Foreground pipeline: the body the old bare binary ran. `live_arg`
/// comes from `run --live` (the daemon child passes it through); the
/// `IS_LIVE=true` environment fallback is honored as before.
async fn run_pipeline(live_arg: bool) {
    // Must run before any TLS use: the dependency tree enables both the
    // `ring` and `aws-lc-rs` rustls providers, so auto-detection panics.
    // `Err` only means another call already installed one — safe to ignore.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Daemon children redirect stdout/stderr to the log file: no ANSI
    // color codes there (a colorized log is unreadable ops noise).
    // Targets are dropped for the same reason (long module paths are
    // noise in an ops log); levels + timestamps stay.
    let daemon = std::env::var("BOT_DAEMON").is_ok_and(|v| v == "1");
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(!daemon)
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
    // `--live` now arrives via `run --live` (or `start --live` through
    // the daemon child); the `IS_LIVE=true` fallback is unchanged.
    let live_mode = live_arg || std::env::var("IS_LIVE").is_ok_and(|v| v == "true");
    // `get_bool` elsewhere accepts 1/yes, but the live gate accepts ONLY
    // the exact string `true`: a set-but-unrecognized value (e.g.
    // `IS_LIVE=1`) must never silently paper-trade when the operator
    // meant live — say so loudly at startup.
    if !live_mode
        && let Ok(v) = std::env::var("IS_LIVE")
        && !v.is_empty()
        && v != "false"
        && v != "0"
        && v != "no"
    {
        warn!("config: IS_LIVE='{v}' ignored (only the exact value 'true' enables live; paper mode)");
    }
    // Startup banner: the supervisor log records WHEN this boot happened
    // and whether it was a first run or a re-run/restart, so the uptime
    // question is answered by reading the log, not by memory.
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
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Surface suspicious config (typos fall back to defaults silently).
    for warning in settings.warnings() {
        warn!("config: {warning}");
    }
    // Live fail-closed gates: real funds never run on warn-and-proceed
    // defaults (no breaker, unproven sims, unbounded guards, tipless
    // bundles, reorgable prints, or static fallback prices).
    if live_mode {
        if !(settings.max_daily_loss_usd.is_finite() && settings.max_daily_loss_usd > 0.0) {
            eprintln!("live mode requires MAX_DAILY_LOSS_USD>0 (portfolio circuit breaker)");
            std::process::exit(1);
        }
        if !settings.sim_strict {
            eprintln!("live mode requires SIM_STRICT=true (unproven sims must refuse, not warn-and-proceed)");
            std::process::exit(1);
        }
        if settings.slippage_bps >= 10_000 {
            eprintln!("live mode refuses SLIPPAGE_BPS={} (>=100%: buys would authorize 2x spend while every sell fails closed)", settings.slippage_bps);
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
        if settings.priority_fee_micro_lamports == 0 {
            warn!("config: live without PRIORITY_FEE_MICRO_LAMPORTS — landing odds drop under congestion");
        }
    }

    let commitment = CurveTicker::parse_commitment(&settings.commitment).unwrap_or_else(|| {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
    });
    // One dynamic SOL/USD feed for all pricing + live sizing (no statics).
    let sol_feed = curve::SolUsdFeed::with_defaults();
    // Background refresher: runs for process lifetime (handle aborts on
    // shutdown below).
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
        if settings.sim_strict {
            info!("strict preflight on: unproven simulations refuse to dispatch");
        }
        let jito = match JitoClient::new(&settings.jito_endpoint) {
            Ok(jito) => jito,
            Err(e) => {
                eprintln!("bad JITO_ENDPOINT '{}': {e}", settings.jito_endpoint);
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
        Some(LiveTrader::new(
            executor,
            settings.slippage_bps,
            settings.tip_lamports,
        ).with_dispatch_fees(settings.compute_unit_limit, settings.priority_fee_micro_lamports).with_wal(live_wal_path()))
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

    // Crash-recovery replay (live only): the WAL restores custody that a
    // SIGKILL/abort stranded mid-dispatch — pending buys re-enter the
    // recheck loop, tracked/manual bags regain exit automation. Every
    // recovered mint is re-watched (the ticker watch set is in-memory)
    // and reported loudly; delete the WAL file to clear resolved manual
    // bags (compaction rewrites it to the surviving state).
    if live_mode {
        replay_live_wal(&pipeline, &ticker).await;
    }

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

    // Heartbeat (P3.2): proof-of-life on quiet markets — a card every
    // 6h with throughput counters + cash so silence reads as "alive",
    // never "hung". First tick fires immediately (boot card).
    /// Heartbeat cadence in seconds.
    const HEARTBEAT_SECS: u64 = 6 * 3_600;
    let boot_ts = settings::now_secs();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // In-flight units run as JoinSet tasks so Ctrl-C never cancels a
    // half-applied launch/exit mid-await (paper-open without live mirror
    // and similar partial commits). Shutdown drains them bounded.
    //
    // Concurrency is bounded by a semaphore: every event fans out to
    // RPC-heavy work, and an unbounded spawn lets a Pump.fun burst open
    // thousands of concurrent RPCs (rate-limit + FD exhaustion). Permits
    // are `try_acquire`d — never awaited — so saturation drops (loudly)
    // instead of queueing an unbounded JoinSet backlog that only burns
    // memory while the RPCs stay throttled anyway.
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
                    let Ok(_permit) = permits.try_acquire_owned() else {
                        debug!(mint = %event.mint, "launch dropped: dispatch saturated");
                        return;
                    };
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
                    let Ok(_permit) = permits.try_acquire_owned() else {
                        // Ticks drive TP/SL exits: dropping one delays a
                        // live exit, so this is warn (launches are
                        // fail-closed drops and stay debug below).
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
                let (cash, equity) = pipeline.paper_cash_equity().await;
                let card = Alerter::format_heartbeat(&alert::HeartbeatReport {
                    mode: if live_mode { "live" } else { "paper" },
                    uptime_secs: settings::now_secs().saturating_sub(boot_ts),
                    logs_seen: snap.logs_seen,
                    candidates: snap.candidates,
                    parse_ok: snap.parse_ok,
                    dropped: snap.dropped_full.saturating_add(snap.dropped_throttled),
                    watchlist_pending: pending,
                    open_positions: open,
                    cash_usd: cash,
                    equity_usd: equity,
                });
                info!("heartbeat: logs={} candidates={} watchlist={} open={} equity={:.2}",
                    snap.logs_seen, snap.candidates, pending, open, equity);
                if let Some(alerts) = pipeline.alerts.as_ref() {
                    alerts.alert(card);
                }
            }
            _ = gate_timer.tick() => {
                // Manual-sell drain (`sell <MINT>` request files): every
                // round, unconditionally — exits must not wait on the
                // gate slot. Settled requests are deleted; holding ones
                // stay for the next round.
                {
                    let (_, log_file) = cli::resolve_paths();
                    for (mint, path) in cli::pending_sell_requests(&log_file) {
                        // Body must name the same mint as the filename:
                        // anything else is forged or torn — drop loudly,
                        // never act on it, never retry it.
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
            // SIGINT (Ctrl-C) AND SIGTERM (`stop`, systemd, docker): the
            // old code watched Ctrl-C only, so every supervisor stop
            // killed the process mid-commit (paper-open without live
            // mirror and friends) — the non-atomic shutdown.
            _ = cli::shutdown_signal() => {
                info!("shutdown requested (signal)");
                clean_shutdown = true;
                break;
            }
        }
    }

    // Atomic shutdown in three ordered steps: (1) stop producers so no
    // NEW work enters, (2) snapshot what is still open so nothing is
    // lost silently, (3) drain in-flight units bounded — and ABORT what
    // remains past the deadline instead of exiting over live tasks. A
    // second signal during the drain force-aborts immediately so a
    // wedged RPC cannot hold the process hostage (the snapshot then
    // records `clean: false`).
    // The listener loop exits on its own once the event channel closes...
    // except it only checks between sessions, so close the channel AND
    // abort the task, then await it briefly for a clean join.
    rx.close();
    tick_handle.abort();
    pool_handle.abort();
    listener_handle.abort();
    sol_handle.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), listener_handle).await;

    // Snapshot BEFORE the drain: paper opens, tracked live fills, and
    // the watchlist, frozen to `logs/shutdown-<ts>.json` plus loud log
    // lines. Best-effort file IO (a snapshot failure must not fail the
    // shutdown itself); the log lines always fire.
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
        clean: false, // fixed up below once the drain outcome is known
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
    // Second signal during the drain: force-abort, snapshot dirty, exit.
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
    // Re-snapshot AFTER the drain/abort: the pre-drain picture is stale —
    // an abort can cancel a confirm mid-flight (fill unknown) and the
    // drain may have settled paper/live state. The rewritten file is the
    // source of truth; `clean: false` plus the abort warning below tells
    // the operator to check the explorer for in-flight live mints.
    let paper_open = pipeline.open_position_mints().await;
    let live_tracked = pipeline.live_tracked_mints().await;
    let watchlist_pending = pipeline.pending_count().await;
    summary.paper_open = paper_open.clone();
    summary.live_tracked = live_tracked.clone();
    summary.watchlist_pending = watchlist_pending;
    // Record the true outcome in the snapshot (a timed-out/force-aborted
    // drain leaves `clean: false` for the operator to notice).
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
