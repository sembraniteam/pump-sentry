//! Paper pipeline binary. Default is paper; `--live` moves real funds.

mod cli;
mod config;
mod day_state;
mod discord_sidecar;
mod follow;
mod gate;
mod journal;
mod live;
mod live_mirror;
mod money;
mod pipeline;
mod replay;
mod research;
mod settings;
mod status;

#[cfg(test)]
mod test_env;

mod wal;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use alert::Alerter;
use auditor::RugCheckClient;
use curve::{CurveTicker, PriceTick};
use executor::{FundingPrincipal, JitoClient, LiveFundingReport, SwapExecutor, WalletGuard};
use listener::{DevScreenConfig, ListenerConfig, PumpFunListener, RpcScreener};
use migration::{PoolTick, VaultTicker};
use risk_engine::RiskEngine;
use simulator::PaperPortfolio;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use gate::LaunchRequest;
use live::LiveTrader;
use pipeline::{AlertSink, Pipeline};
use settings::Settings;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match cli::parse_cli(&argv) {
        // The trader runs through the synchronous boot phase first:
        // lock, settings, and secret capture happen before any thread
        // exists (AUD-001, AUD-015).
        cli::CliAction::Run { live } => run_binary(live),
        cli::CliAction::Start { live } => cli::cmd_start(live, "start"),
        cli::CliAction::Restart { live } => cli::cmd_restart(live),
        cli::CliAction::Stop => cli::cmd_stop(),
        cli::CliAction::Status => cli::cmd_status(),
        // Read-only/aux actions never mutate the environment or touch
        // state files: a minimal current-thread runtime is enough.
        cli::CliAction::Logs { lines, follow } => {
            aux_runtime().block_on(cli::cmd_logs(lines, follow))
        }
        cli::CliAction::Sell { mint } => cli::cmd_sell(&mint),
        cli::CliAction::Pause => cli::cmd_pause(),
        cli::CliAction::Resume => cli::cmd_resume(),
        // The sidecar needs the token captured in this sync phase
        // (before any thread), then its own runtime: a gateway
        // connection, unlike the aux commands below, runs forever.
        cli::CliAction::Discord => {
            let Some(token) = alert::capture_bot_token() else {
                eprintln!(
                    "Discord sidecar refusing to start: {} (or {}_FILE) is not set — the sidecar owns the Discord connection, so it cannot run tokenless",
                    alert::DISCORD_BOT_TOKEN_ENV,
                    alert::DISCORD_BOT_TOKEN_ENV
                );
                std::process::exit(1);
            };
            // Indirect to keep the failure exit code the caller's.
            if let Err(e) = discord_sidecar::run(token) {
                eprintln!("Discord sidecar stopping: {e}");
                std::process::exit(1);
            }
        }
        cli::CliAction::Replay { file } => aux_runtime().block_on(replay::cmd_replay(file)),
        cli::CliAction::Research { file } => aux_runtime().block_on(research::cmd_research(file)),
        cli::CliAction::Upgrade { allow_unverified } => cli::cmd_upgrade(allow_unverified),
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

/// Runtime for the aux commands (logs/replay/research): no env
/// mutation, no state ownership — current-thread is enough and keeps
/// those paths off the trader's boot phase.
fn aux_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("aux tokio runtime")
}

/// Values captured in the synchronous boot phase, before any thread
/// exists. Secrets and env-derived settings cross into the async world
/// as owned data; nothing re-reads (or mutates) the environment after
/// boot (AUD-015). The state-dir lock is acquired here too — before
/// the config file, secrets, WAL, or day-state are touched (AUD-001).
struct Boot {
    live: bool,
    live_via: &'static str,
    settings: Settings,
    wallet: Option<WalletGuard>,
    yellowstone_token: Option<String>,
    discord_token: Option<String>,
    /// Held for the whole process lifetime: one writer per state dir.
    _state_lock: cli::StateDirLock,
}

/// Secret-bearing environment variables scrubbed in the sync boot
/// phase (AUD-015) once Settings/captured values own the data: RPC
/// URLs carry `?api-key=`/path keys. Nothing re-reads them after boot;
/// later children (upgrade checks) inherit a clean env.
/// `_FILE` variants are NOT here (paths, not secrets);
/// `SOLANA_PRIVATE_KEY` is scrubbed by its own loader
/// (`WalletGuard::load_from_env`).
const SECRET_ENV_VARS: &[&str] = &[
    "INFERENCE_API_KEY",
    "MAINNET_RPC_URL",
    "MAINNET_WSS_RPC_URL",
    "MAINNET_RPC_URLS_FALLBACK",
    alert::DISCORD_BOT_TOKEN_ENV,
    listener::YELLOWSTONE_TOKEN_ENV,
];

/// Remove every [`SECRET_ENV_VARS`] entry. SAFETY: `env::remove_var`
/// mutates the process-global environment — call before any thread
/// exists (the sync boot phase owns that ordering).
fn scrub_secret_env() {
    for name in SECRET_ENV_VARS {
        unsafe { std::env::remove_var(name) };
    }
}

/// Effective live mode: `--live` or the exact `IS_LIVE=true` value.
/// Pure so the pid-record identity is testable (AUD-026).
fn live_mode_from(live_arg: bool, is_live: Option<&str>) -> bool {
    live_arg || is_live == Some("true")
}

/// Paper-ledger commission from the typed fee (AUD-025): an invalid
/// raw value contributes 0 bps, never an impossible 100%/side fee.
fn paper_fee_bps(settings: &Settings) -> f64 {
    settings.fee().map_or(0.0, |bps| f64::from(bps.get()))
}

/// Synchronous boot phase, then the async pipeline. Every environment
/// read-and-scrub, the config load, and the wallet load happen here —
/// before the Tokio runtime spawns worker threads (env is process-
/// global and not thread-safe; `remove_var` after thread spawn is UB
/// territory under Rust 2024) and before any state file is read (two
/// processes must never share a state dir). Returns normally on a
/// clean shutdown so destructors run (the journal flushes its queue).
fn run_binary(live_arg: bool) {
    // Must run before TLS: dual providers panic on auto-detect.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let daemon = std::env::var("BOT_DAEMON").is_ok_and(|v| v == "1");
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(!daemon)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,pump_sentry=debug")),
        )
        .init();

    // One process per state dir, before anything else reads state
    // (config, secrets, WAL, day-state). A second `run`/daemon on the
    // same LOG_FILE directory refuses here instead of double-spending
    // the wallet's risk caps against another holder of the same book.
    let (_, log_file) = cli::resolve_paths();
    let state_lock = match cli::StateDirLock::try_acquire(&cli::state_lock_path(&log_file)) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!(
                "refusing to start: {e}\nstate dir {} is already owned by a running pump-sentry (run/start). Two traders must never share a state dir, wallet, WAL, or day-state.",
                cli::state_dir_for_log(&log_file).display()
            );
            std::process::exit(1);
        }
    };
    // Paper unless explicitly live. Only exact `true` enables live;
    // attribute the source (a lingering IS_LIVE=true in the shell is
    // the sharpest mode footgun). Resolved BEFORE the pid record so
    // `status` reports the mode the process actually runs (AUD-026).
    let is_live_env = std::env::var("IS_LIVE").ok();
    let live_mode = live_mode_from(live_arg, is_live_env.as_deref());
    let live_via: &'static str = if live_arg { "--live" } else { "IS_LIVE=true" };
    // Foreground `run` registers itself so `status`/`sell`/`stop` find
    // it exactly like the daemon (the daemon child overwrites the same
    // record with its own pid, same values).
    {
        let (pid_file, _) = cli::resolve_paths();
        cli::register_running(live_mode, &pid_file);
    }
    if !live_mode
        && let Some(v) = is_live_env.as_deref()
        && !v.is_empty()
        && v != "false"
        && v != "0"
        && v != "no"
    {
        tracing::warn!(
            "config: IS_LIVE='{v}' ignored (only the exact value 'true' enables live; paper mode)"
        );
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

    // Capture the geyser token (if any) BEFORE scrubbing: it crosses
    // into the listener as owned data and never re-reads the env.
    let yellowstone_token = std::env::var(listener::YELLOWSTONE_TOKEN_ENV)
        .ok()
        .or_else(|| {
            std::env::var(format!("{}_FILE", listener::YELLOWSTONE_TOKEN_ENV))
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .filter(|s| !s.is_empty());

    // Capture the Discord bot token (if any) BEFORE scrubbing: delivery
    // is a direct HTTPS POST with `Authorization: Bot …`, and the token
    // crosses into the alerter as owned data, never re-read from env.
    let discord_token = alert::capture_bot_token();

    // Scrub endpoint secrets from the process environment now that
    // Settings owns copies: the RPC URLs carry `?api-key=` / path keys,
    // and nothing re-reads them after boot (same rule as the bearer
    // token and the trading keypair — secrets live in guarded memory,
    // not in `/proc` environ or later children). This happens BEFORE
    // any thread exists: `env::remove_var` is process-global and not
    // thread-safe (AUD-015). `_FILE` variables stay: they name
    // paths, they are not secrets.
    scrub_secret_env();

    // Live wallet load happens in the sync phase too: the keypair is
    // scrubbed from the environment inside `load_from_env`, and that
    // mutation must not race a worker thread (AUD-015). A load failure
    // exits before the live banner (fail fast; same message).
    let wallet = if live_mode {
        match WalletGuard::load_from_env() {
            Ok(wallet) => Some(wallet),
            Err(e) => {
                eprintln!("live mode cannot load the wallet: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let boot = Boot {
        live: live_mode,
        live_via,
        settings,
        wallet,
        yellowstone_token,
        discord_token,
        _state_lock: state_lock,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(run_pipeline(boot));
    // Returning drops the runtime and the boot values (journal flush
    // rides the pipeline Arc drop); the process then exits 0. Non-
    // clean paths exit(1) inside `run_pipeline`.
}

/// WAL path next to the log file.
fn live_wal_path() -> std::path::PathBuf {
    let (_, log_file) = cli::resolve_paths();
    cli::state_file_beside_log(&log_file, "live-wal.jsonl")
}

/// Day-loss tally path next to the log file (survives restarts).
fn day_state_path() -> std::path::PathBuf {
    let (_, log_file) = cli::resolve_paths();
    cli::state_file_beside_log(&log_file, day_state::DAY_STATE_FILE)
}

/// Boot replay of the WAL (live only): restores stranded custody.
async fn replay_live_wal(
    pipeline: &Arc<Pipeline<RugCheckClient, CurveTicker, LiveTrader, RpcScreener, Alerter>>,
    ticker: &CurveTicker,
) {
    use crate::wal;
    let path = live_wal_path();
    // An unreadable WAL is not an empty WAL: custody (bags, pending
    // buys, resend guard) would come back UNKNOWN, so live must not
    // start blind — refuse, loudly, and let the operator fix the file.
    let mut replay = match wal::replay_file(&path) {
        Ok(replay) => replay,
        Err(e) => {
            eprintln!(
                "cannot read live WAL {}: {e}\nlive custody is unknown without it (bags, pending buys, sell guards) — refusing to run live.\nFix permissions/ownership on that file, then retry.",
                path.display()
            );
            std::process::exit(1);
        }
    };
    if replay.pending.is_empty()
        && replay.positions.is_empty()
        && replay.manual.is_empty()
        && replay.sell_pending.is_empty()
    {
        // Fresh WAL: stamp the writer even with nothing to replay, so
        // versioning evidence exists from the first session onward.
        if let Some(trader) = pipeline.exec.live.as_ref() {
            trader.wal_write_meta(settings::now_secs()).await;
        }
        return;
    }
    let mut watched: Vec<(String, String)> = Vec::new();
    let mut n_pending = 0usize;
    let mut n_positions = 0usize;
    let mut n_manual = 0usize;
    let mut n_guarded = 0usize;
    for p in &replay.pending {
        watched.push((p.position.mint.to_string(), p.position.curve.to_string()));
    }
    wal::compact_file(&path, &replay);
    if let Some(trader) = pipeline.exec.live.as_ref() {
        // Compaction rewrote the file; the stamp goes in after it so
        // the fresh session's schema/writer provenance survives.
        trader.wal_write_meta(settings::now_secs()).await;
        // Seed before the pending move below (borrow discipline).
        let recent = replay.recent_sell_pending(settings::now_secs());
        // AUD-009: import what the rechecker can own; a second pending
        // for the same mint is held out for a visible parked claim —
        // either bundle may have landed, so silent loss is not an
        // option. `played_pending` is not otherwise used after this.
        let mut played_pending = std::mem::take(&mut replay.pending);
        let (imported, skipped) = trader.import_pending(std::mem::take(&mut played_pending));
        n_pending = imported;
        // Park the skipped pendings: same mint the rechecker owns, but
        // a different bundle — real exposure if that bundle landed.
        {
            let mut guard = pipeline.exec.live_manual.lock().await;
            for item in &skipped {
                watched.push((
                    item.position.mint.to_string(),
                    item.position.curve.to_string(),
                ));
                let key = item.position.mint.to_string();
                let key = if guard.contains_key(key.as_str()) {
                    format!("{key}#wal-dup-{n_manual}")
                } else {
                    key
                };
                guard.insert(key, item.position.clone());
                n_manual += 1;
                warn!(mint = %item.position.mint, bundle = %item.bundle_id,
                    "WAL replay: duplicate pending parked as manual (custody check: explorer lookup by bundle)");
            }
        }
        // Reseed the resend guard from recent unsettled sell traces:
        // a crash between dispatch and confirm leaves the bundle
        // inflight but the in-memory guard empty. Only engine-pending
        // traces are kept; terminal ones release on first check.
        n_guarded = trader.seed_sell_guard(recent).await;
    }
    {
        let mut guard = pipeline.exec.live_positions.lock().await;
        for pos in replay.positions {
            watched.push((pos.mint.to_string(), pos.curve.to_string()));
            let key = pos.mint.to_string();
            if guard.contains_key(key.as_str()) {
                let key = format!("{key}#wal-{n_positions}");
                guard.insert(key, pos);
            } else {
                guard.insert(key, pos);
            }
            n_positions += 1;
        }
    }
    {
        let mut guard = pipeline.exec.live_manual.lock().await;
        for item in replay.manual {
            let pos = item.position;
            watched.push((pos.mint.to_string(), pos.curve.to_string()));
            warn!(mint = %pos.mint, note = %item.note, "WAL replay: manual bag needs human exit (lookup above on explorer)");
            let key = pos.mint.to_string();
            if guard.contains_key(key.as_str()) {
                let key = format!("{key}#wal-{n_manual}");
                guard.insert(key, pos);
            } else {
                guard.insert(key, pos);
            }
            n_manual += 1;
        }
    }
    // Re-watch before loops start. Failures keep bags tracked.
    // Chain-side check first: WAL amounts are claims, ATA balances
    // are custody.
    let custody = reconcile_boot_custody(pipeline).await;
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
        guarded = n_guarded,
        verified = custody.verified,
        unbacked = custody.unbacked,
        handed_off = custody.handed_off,
        excess = custody.excess,
        unreadable = custody.unreadable,
        awaiting_confirm = custody.awaiting_confirm,
        unwatched,
        file = %path.display(),
        "WAL replay: recovered live custody from unclean shutdown; MANUAL REVIEW REQUIRED (delete the file to clear resolved bags)"
    );
    // AUD-021: resolve the migration now for recovered bags. Their curve
    // may already have graduated and stopped ticking, so waiting for a
    // complete tick can strand the only automated exit. Bounded per bag;
    // unresolved bags retry on the supervision recheck.
    let unfed = pipeline.refresh_recovered_bag_feeds().await;
    if unfed > 0 {
        warn!(
            bags = unfed,
            "WAL replay: recovered live bag(s) have no pool exit feed yet; migration hunt retries on the supervision recheck (manual `sell <mint>` is always available)"
        );
    }
}

/// Replayed custody claims for one mint: the tracked copy plus every
/// parked manual note, summed (the ATA is per mint, not per bag).
#[derive(Default)]
struct MintClaims {
    tracked: u64,
    manual: u64,
    /// Map keys carrying the tracked claim; a suppression removes them.
    tracked_keys: Vec<String>,
}

/// Verdict of the boot chain check for one mint (pure).
#[derive(Debug, PartialEq, Eq)]
enum BootCustody {
    /// The ATA backs the claims exactly (one bag or several): keep.
    Keep,
    /// The ATA holds more than claimed: every claim is backed, keep
    /// and warn (the excess is unexplained: airdrop, external send).
    Excess,
    /// The ATA holds less than claimed: the tracked copy's size is
    /// unproven — suppress it (never auto-exit an unverified size).
    /// `handoff` is the one manual note to write instead: the CHAIN
    /// amount, and only while no note covers the mint yet and the
    /// chain still holds something (nothing to exit otherwise).
    SuppressTracked { handoff: Option<u64> },
}

/// Boot rule (pure). Claims are compared per mint, never per bag: two
/// legit bags of the same mint (parked + re-bought) sum to the ATA and
/// stay, while a parked note replayed next to a resurrected tracked
/// copy of the SAME bag exceeds it and suppresses the copy. Repeated
/// boots must never stack claims: the hand-off note is written once
/// and the WAL lines stay untouched.
fn boot_custody(tracked: u64, manual: u64, chain: u64) -> BootCustody {
    match tracked.saturating_add(manual).cmp(&chain) {
        std::cmp::Ordering::Equal => BootCustody::Keep,
        std::cmp::Ordering::Less => BootCustody::Excess,
        std::cmp::Ordering::Greater => BootCustody::SuppressTracked {
            handoff: (manual == 0 && chain > 0).then_some(chain),
        },
    }
}

/// Boot chain check counters (at most one mint each).
#[derive(Default)]
struct BootCustodyCounts {
    verified: usize,
    unbacked: usize,
    handed_off: usize,
    excess: usize,
    unreadable: usize,
    awaiting_confirm: usize,
}

/// Boot chain check (live): the replayed WAL holds the claim, the ATA
/// balance is custody. Per mint, tracked + parked amounts are summed
/// and read against the chain ONCE:
/// - claims match: keep both;
/// - claims exceed: suppress the tracked copy in memory (no auto-exit
///   at an unverified size) and hand off one manual note at the chain
///   amount while no note covers the mint yet. The WAL lines stay
///   untouched — the file is the claim of record and suppression
///   substitutes nothing for it, so it is idempotent across boots;
/// - claims fall short: keep (every claim is backed) and warn.
///
/// Mints with an unconfirmed buy are skipped (the late-confirm recheck
/// still owns their claim). An unreadable balance warns and keeps the
/// claims tracked (dispatch preflights still fail closed).
/// Mid-run custody recheck (live mode, every `CUSTODY_RECHECK_SECS`):
/// the same claims-vs-chain classifier as the boot reconcile, minus
/// the WAL surgery. Read-only — a mismatch pages the operator and
/// keeps the claims; custody is never auto-corrected or auto-exited
/// here (the per-dispatch preflights own dispatch, the boot reconcile
/// owns boot, this owns drift between them). Returns the mints whose
/// chain balance falls short of the claims: the supervisor latches
/// those as an entry pause until a later recheck passes (recovery is
/// a passing re-check, never elapsed time).
async fn custody_recheck(
    pipeline: &Arc<Pipeline<RugCheckClient, CurveTicker, LiveTrader, RpcScreener, Alerter>>,
) -> Vec<String> {
    let Some(trader) = pipeline.exec.live.as_ref() else {
        return Vec::new();
    };
    let mut short = Vec::new();
    // Snapshot per mint under the locks; read the chain without them.
    let mut claims: BTreeMap<Pubkey, MintClaims> = BTreeMap::new();
    for pos in pipeline.exec.live_positions.lock().await.values() {
        let claim = claims.entry(pos.mint).or_default();
        claim.tracked = claim.tracked.saturating_add(pos.amount_tokens);
    }
    for pos in pipeline.exec.live_manual.lock().await.values() {
        let claim = claims.entry(pos.mint).or_default();
        claim.manual = claim.manual.saturating_add(pos.amount_tokens);
    }
    for (mint, claim) in claims {
        if trader.has_pending_for(&mint.to_string()) {
            continue; // late-confirm recheck owns the pending claim
        }
        let chain = match trader.observe_token_balance(&mint).await {
            Ok(chain) => chain.unwrap_or(0),
            Err(e) => {
                warn!(%mint, error = %ids::strip_error_url(&e.to_string()),
                    "custody recheck: balance unreadable; claims kept (preflights still fail closed)");
                continue;
            }
        };
        match boot_custody(claim.tracked, claim.manual, chain) {
            BootCustody::Keep => {}
            BootCustody::Excess => {
                warn!(%mint, tracked = claim.tracked, manual = claim.manual, chain,
                    "custody recheck: chain holds MORE than claims (unexplained excess — re-buy while parked? transfer in?); verify on explorer");
            }
            BootCustody::SuppressTracked { .. } => {
                warn!(%mint, tracked = claim.tracked, manual = claim.manual, chain,
                    "custody recheck: chain holds LESS than claims (tokens moved out, or a stale/bankrupt RPC answered); NO auto-exit on unverified size — inspect before trusting the book");
                short.push(mint.to_string());
            }
        }
    }
    short
}

async fn reconcile_boot_custody(
    pipeline: &Arc<Pipeline<RugCheckClient, CurveTicker, LiveTrader, RpcScreener, Alerter>>,
) -> BootCustodyCounts {
    let Some(trader) = pipeline.exec.live.as_ref() else {
        return BootCustodyCounts::default();
    };
    // Snapshot per mint under the locks; read the chain without them.
    let mut claims: BTreeMap<Pubkey, MintClaims> = BTreeMap::new();
    for (key, pos) in pipeline.exec.live_positions.lock().await.iter() {
        let claim = claims.entry(pos.mint).or_default();
        claim.tracked = claim.tracked.saturating_add(pos.amount_tokens);
        claim.tracked_keys.push(key.clone());
    }
    for pos in pipeline.exec.live_manual.lock().await.values() {
        let claim = claims.entry(pos.mint).or_default();
        claim.manual = claim.manual.saturating_add(pos.amount_tokens);
    }
    let mut counts = BootCustodyCounts::default();
    for (mint, claim) in claims {
        if trader.has_pending_for(&mint.to_string()) {
            counts.awaiting_confirm += 1;
            continue;
        }
        let chain = match trader.observe_token_balance(&mint).await {
            Ok(chain) => chain.unwrap_or(0),
            Err(e) => {
                counts.unreadable += 1;
                warn!(%mint, tracked = claim.tracked, manual = claim.manual,
                    error = %ids::strip_error_url(&e.to_string()),
                    "boot reconcile: ATA balance unreadable; claims kept (preflights still fail closed)");
                continue;
            }
        };
        match boot_custody(claim.tracked, claim.manual, chain) {
            BootCustody::Keep => counts.verified += 1,
            BootCustody::Excess => {
                counts.excess += 1;
                warn!(%mint, tracked = claim.tracked, manual = claim.manual, chain,
                    "boot reconcile: chain holds MORE than the replayed claims; claims kept (unexplained excess, verify on explorer)");
            }
            BootCustody::SuppressTracked { handoff } => {
                counts.unbacked += 1;
                let mut suppressed: Option<(String, live::LivePosition)> = None;
                for key in &claim.tracked_keys {
                    if let Some(held) = pipeline
                        .exec
                        .live_positions
                        .lock()
                        .await
                        .remove(key.as_str())
                    {
                        suppressed.get_or_insert((key.clone(), held));
                    }
                }
                match suppressed {
                    Some((key, pos)) => {
                        warn!(%mint, tracked = claim.tracked, manual = claim.manual, chain,
                            "boot reconcile: chain holds LESS than the replayed claims; tracked copy suppressed (no auto-exit at an unverified size)");
                        if let Some(amount) = handoff {
                            // Hand off at the CHAIN amount: what can
                            // really be exited, not the distrusted claim.
                            let held = live::LivePosition {
                                amount_tokens: amount,
                                ..pos
                            };
                            warn!(%mint, amount, "boot reconcile: manual note handed off (verify, then exit)");
                            trader.record_manual(&held, "boot-balance-mismatch").await;
                            pipeline.exec.park_manual(key, held).await;
                            counts.handed_off += 1;
                        }
                    }
                    // A parked note only: the operator's own claim.
                    None => {
                        warn!(%mint, manual = claim.manual, chain,
                            "boot reconcile: parked claim exceeds the chain; verify the size before the manual exit");
                    }
                }
            }
        }
    }
    counts
}

/// Heartbeat cadence: the mode/status card only (AUD-003). All
/// execution-quality/custody monitors run on [`SUPERVISION_SECS`].
const HEARTBEAT_SECS: u64 = 6 * 3_600;

/// Supervision cadence: the custody recheck, GiveUp burst monitor,
/// and overdue-exit warning are sampled this often (AUD-003).
const SUPERVISION_SECS: u64 = 60;

/// Mid-run custody drift check cadence in seconds (30 minutes, the
/// documented README cadence; boot reconcile owns boot).
const CUSTODY_RECHECK_SECS: u64 = 1_800;

/// Oldest-open warn threshold: paper timeouts are minutes; an open
/// position older than this reads as a stuck exit (log-only).
const STALE_OPEN_WARN_SECS: u64 = 3_600;

/// GiveUp window surfaced in the heartbeat (trailing giveups card).
const GIVEUP_HEARTBEAT_WINDOW_SECS: u64 = 3_600;

/// Exit-lane reserve (AUD-008): curve ticks, pool ticks, and manual
/// sells dispatch from their own permit budget, so a launch flood
/// (audits, dev screens, forensics burning every launch permit) can
/// never consume the lanes exits dispatch on. The two lanes together
/// still cap concurrent units at [`MAX_IN_FLIGHT`].
pub(crate) const EXIT_LANE_PERMITS: usize = 8;

/// Concurrent-unit bound across both lanes plus the in-flight JoinSet.
pub(crate) const MAX_IN_FLIGHT: usize = 32;

/// Time-injected cadence check for the supervision timers (AUD-003):
/// `true` once at least `interval_secs` have elapsed since `last`.
/// Pure so the supervision cadences are testable without running the
/// event loop.
fn supervision_due(last: u64, now: u64, interval_secs: u64) -> bool {
    now.saturating_sub(last) >= interval_secs
}

/// Foreground pipeline.
/// One status/heartbeat collection (docs/discord-sidecar.md §2): every
/// read is cache-backed or a lock-brief in-memory snapshot — the
/// collector never does an RPC, so the 10-second sidecar cadence adds
/// no endpoint load. Single source for both the 6-hour heartbeat card
/// and the sidecar's `status.json` (never two drift paths).
async fn collect_status(
    pipeline: &Arc<Pipeline<RugCheckClient, CurveTicker, LiveTrader, RpcScreener, Alerter>>,
    stats_task: &Arc<listener::ListenerStats>,
    pool: &Arc<executor::rpc_pool::RpcPool>,
    live_mode: bool,
    boot_ts: u64,
) -> status::StatusReport {
    let snap = stats_task.snapshot();
    let pending = pipeline.pending_count().await;
    let manual = pipeline.exec.manual_keys().await;
    let diverged = pipeline.exec.diverged_keys().await;
    let pending_buys = pipeline.exec.live_pending_mints();
    // One book read serves the card (cash/equity) and the `/pnl`
    // panel (docs/discord-sidecar.md §2: one collector, never two
    // drift paths). Bounded disclosure: a flooded book stays finite
    // in the file; the true count ships with the sample.
    let book = pipeline.paper_book_snapshot().await;
    let (breaker_halted, day_net, day_damaged) = {
        let mut daily = pipeline.daily.lock().await;
        let halted = daily
            .tripped(pipeline.settings.max_daily_loss_usd, settings::now_secs())
            .await;
        // A poisoned tally is `-inf` (not JSON-finite): publish the
        // damage flag with a finite placeholder instead. Renderers
        // must print UNKNOWN, never the placeholder as a number.
        let raw = daily.day_net();
        let damaged = daily.persistence_halted(settings::now_secs());
        let net = if raw.is_finite() { raw } else { 0.0 };
        (halted, net, damaged)
    };
    let rpc_snap = pool.snapshot();
    let oldest_open_age = pipeline.oldest_open_age_secs(settings::now_secs()).await;
    let giveups_window = pipeline
        .exec
        .giveups_since(GIVEUP_HEARTBEAT_WINDOW_SECS, settings::now_secs())
        .await as u64;
    let mut positions = pipeline.open_position_views().await;
    let positions_total = positions.len();
    let (live_extra, live_bags, paper_locked) = pipeline.gate_limit_inputs().await;
    positions.truncate(status::POSITIONS_MAX_PUBLISHED);
    let watchlist = pipeline.watchlist_view().await;
    status::StatusReport {
        ts: settings::now_secs(),
        report: alert::HeartbeatReport {
            mode: (if live_mode { "live" } else { "paper" }).to_string(),
            uptime_secs: settings::now_secs().saturating_sub(boot_ts),
            logs_seen: snap.logs_seen,
            candidates: snap.candidates,
            parse_ok: snap.parse_ok,
            dropped: snap.dropped_full.saturating_add(snap.dropped_throttled),
            watchlist_pending: pending,
            open_positions: positions_total,
            manual_bags: manual.len(),
            diverged: diverged.len(),
            pending_buys: pending_buys.len(),
            breaker_halted,
            day_net,
            strategy: pipeline.strategy.as_str().to_string(),
            cash_usd: book.cash,
            equity_usd: book.equity,
            data_paused: pipeline.data_degraded(),
            rpc_endpoints: rpc_snap.endpoints,
            rpc_healthy: rpc_snap.healthy,
            rpc_failover_reads: rpc_snap.failover_reads,
            rpc_inconsistent: rpc_snap.inconsistent,
            oldest_open_age_secs: oldest_open_age,
            stream_gaps_shed: snap.slot_gaps_shed,
            stream_pages_capped: snap.backfill_pages_capped,
            giveups_window,
        },
        pnl: book.pnl,
        positions,
        manual_mints: manual,
        diverged_mints: diverged,
        pending_buy_mints: pending_buys,
        watchlist,
        heat_live_extra: live_extra,
        exposure_live_bags: live_bags,
        exposure_paper_locked: paper_locked,
        day_tally_damaged: day_damaged,
    }
}

async fn run_pipeline(boot: Boot) {
    let Boot {
        live: live_mode,
        live_via,
        settings,
        mut wallet,
        yellowstone_token,
        discord_token,
        // Held until this function returns: this process owns the
        // state dir for its whole lifetime (AUD-001).
        _state_lock,
    } = boot;

    // SIGHUP reopens the log for logrotate.
    #[cfg(unix)]
    {
        tokio::spawn(async {
            let mut hup =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
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

    if CurveTicker::parse_commitment(&settings.commitment).is_none() {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
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
            "bot {kind} — pid {} mode {} ({}) unix_ts={}",
            std::process::id(),
            if live_mode { "live" } else { "paper" },
            if live_mode { live_via } else { "default" },
            settings::now_secs(),
        );
    }
    if live_mode {
        warn!("================================================================");
        warn!("LIVE MODE via {live_via}: real SOL will move via Jito bundles.");
        warn!("Starting in 5s — Ctrl-C now to abort (no confirmation prompt,");
        warn!("so automation never hangs; the delay is the abort window).");
        warn!("================================================================");
        // Ctrl-C inside the window exits before boot.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            _ = cli::shutdown_signal() => {
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
        // Day-tally writability BEFORE the funding gate (AUD-006): the
        // restart-persistent loss breaker is mandatory in live, and a
        // tally the process cannot write would read back as a fresh
        // day across a restart.
        if let Err(err) = day_state::ensure_writable(&day_state_path()) {
            eprintln!("live mode refuses to start: {err}");
            std::process::exit(1);
        }
        if !(settings.max_daily_loss_usd.is_finite() && settings.max_daily_loss_usd > 0.0) {
            eprintln!("live mode requires MAX_DAILY_LOSS_USD>0 (portfolio circuit breaker)");
            std::process::exit(1);
        }
        if !settings.sim_strict {
            eprintln!(
                "live mode requires SIM_STRICT=true (unproven sims must refuse, not warn-and-proceed)"
            );
            std::process::exit(1);
        }
        if settings.tip_lamports == 0 {
            eprintln!("live mode requires JITO_TIP_LAMPORTS>0 (tipless bundles may never land)");
            std::process::exit(1);
        }
        if settings.commitment == "processed" {
            eprintln!(
                "live mode refuses COMMITMENT=processed (reorgable prints drive exits); use confirmed/finalized"
            );
            std::process::exit(1);
        }
        if settings.quote_usd.is_some() {
            eprintln!(
                "live mode refuses QUOTE_USD override (static fallback is paper-only by construction)"
            );
            std::process::exit(1);
        }
        // AUD-007: an invalid commission must not boot live. Paper
        // settlement fails closed at/above 100% on its own, and the
        // gate's stop widening only consumes the typed value now — but
        // a mistyped FEE_BPS silently changes live risk geometry, so
        // live refuses an untyped (None) fee.
        if settings.fee().is_none() {
            eprintln!(
                "live mode refuses FEE_BPS={} (must be a finite, non-negative bps value below 100%)",
                settings.fee_bps
            );
            std::process::exit(1);
        }
        if settings.inference_api_key.is_some()
            && settings::is_plaintext_remote(&settings.inference_endpoint)
        {
            eprintln!(
                "live mode refuses INFERENCE_API_KEY over plain-http remote INFERENCE_ENDPOINT (bearer travels in cleartext); use https or localhost"
            );
            std::process::exit(1);
        }
        // AUD-016: endpoint trust — remote cleartext endpoints and
        // cross-cluster fallbacks never back live reads/dispatch (the
        // override, set + logged, downgrades a flagged fallback).
        let (fatals, _) = settings.endpoint_trust(
            std::env::var("ALLOW_FALLBACK_CLUSTER_MISMATCH").is_ok_and(|v| v == "1" || v == "true"),
        );
        for fatal in &fatals {
            eprintln!("live mode refuses to start: {fatal}");
        }
        if !fatals.is_empty() {
            std::process::exit(1);
        }
        // AUD-005: live never silently boots on the public RPC default —
        // the project documents it dropping candidates and truncating
        // quorum reads. Refuse unless the operator explicitly opts in.
        let public_rpc_optin =
            std::env::var("ALLOW_PUBLIC_RPC_LIVE").is_ok_and(|v| v == "1" || v == "true");
        if settings.on_public_rpc_default() && !public_rpc_optin {
            eprintln!(
                "live mode refuses the public RPC default ({}): documented unfit for live reads (drops candidates, truncates quorum, rate limits). Set MAINNET_RPC_URL to a paid endpoint, or ALLOW_PUBLIC_RPC_LIVE=1 to override explicitly",
                ids::redact_endpoint(&settings.http_url)
            );
            std::process::exit(1);
        }
        if settings.on_public_rpc_default() {
            warn!(
                "ALLOW_PUBLIC_RPC_LIVE: live mode boots on the public RPC default; unreliable reads (missed candidates, truncated quorum, rate limits) are operator-acknowledged"
            );
        }
        if settings.priority_fee_micro_lamports == 0 {
            warn!(
                "config: live without PRIORITY_FEE_MICRO_LAMPORTS — landing odds drop under congestion"
            );
        }
    }

    let commitment = CurveTicker::parse_commitment(&settings.commitment).unwrap_or_else(|| {
        eprintln!("bad COMMITMENT: {}", settings.commitment);
        std::process::exit(1);
    });
    // The shared pool constructors don't validate (single pool, five
    // consumers): refuse a malformed RPC URL here instead of degrading
    // silently into failed reads mid-run.
    match url::Url::parse(&settings.http_url) {
        Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
        _ => {
            eprintln!(
                "bad MAINNET_RPC_URL: {}",
                ids::redact_endpoint(&settings.http_url)
            );
            std::process::exit(1);
        }
    }
    // One RPC connection pool per process: ticker, screener, vaults,
    // executor, and the WS backfill/fetch share this instead of opening
    // five uncoordinated pools (rate-limit pressure + retry storms).
    // The pool wraps the read failover INSIDE the one shared client —
    // primary first, next healthy endpoint only after an attempt fails
    // (reads only; dispatch stays Jito-only and single-path).
    let pool = executor::rpc_pool::RpcPool::build({
        let mut urls = vec![settings.http_url.clone()];
        urls.extend(settings.fallback_rpc_urls.iter().cloned());
        urls
    });
    pool.spawn_sampler();
    let shared_rpc = pool.client(commitment);
    // One SOL/USD feed for pricing plus sizing.
    let sol_feed = curve::SolUsdFeed::with_defaults();
    let sol_handle = sol_feed.clone().spawn();
    let ticker = CurveTicker::with_rpc(
        Arc::clone(&shared_rpc),
        Duration::from_millis(settings.tick_interval_ms),
        Arc::clone(&sol_feed),
    );

    let (mut rx, stats, listener_handle) = if !settings.yellowstone_endpoint.is_empty() {
        // Geyser replaces WS ingestion (never both): full transactions
        // arrive inline. REST stays for ticks/screens/sim. The x-token
        // is env-only (secret, never the config file, never logged);
        // `_FILE` avoids exporting it into shell history. The token
        // was captured and scrubbed in the sync boot phase (AUD-015)
        // and crosses here as owned data.
        let token = yellowstone_token;
        match listener::GeyserSubscriber::subscribe_with_rpc(
            ListenerConfig {
                ws_url: settings.ws_url.clone(),
                http_url: settings.http_url.clone(),
                geyser_url: settings.yellowstone_endpoint.clone(),
                commitment: settings.commitment.clone(),
                fetch_concurrency: settings.fetch_concurrency,
                backfill_sig_limit: settings.backfill_sig_limit,
                reconnect_backoff_max_ms: settings.reconnect_backoff_max_ms,
            },
            token,
            Some(Arc::clone(&shared_rpc)),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "failed to subscribe (geyser): {}",
                    ids::redact_urls_in_text(&e.to_string())
                );
                std::process::exit(1);
            }
        }
    } else {
        match PumpFunListener::subscribe_with_rpc(
            ListenerConfig {
                ws_url: settings.ws_url.clone(),
                http_url: settings.http_url.clone(),
                geyser_url: String::new(),
                commitment: settings.commitment.clone(),
                fetch_concurrency: settings.fetch_concurrency,
                backfill_sig_limit: settings.backfill_sig_limit,
                reconnect_backoff_max_ms: settings.reconnect_backoff_max_ms,
            },
            Some(Arc::clone(&shared_rpc)),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "failed to subscribe: {}",
                    ids::redact_urls_in_text(&e.to_string())
                );
                std::process::exit(1);
            }
        }
    };
    // The token lives in guarded memory only from the sync boot scrub
    // onward (AUD-015); nothing re-reads the environment here.

    let mut engine_builder = RiskEngine::builder()
        .endpoint(&settings.inference_endpoint)
        .model(&settings.inference_model)
        .timeout(Duration::from_millis(settings.inference_timeout_ms))
        .headers(settings.inference_headers.clone());
    if let Some(key) = &settings.inference_api_key {
        engine_builder = engine_builder.api_key(key.as_str());
    }
    let engine = match engine_builder.build() {
        Ok(engine) => Some(engine),
        Err(e) => {
            warn!(error = %ids::strip_error_url(&e.to_string()), "bad inference endpoint; heuristic-only mode");
            None
        }
    };

    let start_balance = settings.start_balance;
    // AUD-025: the paper ledger consumes the same typed commission as
    // settlement and stop widening. An invalid `FEE_BPS` contributes
    // no commission (the gate already refuses to widen stops with it),
    // instead of booking an impossible 100%/side fee into the book.
    let fee_bps = paper_fee_bps(&settings);

    // Live stack (fatal on misconfig).
    let live = if live_mode {
        // Hot-wallet ceiling in lamports (`0` = off): buys refuse while
        // the wallet holds more, so a leaked key or an un-swept wallet
        // can only lose what the operator allows to sit there. Exits
        // never consult it. The wallet was loaded (and the key env
        // scrubbed) in the sync boot phase (AUD-015).
        let hot_cap = settings.hot_wallet_cap_lamports();
        let wallet = match wallet.take() {
            Some(wallet) => wallet.with_hot_wallet_cap(hot_cap),
            None => {
                eprintln!("live mode requires a loaded wallet");
                std::process::exit(1);
            }
        };
        info!(payer = %wallet.pubkey(), "live wallet loaded");
        if hot_cap > 0 {
            info!(
                cap_sol = settings.hot_wallet_max_sol,
                "hot-wallet cap armed: entries refuse while the wallet holds more SOL (sweep the excess)"
            );
        }
        if settings.sim_strict {
            info!("strict preflight on: unproven simulations refuse to dispatch");
        }
        let jito = match JitoClient::new(&settings.jito_endpoint) {
            Ok(jito) => jito,
            Err(e) => {
                eprintln!(
                    "bad JITO_ENDPOINT '{}': {e}",
                    ids::redact_endpoint(&settings.jito_endpoint)
                );
                std::process::exit(1);
            }
        };
        let executor = SwapExecutor::with_rpc(Arc::clone(&shared_rpc), wallet)
            .with_jito(jito.clone())
            .with_compute_budget(
                settings.compute_unit_limit,
                settings.priority_fee_micro_lamports,
            )
            .with_strict_sim(settings.sim_strict);
        // Background tip refresher (AUD-011): the tip-list cache lives so
        // dispatch never waits on `getTipAccounts` — the refresher owns
        // the fetch, and dispatch reads the cache (fallback: official
        // constants). A half-TTL cadence repairs past a blip faster than
        // the 15-minute TTL.
        let tip_refresher = jito.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                executor::jito::TIP_ACCOUNTS_TTL_SECS / 2,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately: refresh once at boot
            loop {
                tip_refresher.refresh_tips().await;
                ticker.tick().await;
            }
        });
        // Background tip-floor refresher (AUD-006): same contract for
        // the landed-tip floor — `tip_floor_lamports` is a cache read on
        // the dispatch path; a stalled oracle costs freshness, never
        // dispatch latency. Half-TTL (30s) cadence.
        let floor_refresher = jito.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(Duration::from_secs(executor::jito::TIP_FLOOR_TTL_SECS / 2));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately: refresh once at boot
            loop {
                floor_refresher.refresh_tip_floor().await;
                ticker.tick().await;
            }
        });
        // Funding gate (read-only): native shortfall fails closed.
        // Principal needs a SOL price: one-shot fetch, skipped loudly
        // when unreadable (dispatch legs still enforced; per-trade
        // solvency covers runtime).
        {
            let principal = match sol_feed.fetch_once().await {
                Ok(sol_usd) => Some(FundingPrincipal {
                    notional_usd: settings.notional_usd,
                    slippage_bps: settings.slippage_bps,
                    sol_usd,
                }),
                Err(e) => {
                    warn!(error = %ids::strip_error_url(&e.to_string()), "SOL price unreadable at funding gate; principal unchecked (dispatch legs still enforced)");
                    None
                }
            };
            let report: LiveFundingReport = executor
                .live_funding_report(
                    settings.tip_lamports,
                    settings.max_open_positions,
                    principal,
                )
                .await;
            match report.native_ok() {
                Some(true) => {
                    info!(payer = %report.payer, balance = report.native_balance, required = report.native_required, max_open = settings.max_open_positions, "live native SOL covers full-book principal + dispatch + reserve")
                }
                Some(false) => {
                    eprintln!(
                        "live wallet {} short native SOL: have {} lamports, need {} for {} positions (principal + dispatch + ATA rent + 0.05 reserve); fund before live",
                        report.payer,
                        report.native_balance.unwrap_or(0),
                        report.native_required,
                        settings.max_open_positions
                    );
                    std::process::exit(1);
                }
                None => warn!("live native balance unreadable; per-trade solvency decides"),
            }
            match report.wsol_funded() {
                Some(true) => {
                    info!(ata = %report.wsol_ata, held = report.wsol_balance, "live WSOL float covers token-paired quotes")
                }
                _ => {
                    warn!(
                        "no WSOL float at {} — token-paired quotes need one (SOL-pair buys pay native SOL and are unaffected).",
                        report.wsol_ata
                    );
                }
            }
            if hot_cap > 0
                && let Some(balance) = report.native_balance
                && balance > hot_cap
            {
                warn!(
                    balance_sol = balance as f64 / ids::LAMPORTS_PER_SOL as f64,
                    cap_sol = settings.hot_wallet_max_sol,
                    "hot wallet OVER its cap: every buy preflight refuses until the excess is swept (exits and `sell <mint>` keep working)"
                );
            }
        }
        Some(
            LiveTrader::new(executor, settings.slippage_bps, settings.tip_lamports)
                .with_dispatch_fees(
                    settings.compute_unit_limit,
                    settings.priority_fee_micro_lamports,
                )
                .with_adaptive_slippage(
                    settings.slippage_vol_window_secs,
                    settings.slippage_vol_extra_cap_bps,
                )
                .with_wal(live_wal_path()),
        )
    } else {
        None
    };

    let screener = RpcScreener::with_rpc(
        Arc::clone(&shared_rpc),
        commitment,
        DevScreenConfig {
            min_age_secs: settings.dev_min_age_secs,
            max_prior_launches: settings.dev_max_prior_launches,
            sample_txs: settings.dev_screen_sample_txs,
            sig_limit: settings
                .dev_sig_limit
                .max(settings.dev_screen_sample_txs)
                .clamp(1, 1000),
        },
    );

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

    let alerts = settings.discord_alerts.then(|| {
        let mut alerter = Alerter::new();
        match &settings.discord_channel {
            Some(channel) => {
                info!(%channel, "discord alerts routed to channel");
                alerter = alerter.with_channel(channel);
            }
            None => info!("discord alerts lack a channel id; sends stay disabled"),
        }
        match discord_token {
            Some(token) => alerter.with_token(&token),
            None => {
                info!("discord alerts lack a bot token; sends stay disabled");
                alerter
            }
        }
    });
    if alerts.is_none() {
        info!("discord alerts disabled (DISCORD_ALERTS=false)");
    }

    let vault_ticker = VaultTicker::with_rpc(
        Arc::clone(&shared_rpc),
        Duration::from_millis(settings.vault_tick_interval_ms),
        Arc::clone(&sol_feed),
    );

    let auditor = match RugCheckClient::new() {
        Ok(auditor) => auditor,
        Err(e) => {
            eprintln!("bad auditor config: {e}");
            std::process::exit(1);
        }
    };

    // Decision journal (analytics-only, fail-soft): opened even in
    // paper mode so replay and paper/live metric separation work from
    // day one. `with_journal` before any tick runs.
    let jrn = journal::JournalSink::maybe_open(&journal::default_journal_path());
    if let Some(ref j) = jrn {
        // Snapshot BEFORE the settings move into the pipeline.
        j.boot(
            settings::now_secs(),
            &settings,
            if live_mode { "live" } else { "paper" },
        );
    }

    // Supervisor's read view of the SOL feed (the owned clone moves
    // into the pipeline; staleness polling needs its own handle).
    let sol_feed_view = Arc::clone(&sol_feed);
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
        Some(day_state_path()),
    );
    if let Some(j) = jrn {
        pipeline.with_journal(Arc::new(j));
    }

    // Kill switch: the marker file halts entries without a restart.
    // Polled below each gate round; flip the state now so boot tells
    // the truth on the first log line.
    let paused_path = cli::entries_paused_path();
    pipeline.set_entries_paused(cli::marker_present(&paused_path));

    // Crash replay (live only).
    if live_mode {
        replay_live_wal(&pipeline, &ticker).await;
    }

    // Pending-buy rechecks (AUD-013): a dedicated supervisor task, NOT
    // the tick path — a recheck round runs a 60-second confirm deadline
    // per entry, and a tick that waits behind it can starve its own
    // exit for a minute. The existing `recheck_busy` still bounds
    // rounds to one, `maybe_unwatch` pins pending mints to the
    // watchlist, and WAL replay recovers anything lost at shutdown.
    // The task is stopped between rounds at shutdown (a trailing round
    // is joined so confirm outcomes are never dropped mid-handoff).
    let mut pending_recheck_stop: Option<tokio::sync::oneshot::Sender<()>> = None;
    let mut pending_recheck_handle = if live_mode {
        /// Pending recheck cadence: at the tick cadence the old inline
        /// path effectively ran at; `recheck_busy` keeps it to one round.
        const PENDING_RECHECK_SECS: u64 = 2;
        let recheck_pipeline = Arc::clone(&pipeline);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(PENDING_RECHECK_SECS));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            timer.tick().await; // interval's immediate first tick
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        recheck_pipeline.recheck_live_pending().await;
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });
        pending_recheck_stop = Some(stop_tx);
        Some(handle)
    } else {
        None
    };

    // Ticker loop: poll curves, auto-close.
    let (tick_tx, mut tick_rx) = mpsc::channel::<PriceTick>(1024);
    let tick_handle = tokio::spawn(async move { ticker.run(tick_tx).await });

    // Pool loop: Raydium prices, spike alerts.
    let (pool_tx, mut pool_rx) = mpsc::channel::<PoolTick>(1024);
    let pool_handle = tokio::spawn(async move { vault_ticker.run(pool_tx).await });

    // Gate loop: overlaps skip, never double-confirm.
    let mut gate_timer = tokio::time::interval(gate_interval);
    gate_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Heartbeat cadence (mode/status card only; the supervision
    // monitors live on `SUPERVISION_SECS`, AUD-003).
    let boot_ts = settings::now_secs();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Supervision cadence (AUD-003): the short timer that runs the
    // execution-quality/custody monitors — the custody recheck at its
    // documented 30-minute cadence, the GiveUp burst monitor on its own
    // configured window, and the overdue-exit warning. Sampling these
    // only at the 6-hour heartbeat made every window at least six
    // hours wide (a custody short escaped detection for hours; a
    // GiveUp burst was visible only if it fell just before a heartbeat).
    let mut supervision = tokio::time::interval(Duration::from_secs(SUPERVISION_SECS));
    supervision.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // In-flight tasks never cancel mid-commit. Bounded; drops loudly.
    let mut in_flight = tokio::task::JoinSet::new();
    /// Manual-sell drain cadence in gate rounds.
    const MANUAL_SELL_DRAIN_EVERY: u64 = 15;
    let mut gate_rounds: u64 = 0;
    // Ingestion-ban edge mirror: the listener trips the latch, this
    // loop owns the Discord card (the listener owns no Alerter).
    // Polled here — not in the 6h heartbeat — so a fresh ban pages
    // within seconds instead of next heartbeat.
    let mut ingestion_banned_seen = false;
    // Data-degrade card mirror (rising-edge card, same policy).
    let mut data_degraded_seen = false;
    // Rising-edge mirrors for the supervision monitors (AUD-003): a
    // warn fires when the condition starts and clears when it ends —
    // a 60-second sampler must not spam every tick.
    let mut overdue_warned = false;
    let mut giveup_burst_seen = false;
    // Listener-stall mirror: last logs_seen advance (both modes; the
    // ban latch is geyser-only, silence covers WS too).
    let mut last_logs_seen = 0u64;
    let mut last_logs_advance = boot_ts;
    // Custody-recheck stamp (live mode): mid-run drift check cadence.
    let mut last_custody_check = settings::now_secs();
    // Latched ledger shorts (live mode): mints whose chain balance
    // fell short of the claims at the last recheck. Refreshed only by
    // a passing recheck — the pause outlives the timer by design.
    let mut ledger_shorts: Vec<String> = Vec::new();
    /// SOL-feed boot grace: a feed that never priced since boot pauses
    /// entries only after this (a fresh boot without a price yet is
    /// normal startup, not degradation; losing an HAD price is).
    const FEED_NEVER_GRACE_SECS: u64 = 300;
    /// Listener stall: no log seen for this long means the stream is
    /// dead in both modes (geyser ban has its own latch; WS has none —
    /// pump.fun launches are seconds apart, so 30 silent minutes is
    /// never a quiet chain; a chain halt also correctly pauses entries).
    const LISTENER_STALL_SECS: u64 = 1_800;
    let permits = Arc::new(tokio::sync::Semaphore::new(
        MAX_IN_FLIGHT - EXIT_LANE_PERMITS,
    ));
    let exit_permits = Arc::new(tokio::sync::Semaphore::new(EXIT_LANE_PERMITS));
    // Saturation drops are latency signal: an exit delayed a tick is
    // slippage paper never pays, so they ride the heartbeat.
    let tick_drops = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let pool_drops = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stats_task = Arc::clone(&stats);
    // Sidecar status publisher (docs/discord-sidecar.md §2): fresh
    // readings on disk every few seconds so `pump-sentry discord`
    // answers slash commands without touching this loop. Best-effort:
    // a failed write only logs — the sidecar serves the last complete
    // file, and trading is unaffected by construction.
    {
        let pipeline = Arc::clone(&pipeline);
        let stats = Arc::clone(&stats_task);
        let pool = Arc::clone(&pool);
        let path = status::status_path();
        tokio::spawn(async move {
            let mut writer =
                tokio::time::interval(Duration::from_secs(status::STATUS_WRITE_INTERVAL_SECS));
            loop {
                writer.tick().await;
                let report = collect_status(&pipeline, &stats, &pool, live_mode, boot_ts).await;
                // The staged tmp+fsync+rename never waits on a runtime
                // worker: this task runs every 10 s regardless of
                // trading load.
                let write_path = path.clone();
                let written =
                    tokio::task::spawn_blocking(move || status::write(&write_path, &report))
                        .await
                        .map_err(|join| {
                            std::io::Error::other(format!("status writer aborted: {join}"))
                        })
                        .and_then(|res| res);
                if let Err(e) = written {
                    warn!(error = %e, "status file write failed; Discord sidecar serves stale state");
                }
            }
        });
    }
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
                        .on_launch(&LaunchRequest {
                            mint: &event.mint.to_string(),
                            curve: &event.bonding_curve.to_string(),
                            assoc_curve: &event.associated_bonding_curve.to_string(),
                            dev: &event.user.to_string(),
                            creator: creator.as_deref(),
                            name: &event.name,
                            symbol: &event.symbol,
                            slot: event.slot,
                        })
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
                let exit_permits = Arc::clone(&exit_permits);
                let tick_drops = Arc::clone(&tick_drops);
                in_flight.spawn(async move {
                    let Ok(_permit) = exit_permits.try_acquire_owned() else {
                        // Tick drops are warn (launches stay debug).
                        tick_drops.fetch_add(1, Ordering::Relaxed);
                        warn!(mint = %tick.mint, "tick dropped: exit lane saturated; exit delayed");
                        return;
                    };
                    pipeline.on_tick(&tick).await;
                });
            }
            pool = pool_rx.recv() => {
                let Some(pool) = pool else { break };
                let pipeline = Arc::clone(&pipeline);
                let exit_permits = Arc::clone(&exit_permits);
                let pool_drops = Arc::clone(&pool_drops);
                in_flight.spawn(async move {
                    let Ok(_permit) = exit_permits.try_acquire_owned() else {
                        pool_drops.fetch_add(1, Ordering::Relaxed);
                        debug!(mint = %pool.mint, "pool tick dropped: exit lane saturated");
                        return;
                    };
                    pipeline.on_pool_tick(&pool).await;
                });
            }
            _ = supervision.tick() => {
                // Custody recheck (live only) at its documented
                // 30-minute cadence, sampled every supervision tick
                // (AUD-003: it was only reachable at the 6-hour
                // heartbeat). A short ledger latches the entry pause
                // until a later recheck passes — recovery is a passing
                // re-check, never elapsed time.
                if live_mode
                    && supervision_due(last_custody_check, settings::now_secs(), CUSTODY_RECHECK_SECS)
                {
                    last_custody_check = settings::now_secs();
                    ledger_shorts = custody_recheck(&pipeline).await;
                    // AUD-021: recovered bags with no pool leg get their
                    // migration hunt retried here (a graduated curve can
                    // stop ticking, stranding the only automated exit).
                    // Riding the 30-minute recheck bounds the RPC cost —
                    // one hunt may take its full 15s deadline.
                    let unfed = pipeline.refresh_recovered_bag_feeds().await;
                    if unfed > 0 {
                        warn!(
                            bags = unfed,
                            "live bag(s) still have no pool exit feed; curve ticks or manual `sell <mint>` are the only exits"
                        );
                    }
                }
                // Positions outliving their OWN timeouts read as stuck
                // exits (drained pools, guard holds) — log-only warn
                // per the alert burn-in doctrine (never a card).
                // Overdue (not raw age) respects runner timeout lifts.
                // Rising edge: fire when stuck, clear when unstuck, no
                // per-tick spam.
                let overdue =
                    pipeline.oldest_open_overdue_secs(settings::now_secs()).await;
                if overdue > STALE_OPEN_WARN_SECS {
                    if !overdue_warned {
                        overdue_warned = true;
                        warn!("oldest open position {overdue}s past its timeout (> {STALE_OPEN_WARN_SECS}s overdue): exits stuck? check pool depth / pending rechecks before assuming the book is live");
                    }
                } else {
                    overdue_warned = false;
                }
                // GiveUp burst monitor on its configured trailing
                // window, sampled every supervision tick (AUD-003: a
                // burst was only visible if it fell just before a
                // 6-hour heartbeat). Rising edge, like the other
                // supervision latches.
                if pipeline.settings.alert_giveup_burst_n > 0 {
                    let now = settings::now_secs();
                    let burst = pipeline
                        .exec
                        .giveups_since(pipeline.settings.alert_giveup_window_secs, now)
                        .await;
                    if burst as u64 >= pipeline.settings.alert_giveup_burst_n {
                        if !giveup_burst_seen {
                            giveup_burst_seen = true;
                            warn!("alert: {burst} GiveUps in trailing {}s >= ALERT_GIVEUP_BURST_N={}; systemic dispatch trouble, check RPC/Jito before funding more", pipeline.settings.alert_giveup_window_secs, pipeline.settings.alert_giveup_burst_n);
                        }
                    } else {
                        giveup_burst_seen = false;
                    }
                }
            }
            _ = heartbeat.tick() => {
                // One shared collector with the sidecar status writer:
                // the same fresh readings, never two drift paths.
                let report =
                    collect_status(&pipeline, &stats_task, &pool, live_mode, boot_ts).await;
                let h = report.report;
                let card = Alerter::format_heartbeat(&h);
                // Locals the rest of the arm reads (journal, logs).
                let (cash, equity, breaker_halted, day_net) =
                    (h.cash_usd, h.equity_usd, h.breaker_halted, h.day_net);
                let (pending, open, manual, diverged, pending_buys) = (
                    h.watchlist_pending,
                    h.open_positions,
                    h.manual_bags,
                    h.diverged,
                    h.pending_buys,
                );
                // Custody recheck, the overdue-exit warning, and the
                // GiveUp burst monitor live on the 60-second
                // supervision tick (AUD-003); the heartbeat card
                // keeps its own 6-hour cadence and surfaces their
                // latest state above.
                // Equity-curve record (drawdown / expectation evidence).
                if let Some(j) = pipeline.journal() {
                    if j.is_degraded() {
                        warn!("decision journal is degraded (stopped recording after an IO failure); decisions since then are unplayable — restart the bot to clear (trading unaffected)");
                    } else {
                        j.append(&journal::JournalLine::Equity {
                            ts: settings::now_secs(),
                            cash,
                            equity,
                            open_count: open,
                            day_net,
                            breaker_halted,
                        });
                    }
                }
                info!("heartbeat: logs={} candidates={} watchlist={} open={} manual={} diverged={} pending_buys={} equity={:.2}",
                    h.logs_seen, h.candidates, pending, open, manual, diverged, pending_buys, equity);
                // Stashed and synthetic PnL bypass the breaker by
                // design — but invisible bypasses hide exposure, so
                // both ride the heartbeat every round.
                {
                    let (deferred_n, deferred_total) = pipeline.exec.deferred_stats().await;
                    let synthetic_n = pipeline.exec.synthetic_count().await;
                    let unaccounted_n = pipeline.exec.unaccounted_realized_count().await;
                    if deferred_n > 0 || synthetic_n > 0 || unaccounted_n > 0 {
                        info!("breaker bypass: deferred_pnl_n={deferred_n} deferred_total={deferred_total:+.2} synthetic_n={synthetic_n} unaccounted_realized_n={unaccounted_n} (excluded from day-loss breaker)");
                    }
                    if unaccounted_n > 0 {
                        warn!("{unaccounted_n} settled live exit(s) had no persisted entry cost (legacy WAL): realized PnL is UNACCOUNTED and the day-loss tally under-counts them");
                    }
                }
                // AUD-021: custody with no pool leg has no guaranteed
                // automated exit once its curve stops ticking.
                let unfed = pipeline.bags_without_pool_leg().await.len();
                if unfed > 0 {
                    warn!("{unfed} live bag(s) have no pool exit feed (curve ticks or manual `sell <mint>` are the only exits)");
                }
                let tick_dropped = tick_drops.load(Ordering::Relaxed);
                let pool_dropped = pool_drops.load(Ordering::Relaxed);
                if tick_dropped > 0 || pool_dropped > 0 {
                    warn!("saturation drops: curve_ticks={tick_dropped} pool_ticks={pool_dropped} (delayed exits are slippage paper never pays)");
                }
                // Paper-ledger overdraft (AUD-010): dispatch costs past
                // available cash surface as a warning instead of being
                // absorbed at a zero cash floor.
                let overdraft_usd = pipeline.paper_overdraft_usd().await;
                if overdraft_usd > 0.0 {
                    warn!("paper ledger OVERDRAWN: dispatch costs exceed cash by ${overdraft_usd:.2}; equity carries the shortfall and new Long opens refuse");
                }
                // Adaptive slippage widens live guards/floors with
                // volatility while paper models base slippage only: say
                // so every heartbeat or the gap reads as pure alpha.
                if pipeline.settings.slippage_vol_window_secs > 0
                    && pipeline.settings.slippage_vol_extra_cap_bps > 0
                {
                    info!("adaptive slippage armed (window={}s cap={}bps): live pays volatility add-on, paper does not",
                        pipeline.settings.slippage_vol_window_secs, pipeline.settings.slippage_vol_extra_cap_bps);
                }
                // A ban must never be discoverable only from a weeks-old
                // edge card: re-log every heartbeat while it holds.
                if stats_task.ingestion_banned.load(Ordering::Relaxed) {
                    warn!("ingestion STILL BANNED on geyser: bot is blind, retries hourly; check token/quota or fall back to WS mode");
                }
                // Execution-gap calibration: paper model vs live reality
                // over the retained window. Both legs read negative when
                // live trails; the exit leg is a LOWER BOUND (tick proxy,
                // excl. intra-bundle slip — see `FillGap` docs).
                let (gap_n, gap_avg, gap_total) = pipeline.exec.gap_stats().await;
                if gap_n > 0 {
                    info!("execution gap (lower bound, excl. intra-bundle slip): n={gap_n} avg={gap_avg:+.4} total={gap_total:+.4} USD (paper model vs live settle + tips)");
                }
                let (egap_n, egap_avg, egap_total) = pipeline.exec.entry_gap_stats().await;
                if egap_n > 0 {
                    info!("entry gap (negative = live paid more): n={egap_n} avg={egap_avg:+.4} total={egap_total:+.4} USD (paper quote vs landed fill)");
                }
                // Fill-rate trip input (trailing settled outcomes; thin
                // windows stay silent via the sample floor inside the
                // tracker — unknown reads as unknown, never as healthy).
                if let Some(rate) = pipeline.exec.buy_fill_rate().await {
                    info!("buy fill rate (trailing settled outcomes): {:.0}% (floor {:.0}%)", rate * 100.0, pipeline.settings.min_fill_rate_pct);
                }
                // Execution-gap dimensions: which dispatch path bled, at
                // what ticket shape. Log-only calibration (never cards,
                // never gates) — thin cells stay silent via the sample
                // floor inside the tracker.
                for (venue, n, avg, total) in pipeline.exec.venue_exit_stats().await {
                    info!("exit gap by venue: {venue} n={n} avg={avg:+.4} total={total:+.4} USD");
                }
                for (venue, n, avg, total) in pipeline.exec.venue_entry_stats().await {
                    info!("entry gap by venue: {venue} n={n} avg={avg:+.4} total={total:+.4} USD");
                }
                if let Some((venue, size, liq, cell)) = pipeline.exec.worst_exit_cell().await {
                    info!("worst exit cell: venue={venue} size_bkt={size} liq_bkt={liq} n={} avg={:+.4} total={:+.4} USD",
                        cell.n, cell.avg_gap_usd, cell.total_gap_usd);
                }
                if let Some((venue, size, liq, cell)) = pipeline.exec.worst_entry_cell().await {
                    info!("worst entry cell: venue={venue} size_bkt={size} liq_bkt={liq} n={} avg={:+.4} total={:+.4} USD",
                        cell.n, cell.avg_gap_usd, cell.total_gap_usd);
                }
                {
                    let hours = pipeline.exec.entry_hour_stats().await;
                    let mut worst: Option<(usize, usize, f64, f64)> = None;
                    for (h, (n, avg, total)) in hours.iter().enumerate() {
                        if *n >= live_mirror::MIN_GAP_SAMPLES
                            && worst.is_none_or(|(_, _, wavg, _)| *avg < wavg)
                        {
                            worst = Some((h, *n, *avg, *total));
                        }
                    }
                    if let Some((h, n, avg, total)) = worst {
                        info!("worst entry hour (UTC): {h:02} n={n} avg={avg:+.4} total={total:+.4} USD");
                    }
                }
                // Congestion pricing vs the paper calm-floor: paper legs
                // pay base tip only, live pays up to worst_4x per
                // dispatch under congestion. Read paper edge net of the
                // worst case, not the floor.
                if live_mode
                    && let Some(trader) = pipeline.exec.live.as_ref()
                {
                    let (base, floor, worst) = trader.congestion_snapshot().await;
                    let worst_usd = pipeline
                        .sol_feed
                        .stamped_price()
                        .map(|(s, _)| worst as f64 / ids::LAMPORTS_PER_SOL as f64 * s)
                        .filter(|v| v.is_finite())
                        .map(|v| format!("${v:.2}"))
                        .unwrap_or_else(|| "n/a".to_string());
                    info!("congestion: base_tip={base} floor={floor} worst_4x={worst} lamports/dispatch ({worst_usd} at 4x; paper legs pay base only)");
                }
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
                            // Policy-aware: what entries do on failure is
                            // the operator's call, not a constant.
                            let policy = thresholds.inference_fallback.as_str();
                            warn!("alert: risk-engine fallback rate {pct}% ({fallbacks}/{decisions}) >= ALERT_RISK_FALLBACK_WARN_PCT={}; model may be down, entries run policy '{policy}' on fallback", thresholds.alert_risk_fallback_warn_pct);
                        }
                    }
                    // Confidence view per model: latency percentiles plus
                    // what the fallback actually decided (BUY keeps
                    // volume, PASS protects edge — the split shows which
                    // the fallback is doing, so a volume-protecting
                    // fallback can't hide behind the rate alone).
                    if let Some((p50, p95, p99)) = engine.latency_ms_percentiles() {
                        info!("inference confidence: model={} decisions={decisions} fallbacks={fallbacks} (buy={}, pass={}) latency_ms p50={p50} p95={p95} p99={p99}",
                            engine.model(), engine.fallback_buy_count(), engine.fallback_pass_count());
                    }
                }
                if let Some(alerts) = pipeline.alerts.as_ref() {
                    alerts.alert(card);
                }
            }
            _ = gate_timer.tick() => {
                // Kill switch: entries halt while the marker exists.
                // Polled here (one stat per round) so a flip needs no
                // restart; the edge is logged once.
                pipeline.set_entries_paused(cli::marker_present(&paused_path));
                // Ingestion-ban edges: card on trip (stop hammering,
                // check token/quota, or clear yellowstone_endpoint for
                // WS mode), note on recovery. The latch lives in the
                // listener; this loop owns the cards.
                let banned = stats_task.ingestion_banned.load(Ordering::Relaxed);
                if banned && !ingestion_banned_seen {
                    ingestion_banned_seen = true;
                    warn!("INGESTION BANNED: geyser 429 trip — retries hourly; check token/quota or fall back to WS mode");
                    pipeline.emit("INGESTION BANNED on geyser (consecutive 429s): bot is blind, retries hourly. Check Yellowstone token/quota, or clear yellowstone_endpoint to fall back to WS ingestion.".to_string());
                } else if !banned && ingestion_banned_seen {
                    ingestion_banned_seen = false;
                    warn!("ingestion ban lifted: geyser subscription accepted again");
                    pipeline.emit("Ingestion recovered: geyser subscription accepted again.".to_string());
                }
                // Data-degrade supervisor: entries pause while a critical
                // source is unhealthy — no decisions from stale data.
                // Same choke point as the kill switch (`open_tracked`),
                // separate state; exits keep running either way.
                // Recovery is a passing re-check of every reason below,
                // never elapsed time: nothing here resumes on a timer.
                let mut reasons: Vec<String> = Vec::new();
                if pool.degraded() {
                    reasons.push("rpc pool has no healthy endpoint".to_string());
                }
                // Feed grace: a boot that never priced yet is startup,
                // not degradation; losing a HAD price pauses immediately.
                if sol_feed_view.is_stale()
                    && (sol_feed_view.has_ever_priced()
                        || settings::now_secs().saturating_sub(boot_ts) > FEED_NEVER_GRACE_SECS)
                {
                    reasons.push("SOL/USD feed stale".to_string());
                }
                if banned {
                    reasons.push("listener ingestion banned (blind)".to_string());
                }
                // Stall covers both modes (the ban latch is geyser-only):
                // launches are seconds apart, so a long silence is a
                // dead stream, never a quiet chain.
                {
                    let now = settings::now_secs();
                    let logs = stats_task.logs_seen();
                    if logs != last_logs_seen {
                        last_logs_seen = logs;
                        last_logs_advance = now;
                    } else if now.saturating_sub(last_logs_advance) > LISTENER_STALL_SECS {
                        reasons.push("listener stalled (no logs)".to_string());
                    }
                }
                // Ledger short vs chain (live mode): the latched custody
                // recheck verdict — refreshed only by a passing recheck,
                // so the pause outlives the audit timer by design.
                if !ledger_shorts.is_empty() {
                    reasons.push(format!("ledger short vs chain: {}", ledger_shorts.join(",")));
                }
                // Execution-quality trips (live evidence): a collapsing
                // buy fill rate or a widening paper-vs-live gap pauses
                // entries until the windows themselves read healthy
                // again. Thin windows read as unknown, never as trips.
                if let Some(rate) = pipeline.exec.buy_fill_rate().await {
                    let floor = pipeline.settings.min_fill_rate_pct;
                    if floor.is_finite() && floor > 0.0 && rate * 100.0 < floor {
                        reasons.push(format!(
                            "buy fill rate {:.0}% below floor {:.0}%",
                            rate * 100.0,
                            floor
                        ));
                    }
                }
                {
                    let (_, exit_avg, _) = pipeline.exec.gap_stats().await;
                    let cap = pipeline.settings.max_avg_exit_gap_usd;
                    if cap.is_finite() && cap > 0.0 && exit_avg < -cap {
                        reasons.push(format!(
                            "avg exit gap {exit_avg:+.4} USD worse than -{cap:.4}"
                        ));
                    }
                }
                {
                    let (_, entry_avg, _) = pipeline.exec.entry_gap_stats().await;
                    let cap = pipeline.settings.max_avg_entry_gap_usd;
                    if cap.is_finite() && cap > 0.0 && entry_avg < -cap {
                        reasons.push(format!(
                            "avg entry gap {entry_avg:+.4} USD worse than -{cap:.4}"
                        ));
                    }
                }
                pipeline.set_data_degraded(!reasons.is_empty(), &reasons);
                if !reasons.is_empty() && !data_degraded_seen {
                    data_degraded_seen = true;
                    pipeline.emit(format!(
                        "DATA DEGRADED — entries paused: {} . No decisions from unhealthy data; open-position management keeps running.",
                        reasons.join(", ")
                    ));
                } else if reasons.is_empty() && data_degraded_seen {
                    data_degraded_seen = false;
                    pipeline.emit("Data sources healthy again: entries resume.".to_string());
                }
                // Manual-sell drain every 15th round (AUD-008): each sell
                // dispatches from the exit lane inside the bounded
                // JoinSet — the event loop never awaits a sell, so a
                // slow dispatch+confirm cannot stall ticks/launches.
                gate_rounds = gate_rounds.wrapping_add(1);
                if gate_rounds.is_multiple_of(MANUAL_SELL_DRAIN_EVERY) {
                    let (_, log_file) = cli::resolve_paths();
                    for (mint, path) in cli::pending_sell_requests(&log_file) {
                        // A request whose body does not name its own mint
                        // is stale garbage: drop it. An unreadable file is
                        // transient IO — keep the request for the next
                        // drain instead of swallowing an operator sell.
                        let body = match std::fs::read_to_string(&path) {
                            Ok(body) => body,
                            Err(e) => {
                                warn!(%mint, path = %path.display(), error = %e, "manual sell request unreadable; keeping for next drain");
                                continue;
                            }
                        };
                        if cli::sell_request_body_mint(&body).is_none_or(|named| named != mint) {
                            warn!(%mint, path = %path.display(), "manual sell request body mismatch; dropping request");
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                        let pipeline = Arc::clone(&pipeline);
                        let exit_permits = Arc::clone(&exit_permits);
                        in_flight.spawn(async move {
                            let _permit = exit_permits.acquire_owned().await;
                            let settled = pipeline.manual_sell(&mint).await;
                            if settled {
                                let _ = std::fs::remove_file(&path);
                            }
                        });
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
    // Pending rechecks stop between rounds and join BEFORE the
    // snapshot (AUD-013): a trailing round may still park resolved
    // confirmations, which the snapshot must see. A round that
    // outlives the stop window is aborted loudly; WAL replay
    // recovers its pending state on the next boot.
    if let Some(stop) = pending_recheck_stop.take() {
        let _ = stop.send(());
        if let Some(handle) = pending_recheck_handle.take() {
            match tokio::time::timeout(Duration::from_secs(65), handle).await {
                Ok(Ok(())) => {}
                Err(_) => warn!(
                    "pending recheck round outlived the 65s stop window; aborting (trailing confirm outcomes may be dropped; WAL replay recovers them next boot)"
                ),
                Ok(Err(join)) => warn!("pending recheck task ended with error: {join}"),
            }
        }
    }

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

    let drain = async { while in_flight.join_next().await.is_some() {} };
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
        warn!(
            "in-flight units were aborted; any live dispatch mid-confirm has an UNKNOWN fill — check the explorer for the tracked mints above"
        );
    }

    if clean_shutdown {
        // A self-registered run removes its own pid record on a clean
        // exit (stop/daemon flows own their own cleanup).
        cli::unregister_running(&cli::resolve_paths().0);
        info!("shutdown complete (drained={drained})");
    } else {
        // Streams ended: exit nonzero so supervisors notice.
        warn!("streams ended; open positions need manual review");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CUSTODY_RECHECK_SECS, EXIT_LANE_PERMITS, HEARTBEAT_SECS, MAX_IN_FLIGHT, SECRET_ENV_VARS,
        SUPERVISION_SECS, live_mode_from, paper_fee_bps, supervision_due,
    };

    /// AUD-026: the pid record must carry the effective mode. Only
    /// `--live` or the exact `IS_LIVE=true` value is live.
    #[test]
    fn pid_record_follows_the_effective_live_mode() {
        assert!(live_mode_from(false, Some("true")));
        assert!(live_mode_from(true, None));
        assert!(live_mode_from(true, Some("false")));
        for env in [None, Some(""), Some("false"), Some("0"), Some("TRUE")] {
            assert!(
                !live_mode_from(false, env),
                "IS_LIVE={env:?} must stay paper"
            );
        }
    }

    /// AUD-025: the paper ledger consumes the typed commission; an
    /// invalid `FEE_BPS` contributes none (and does not widen stops).
    #[test]
    fn paper_ledger_ignores_an_invalid_fee() {
        let invalid = crate::settings::Settings::from_map(
            &[("FEE_BPS".to_string(), "10000".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(paper_fee_bps(&invalid), 0.0);
        let valid = crate::settings::Settings::from_map(
            &[("FEE_BPS".to_string(), "345".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(paper_fee_bps(&valid), 345.0);
    }

    /// AUD-008: a launch flood that holds every launch-lane permit
    /// cannot consume the exits' reserve — the dispatch-lane split
    /// keeps a curve/pool tick's and a manual sell's dispatcher
    /// independently available.
    #[test]
    fn launch_flood_cannot_starve_the_exit_lane() {
        let launch = tokio::sync::Semaphore::new(MAX_IN_FLIGHT - EXIT_LANE_PERMITS);
        let exit = tokio::sync::Semaphore::new(EXIT_LANE_PERMITS);
        let _flood = launch
            .try_acquire_many((MAX_IN_FLIGHT - EXIT_LANE_PERMITS) as u32)
            .expect("simulated launch flood");
        assert_eq!(
            launch.available_permits(),
            0,
            "lane is fully saturated by the flood"
        );
        let tick = exit
            .try_acquire()
            .expect("exit permits remain under launch flood");
        drop(tick);
        assert_eq!(
            exit.available_permits(),
            EXIT_LANE_PERMITS,
            "exit budget is a separate pool, never shared"
        );
    }

    /// AUD-015: the boot scrub list covers every secret-bearing env
    /// var the boot owns; `_FILE` variants (paths) stay.
    #[test]
    fn secret_env_scrub_covers_every_secret() {
        for name in [
            "INFERENCE_API_KEY",
            "MAINNET_RPC_URL",
            "MAINNET_WSS_RPC_URL",
            "MAINNET_RPC_URLS_FALLBACK",
            alert::DISCORD_BOT_TOKEN_ENV,
            listener::YELLOWSTONE_TOKEN_ENV,
        ] {
            assert!(
                SECRET_ENV_VARS.contains(&name),
                "{name} must be in SECRET_ENV_VARS (scrubs before any thread exists)"
            );
        }
        for name in SECRET_ENV_VARS {
            assert!(!name.ends_with("_FILE"), "{name} is a path, not a secret");
        }
    }

    /// AUD-015 acceptance: after the boot scrub, child processes
    /// (which inherit the process env) see no secret variables/values.
    #[test]
    #[cfg(unix)]
    fn children_inherited_env_carries_no_secrets() {
        // Fictitious secret fixtures (never real values).
        let secrets = [
            ("INFERENCE_API_KEY", "test-key-fixture-1"),
            (
                "MAINNET_RPC_URL",
                "https://test.example/?api-key=test-victim-2",
            ),
            (
                "MAINNET_RPC_URLS_FALLBACK",
                "https://test.example/?api-key=test-victim-3",
            ),
            (listener::YELLOWSTONE_TOKEN_ENV, "test-token-fixture-4"),
            (alert::DISCORD_BOT_TOKEN_ENV, "test-token-fixture-5"),
            ("SOLANA_PRIVATE_KEY", "test-mint-fixture-6"),
        ];
        // Restored on drop even while unwinding (AUD-022 guard).
        let _guard = {
            let vars: Vec<(&str, Option<&str>)> =
                secrets.iter().map(|(k, v)| (*k, Some(*v))).collect();
            crate::test_env::scoped(&vars)
        };
        // The exact sync-boot scrub (SOLANA_PRIVATE_KEY included: in
        // live boot the wallet loader scrubs it after `load_from_env`).
        super::scrub_secret_env();
        unsafe { std::env::remove_var("SOLANA_PRIVATE_KEY") };
        // A child spawned after boot (any `process::Command`, e.g. the
        // upgrade check) inherits the scrubbed environment.
        let out = std::process::Command::new("env")
            .output()
            .expect("spawn env");
        let stdout = String::from_utf8_lossy(&out.stdout);
        for (_, value) in secrets {
            assert!(
                !stdout.contains(value),
                "secret fixture value must not reach child env"
            );
        }
        // Sanity: `env` did run.
        assert!(out.status.success());
        assert!(stdout.contains("="), "expected name=value lines");
    }

    /// AUD-003: the supervision sampler must be shorter than every
    /// window it observes — the documented 30-minute custody recheck
    /// and the one-hour GiveUp window — and the heartbeat card keeps
    /// its own 6-hour cadence.
    #[test]
    fn supervision_cadence_observes_its_windows() {
        assert_eq!(
            CUSTODY_RECHECK_SECS, 1_800,
            "documented 30-minute custody recheck"
        );
        assert_eq!(HEARTBEAT_SECS, 21_600, "heartbeat card cadence unchanged");
        assert_eq!(SUPERVISION_SECS, 60, "short supervision sampling");
        // The sampler must be shorter than both windows
        // (compile-time check; constant asserts belong in const).
        const _: () = const { assert!(SUPERVISION_SECS < CUSTODY_RECHECK_SECS) };
        const _: () = const { assert!(SUPERVISION_SECS < 3_600) };
        // A recheck due at t+1800 is found by the first supervision
        // tick at or past it: cadence at most 30 min + one tick, never
        // six hours.
        assert!(!supervision_due(0, 1_799, CUSTODY_RECHECK_SECS));
        assert!(supervision_due(0, 1_800, CUSTODY_RECHECK_SECS));
        assert!(supervision_due(0, 1_860, CUSTODY_RECHECK_SECS));
        // Time-injected GiveUp observation: a burst of three GiveUps
        // at t=100 (one-hour window, threshold 3) must be counted by
        // the next 60-second supervision tick, using the same
        // trailing-window predicate the supervision arm runs.
        let gives = [100u64, 100, 100];
        let window = 3_600;
        let count_at = |now: u64| {
            gives
                .iter()
                .filter(|t| now >= **t && now - **t <= window)
                .count()
        };
        let next_supervision_tick = 120;
        assert!(
            count_at(next_supervision_tick) >= 3,
            "the next supervision tick evaluates the burst"
        );
    }
    use super::{BootCustody, boot_custody};

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

    /// Per-mint boot verdicts: never a false park on two real bags,
    /// never an auto-exit at an unverified size, one hand-off at most.
    #[test]
    fn boot_custody_pins_the_claim_shapes() {
        use BootCustody::*;
        // Chain backs the claims exactly (single bag, parked only).
        assert_eq!(boot_custody(1_000, 0, 1_000), Keep);
        assert_eq!(boot_custody(0, 400, 400), Keep);
        // Parked note plus a real re-buy: sum matches, both stay.
        assert_eq!(boot_custody(500, 500, 1_000), Keep);
        // Resurrected parked bag: the SAME bag claimed twice.
        assert_eq!(
            boot_custody(1_000, 1_000, 1_000),
            SuppressTracked { handoff: None }
        );
        // Ledger overstates: suppress and hand off at the chain size.
        assert_eq!(
            boot_custody(1_000, 0, 600),
            SuppressTracked { handoff: Some(600) }
        );
        // Chain empty: nothing left to exit, no note.
        assert_eq!(boot_custody(1_000, 0, 0), SuppressTracked { handoff: None });
        // Parked note overstated: the operator's claim, no suppression.
        assert_eq!(boot_custody(0, 400, 100), SuppressTracked { handoff: None });
        // Chain holds more than claimed: every claim is backed.
        assert_eq!(boot_custody(1_000, 0, 1_200), Excess);
        // Saturation must not wrap into a false `Keep`.
        assert_eq!(
            boot_custody(u64::MAX, u64::MAX, 10),
            SuppressTracked { handoff: None }
        );
    }
}
