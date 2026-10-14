//! Supervisor CLI: `start` / `stop` / `restart` (+ `status`, `logs`, `run`, `sell`).
//!
//! The binary never runs the pipeline bare. Every invocation must name a
//! subcommand; anything else prints [`USAGE`] to stderr and exits nonzero
//! so a typo can never boot the bot by accident.
//!
//! - `start [--live]` / `restart [--live]` — spawn the pipeline detached
//!   in the background (`run` child with stdout/stderr appended to the log
//!   file). The log records whether this was the first run or a re-run.
//! - `stop` — SIGTERM the daemon (graceful drain), SIGKILL on timeout.
//! - `status` — report whether the daemon is alive.
//! - `logs [-n N] [--follow]` — print the tail of the daemon log.
//! - `run [--live]` — foreground pipeline (what the daemon child runs).
//! - `sell <mint>` — queue a manual full exit for one mint: the running
//!   daemon closes its paper position (Manual) and sells the whole
//!   tracked live bag on its next round, no daemon restart needed.
//!
//! `--live` (or `IS_LIVE=true`) keeps its old meaning — real Jito-routed
//! swaps — but now attaches to a subcommand (`start --live`,
//! `restart --live`, `run --live`).
//!
//! Files default to `~/.pump-sentry/bot.log` /
//! `~/.pump-sentry/bot.pid`; override with `LOG_FILE` / `PID_FILE`
//! (a leading `~/` in those values expands to the home directory).
//! With relative overrides, start and stop must run from the same
//! directory.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Default state directory (`~/.pump-sentry`): log, pid, sell
/// requests, WAL, and shutdown snapshots all live here unless
/// overridden. `None` when `HOME` is unset (then relative `logs/`
/// fallbacks apply so the bot still runs).
pub fn default_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pump-sentry"))
}

/// Default daemon log file (`~/.pump-sentry/bot.log`).
pub fn default_log_file() -> PathBuf {
    default_state_dir()
        .map(|d| d.join("bot.log"))
        .unwrap_or_else(|| PathBuf::from("logs/bot.log"))
}

/// Default daemon pid file (`~/.pump-sentry/bot.pid`).
pub fn default_pid_file() -> PathBuf {
    default_state_dir()
        .map(|d| d.join("bot.pid"))
        .unwrap_or_else(|| PathBuf::from("logs/bot.pid"))
}
/// Lines shown by `logs` without `-n`.
pub const DEFAULT_LOG_LINES: usize = 100;
/// Grace period after SIGTERM before SIGKILL (`stop`, `restart`).
/// Sized past the pipeline's own shutdown budget (5s producer join +
/// 30s in-flight drain): `stop` polls and returns as soon as the daemon
/// exits, so a longer grace only costs time when the drain actually
/// needs it — instead of SIGKILLing a healthy graceful shutdown.
pub const STOP_GRACE_SECS: u64 = 45;
/// Poll cadence while waiting for the daemon to exit.
pub const STOP_POLL_MS: u64 = 200;

/// Usage text printed when no (valid) subcommand is given.
pub const USAGE: &str = "\
Usage: pump-sentry <command> [options]

Commands:
  start [--live]          Run the bot in the background (daemon, log to file)
  stop                    Stop the daemon gracefully (SIGTERM then SIGKILL)
  restart [--live]        stop + start (restart time is recorded in the log)
  status                  Show whether the daemon is running (pid, mode, log)
  logs [-n N] [--follow]  Show the last N log lines (default 100)
  run [--live]            Run in the foreground (for debugging)
  sell <mint>             Queue a manual full exit for one mint now
  help                    Show this help

Options:
  --live                  Live mode: real swaps via Jito (needs SOLANA_PRIVATE_KEY).
                          Without this flag the bot only paper-trades (no funds move).
  -n N, --lines N         Line count for `logs` (default 100).
  -f, --follow            Follow the log continuously (Ctrl-C to stop).
  -h, --help              Show this help.

Env:
  IS_LIVE=true            Same as --live.  LOG_FILE / PID_FILE override the default paths.
  SOLANA_PRIVATE_KEY      Base58 keypair (required for live mode).

Examples:
  pump-sentry start
  pump-sentry start --live
  pump-sentry logs -n 50 --follow
  pump-sentry restart --live
  pump-sentry sell <mint>
  pump-sentry stop
";

/// Parsed supervisor action (pure; see [`parse_cli`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// Background the pipeline.
    Start {
        /// Live (real funds) when true.
        live: bool,
    },
    /// Stop the daemon.
    Stop,
    /// Stop then start.
    Restart {
        /// Live (real funds) when true.
        live: bool,
    },
    /// Report daemon liveness.
    Status,
    /// Show the log tail.
    Logs {
        /// Lines to show.
        lines: usize,
        /// Keep following.
        follow: bool,
    },
    /// Foreground pipeline.
    Run {
        /// Live (real funds) when true.
        live: bool,
    },
    /// Queue a manual full exit for one mint (handled by the daemon).
    Sell {
        /// Token mint (base58) to exit fully.
        mint: String,
    },
    /// Print usage; `Some` carries the human-readable cause.
    Help {
        /// Why usage is shown (`None` = explicit `help`).
        error: Option<String>,
    },
}

/// True when live mode is requested via argv `--live` or `IS_LIVE=true`.
pub fn live_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == "--live") || std::env::var("IS_LIVE").is_ok_and(|v| v == "true")
}

/// Parse argv (including argv[0]) into a [`CliAction`]. Pure and total:
/// unknown or missing commands become [`CliAction::Help`] with a cause —
/// the caller prints [`USAGE`] and exits nonzero without touching the
/// pipeline.
pub fn parse_cli(args: &[String]) -> CliAction {
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    match cmd {
        "start" => CliAction::Start {
            live: live_flag(args),
        },
        "stop" => CliAction::Stop,
        "restart" => CliAction::Restart {
            live: live_flag(args),
        },
        "status" => CliAction::Status,
        "run" => CliAction::Run {
            live: live_flag(args),
        },
        "logs" => CliAction::Logs {
            lines: log_lines(args),
            follow: args.iter().any(|a| a == "--follow" || a == "-f"),
        },
        "sell" => match args.get(2).map(String::as_str).unwrap_or("") {
            "" => CliAction::Help {
                error: Some("`sell` needs a mint: `pump-sentry sell <mint>`.".to_string()),
            },
            mint => CliAction::Sell { mint: mint.to_string() },
        },
        "help" | "--help" | "-h" => CliAction::Help { error: None },
        "" => CliAction::Help {
            error: Some(
                "no command given. The bot never runs without an explicit command.".to_string(),
            ),
        },
        other => CliAction::Help {
            error: Some(format!("unknown command: '{other}'.")),
        },
    }
}

/// `-n N` / `--lines N` value for `logs`; falls back to
/// [`DEFAULT_LOG_LINES`] on missing/unparsable input (never panics, never
/// fails the command for a typo).
fn log_lines(args: &[String]) -> usize {
    args.windows(2)
        .find_map(|w| {
            (w[0] == "-n" || w[0] == "--lines")
                .then(|| w[1].parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(DEFAULT_LOG_LINES)
}

/// Resolved `(pid_file, log_file)` honoring `PID_FILE` / `LOG_FILE`
/// (a leading `~/` expands to the home directory), else the
/// `~/.pump-sentry/` defaults.
pub fn resolve_paths() -> (PathBuf, PathBuf) {
    let pid = std::env::var("PID_FILE")
        .ok()
        .map(|v| expand_tilde(&v))
        .unwrap_or_else(default_pid_file);
    let log = std::env::var("LOG_FILE")
        .ok()
        .map(|v| expand_tilde(&v))
        .unwrap_or_else(default_log_file);
    (pid, log)
}

/// Expand a leading `~/` (or bare `~`) against `HOME`; anything else
/// passes through untouched. Unset `HOME` leaves the value as-is.
fn expand_tilde(s: &str) -> PathBuf {
    let rest = s.strip_prefix("~/").or_else(|| (s == "~").then_some(""));
    match (rest, std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(s),
    }
}

/// Daemon identity recorded in the pid file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidInfo {
    /// OS pid of the daemon child.
    pub pid: u32,
    /// Whether it runs in live mode.
    pub live: bool,
    /// Unix seconds when it was started.
    pub started_at: u64,
}

impl PidInfo {
    /// Serialize (hand-rolled JSON: one small object, no new deps).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"pid\":{},\"live\":{},\"started_at\":{}}}",
            self.pid, self.live, self.started_at
        )
    }

    /// Parse [`PidInfo::to_json`] output; `None` on any shape mismatch
    /// (a corrupt pid file reads as "not running", never as a panic).
    pub fn from_json(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let raw = raw.strip_prefix('{')?.strip_suffix('}')?;
        let mut pid = None;
        let mut live = None;
        let mut started_at = None;
        for part in raw.split(',') {
            let (k, v) = part.split_once(':')?;
            match k.trim().trim_matches('"') {
                "pid" => pid = v.trim().parse().ok(),
                "live" => {
                    live = match v.trim() {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    };
                }
                "started_at" => started_at = v.trim().parse().ok(),
                _ => {}
            }
        }
        Some(Self {
            pid: pid?,
            live: live?,
            started_at: started_at?,
        })
    }
}

/// Read the pid file; `None` when missing/unreadable/corrupt.
pub fn read_pid_file(path: &Path) -> Option<PidInfo> {
    let raw = std::fs::read_to_string(path).ok()?;
    PidInfo::from_json(&raw)
}

/// True when `pid` names a live process (`kill -0` on unix; pid-file
/// existence elsewhere — documented limitation for non-unix hosts).
///
/// The range guard comes first: pid 0 means "our process group" and any
/// value above `i32::MAX` overflows `pid_t` (on Linux `u32::MAX` wraps
/// to -1 = "all our processes") — `kill` would then succeed for a pid
/// that can never name a daemon. Reject without invoking anything.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// The daemon currently recorded AND alive, if any.
pub fn running_daemon(pid_file: &Path) -> Option<PidInfo> {
    let info = read_pid_file(pid_file)?;
    process_alive(info.pid).then_some(info)
}

/// Quote one shell word for `sh -c` (single-quote with embedded-quote
/// escape; never panics on hostile paths).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Current unix seconds (0 on broken clock; mirrors `settings::now_secs`
/// without coupling this module to it).
pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append one line to the log file (best-effort: failures warn on stderr
/// but never fail the supervisor command).
pub fn append_log_line(log: &Path, line: &str) {
    if let Some(parent) = log.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("warning: cannot create log directory: {e}");
        return;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = match opts.open(log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: cannot write log: {e}");
            return;
        }
    };
    if let Err(e) = writeln!(file, "{line}") {
        eprintln!("warning: cannot write log: {e}");
    }
}

/// Last `n` lines of the log file (`n == 0` yields none; missing file is
/// an error so `logs` can say so loudly).
///
/// Bounded memory: seeks from the end and reads only the trailing chunk
/// (64 KiB windows, back to start at most until `n` lines are covered),
/// so `logs 20` on a 50 MB file never loads the whole file into RAM.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let len = file.metadata()?.len();
    // 64 KiB per window: a few windows cover any sane `n`; huge `n`
    // degrades to a full read, never worse than the old behavior.
    const WINDOW: u64 = 65536;
    let mut pos = len;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let take = WINDOW.min(pos);
        pos -= take;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; take as usize];
        file.read_exact(&mut chunk)?;
        // Prepend: chunk precedes everything read so far.
        let mut next = Vec::with_capacity(chunk.len() + buf.len() + 1);
        next.extend_from_slice(&chunk);
        // Keep a single newline boundary between windows (the split
        // below counts `\n`; without it a line spanning two windows
        // would merge and undercount).
        next.extend_from_slice(&buf);
        buf = next;
        let newlines = buf.iter().filter(|b| **b == b'\n').count();
        if newlines > n || pos == 0 {
            break;
        }
    }
    let text = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // A trailing newline yields no extra empty line via `.lines()` —
    // matches the old `read_to_string` semantics exactly.
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    Ok(lines)
}

/// Open (or create) the daemon log for appending, owner-only on unix.
/// Split out for testability; the SIGHUP-reopen path
/// ([`redirect_stdout_stderr_to`]) and logrotate rely on it. Never
/// truncates: post-rename writes must append to the fresh file, not
/// wipe it.
pub fn reopen_log_file(log: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = log.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(log)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(log, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// Point this process's stdout/stderr at an already-open log file
/// (unix `dup2` onto fds 1/2). The tracing subscriber and `eprintln!`
/// resolve fds 1/2 per write, so after this call every subsequent log
/// line lands in the new file — no restart, no lost lines. External
/// logrotate renames `bot.log` then `kill -HUP`s the daemon (pid from
/// the pid file); `copytruncate` is unnecessary and must NOT be used
/// (its copy-then-truncate window drops lines).
#[cfg(unix)]
pub fn redirect_stdout_stderr_to(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    for target in [1, 2] {
        loop {
            // SAFETY: `fd` is a live open file; `dup2` onto 1/2 only
            // redirects this process's own stdio. Retried on EINTR.
            if unsafe { libc::dup2(fd, target) } == target {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Future that resolves on the first shutdown request: SIGINT (Ctrl-C)
/// or, on unix, SIGTERM (what `stop`, systemd, and docker send). The
/// pipeline used to watch Ctrl-C only, so `stop` killed it mid-commit —
/// the non-atomic shutdown this fixes.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(sig) => sig,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Shutdown snapshot: what was still open when the signal arrived, so a
/// drained-or-aborted shutdown never loses track silently. Serialized to
/// `shutdown-<ts>.json` next to the log file (best-effort) and echoed to the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownSummary {
    /// Unix seconds of the snapshot.
    pub ts: u64,
    /// Daemon pid.
    pub pid: u32,
    /// Live mode flag.
    pub live: bool,
    /// Mints with open paper positions.
    pub paper_open: Vec<String>,
    /// Mints with tracked live fills.
    pub live_tracked: Vec<String>,
    /// Watchlisted mints awaiting quorum.
    pub watchlist_pending: usize,
    /// True when the drain finished before the deadline.
    pub clean: bool,
}

impl ShutdownSummary {
    /// Escape one JSON string (quotes/backslashes/controls only; mints
    /// are base58 so this stays trivial and total).
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// Serialize (hand-rolled like [`PidInfo`]: small, total, no deps).
    pub fn to_json(&self) -> String {
        let join = |v: &[String]| v.iter().map(|s| Self::esc(s)).collect::<Vec<_>>().join(",");
        format!(
            "{{\"ts\":{},\"pid\":{},\"live\":{},\"paper_open\":[{}],\"live_tracked\":[{}],\"watchlist_pending\":{},\"clean\":{}}}",
            self.ts,
            self.pid,
            self.live,
            join(&self.paper_open),
            join(&self.live_tracked),
            self.watchlist_pending,
            self.clean
        )
    }
}

/// `start`: refuse when already running, else spawn the detached `run`
/// child, record the pid file, and report. Diverges via `process::exit`.
pub fn cmd_start(live: bool, invocation: &str) -> ! {
    let (pid_file, log_file) = resolve_paths();
    if let Some(info) = running_daemon(&pid_file) {
        eprintln!(
            "bot is already running (pid {}, {} mode). Use `restart` or `stop` first.",
            info.pid,
            if info.live { "live" } else { "paper" }
        );
        std::process::exit(1);
    }
    // Stale pid file (dead pid, corrupt content): clear it so the next
    // `status` reads clean.
    if pid_file.exists() {
        let _ = std::fs::remove_file(&pid_file);
    }
    // Disk guard: an append-forever daemon log from a previous long run
    // rotates here (mid-run rotation is impossible under shell
    // redirection), before the new child inherits the fd.
    rotate_log_if_huge(&log_file);
    // Owner-only log from the first byte: the shell `>>` preserves the
    // existing mode, so create (or chmod) here — custody lines (mints,
    // amounts, bundles) must never land in a world-readable file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&log_file).is_ok() {
            let _ = std::fs::set_permissions(&log_file, std::fs::Permissions::from_mode(0o600));
        } else {
            if let Some(parent) = log_file.parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = write_private_file(&log_file, b"");
        }
    }
    let first_run = !log_file.metadata().is_ok_and(|m| m.len() > 0);
    match spawn_background(&log_file, live, invocation, first_run) {
        Ok(pid) => {
            let info = PidInfo {
                pid,
                live,
                started_at: unix_secs(),
            };
            if let Err(e) = write_private_file(&pid_file, info.to_json().as_bytes()) {
                eprintln!("warning: pid recorded but pid file write failed: {e}");
            }
            // Health gate: fatal misconfiguration (bad COMMITMENT, dead
            // RPC) exits within seconds. Live mode additionally sleeps a
            // 5s abort window before loading the wallet, so its gate must
            // cover that window or a keyless `--live` start would report
            // healthy and die right after.
            let grace_secs = if live { 8 } else { 3 };
            std::thread::sleep(std::time::Duration::from_secs(grace_secs));
            if !process_alive(pid) {
                let _ = std::fs::remove_file(&pid_file);
                eprintln!(
                    "bot died within {grace_secs}s (likely misconfiguration). Last log lines:"
                );
                match tail_lines(&log_file, 15) {
                    Ok(tail) => {
                        for line in tail {
                            eprintln!("  {line}");
                        }
                    }
                    Err(e) => eprintln!("  (log unreadable: {e})"),
                }
                std::process::exit(1);
            }
            println!(
                "bot started in the background (pid {pid}, {} mode, log {})",
                if live { "live" } else { "paper" },
                log_file.display()
            );
            if live {
                println!("LIVE MODE: real funds move via Jito. Stop with `stop`.");
            }
            println!(
                "{}",
                if first_run {
                    "first run — recorded in the log"
                } else {
                    "re-run — recorded in the log"
                }
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            std::process::exit(1);
        }
    }
}

/// `stop`: SIGTERM, wait out the grace period, SIGKILL on timeout, drop
/// the pid file, and append a footer to the log. Idempotent: stopping a
/// stopped bot exits 0 with a note. Diverges via `process::exit`.
pub fn cmd_stop() -> ! {
    let code = stop_daemon();
    std::process::exit(code);
}

/// Shared stop body (returns the exit code) so `restart` can reuse it
/// without exiting mid-way.
pub fn stop_daemon() -> i32 {
    let (pid_file, log_file) = resolve_paths();
    let Some(info) = read_pid_file(&pid_file) else {
        println!("bot is not running (no pid file).");
        return 0;
    };
    if !process_alive(info.pid) {
        let _ = std::fs::remove_file(&pid_file);
        println!("bot is not running (pid {} is dead; pid file cleaned).", info.pid);
        return 0;
    }
    println!("stopping bot (pid {})...", info.pid);
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg(info.pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let rounds = STOP_GRACE_SECS * 1000 / STOP_POLL_MS;
        for _ in 0..rounds {
            if !process_alive(info.pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(STOP_POLL_MS));
        }
        if process_alive(info.pid) {
            eprintln!("daemon did not stop within {STOP_GRACE_SECS}s; SIGKILL...");
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(info.pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("stop is only fully supported on unix; remove the pid file manually.");
    }
    let stopped = !process_alive(info.pid);
    let _ = std::fs::remove_file(&pid_file);
    append_log_line(
        &log_file,
        &format!("=== bot stopped at unix_ts={} (pid {}) ===", unix_secs(), info.pid),
    );
    if stopped {
        println!("bot stopped.");
        0
    } else {
        eprintln!("failed to stop pid {}; inspect manually.", info.pid);
        1
    }
}

/// `restart [--live]`: best-effort stop, then start with the restart
/// invocation recorded in the log. Diverges via `process::exit`.
pub fn cmd_restart(live: bool) -> ! {
    let code = stop_daemon();
    if code != 0 {
        eprintln!("stop failed; restart aborted.");
        std::process::exit(code);
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    cmd_start(live, "restart");
}

/// `status`: one human line about daemon liveness. Exits 0 when running,
/// 1 when not (script-friendly). A stale pid file (dead pid) is cleaned
/// so the next `start`/`status` reads clean. Diverges via `process::exit`.
pub fn cmd_status() -> ! {
    let (pid_file, log_file) = resolve_paths();
    match running_daemon(&pid_file) {
        Some(info) => {
            println!(
                "running (pid {}, {} mode, started unix_ts={}, log {})",
                info.pid,
                if info.live { "live" } else { "paper" },
                info.started_at,
                log_file.display()
            );
            std::process::exit(0);
        }
        None => {
            if let Some(stale) = read_pid_file(&pid_file) {
                let _ = std::fs::remove_file(&pid_file);
                println!("stopped (stale pid {} cleaned; log {})", stale.pid, log_file.display());
            } else {
                println!("stopped (log {})", log_file.display());
            }
            std::process::exit(1);
        }
    }
}

/// `logs`: print the tail, optionally following. Diverges via
/// `process::exit` (follow loops until Ctrl-C).
pub async fn cmd_logs(lines: usize, follow: bool) -> ! {
    let (_, log_file) = resolve_paths();
    match tail_lines(&log_file, lines) {
        Ok(tail) => {
            for line in tail {
                println!("{line}");
            }
        }
        Err(e) => {
            eprintln!("cannot read log {}: {e}", log_file.display());
            std::process::exit(1);
        }
    }
    if follow {
        let mut pos = std::fs::metadata(&log_file).map(|m| m.len()).unwrap_or(0);
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => std::process::exit(0),
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {},
            }
            let len = std::fs::metadata(&log_file).map(|m| m.len()).unwrap_or(pos);
            if len < pos {
                pos = 0; // rotated/truncated: reread from the top
            }
            if len > pos
                && let Ok(file) = std::fs::File::open(&log_file)
                && let Ok(extra) = read_from(file, pos)
                && !extra.is_empty()
            {
                print!("{extra}");
                let _ = std::io::stdout().flush();
                pos = len;
            }
        }
    }
    std::process::exit(0);
}

/// Read the whole file then skip the first `pos` bytes (log-follow
/// helper; total on short reads).
fn read_from(mut file: std::fs::File, pos: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(pos))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Single-file size cap for the daemon log: rotate at/above this.
pub const LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;
/// Archived generations kept (`log.1` newest … `log.3` oldest).
pub const LOG_RETAIN_ARCHIVES: u32 = 3;

/// Size-triggered rotation at (re)start: the daemon child inherits its
/// stdout/stderr fd from shell redirection, so nothing in-process can
/// rotate mid-run — the supervisor rotates instead, before every spawn.
/// `log` at/above the cap shifts `.1`→`.2`→`.3` (oldest dropped) and the
/// new run starts fresh. Best-effort: rotation failure warns on stderr,
/// never blocks the start.
fn rotate_log_if_huge(log: &Path) {
    rotate_log_sized(log, LOG_ROTATE_BYTES, LOG_RETAIN_ARCHIVES);
}

/// Rotation core with explicit budget (unit-tested with byte budgets).
fn rotate_log_sized(log: &Path, cap_bytes: u64, retain: u32) {
    let huge = std::fs::metadata(log).is_ok_and(|m| m.len() >= cap_bytes);
    if !huge {
        return;
    }
    let archive = |i: u32| log.with_extension(format!("log.{i}"));
    // `with_extension` on `bot.log` yields `bot.log.N` — sibling archives.
    if retain == 0 {
        if std::fs::remove_file(log).is_err() {
            eprintln!("warning: oversized log {} unrotatable", log.display());
        }
        return;
    }
    if std::fs::remove_file(archive(retain)).is_err() {
        // Missing oldest is fine (first rotations); other errors surface below.
    }
    for i in (1..retain).rev() {
        let _ = std::fs::rename(archive(i), archive(i + 1));
    }
    if std::fs::rename(log, archive(1)).is_err() {
        eprintln!("warning: oversized log {} unrotatable", log.display());
    }
}

/// Write a file only its owner can read (0600 on unix): pid files,
/// sell requests, snapshots, and WAL state all name live custody
/// (mints, amounts, bundles) — world-readable defaults leak the book.
/// Best-effort chmod covers pre-existing files; creation mode covers
/// new ones. Returns the write outcome (chmod failure only warns).
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).is_err() {
            eprintln!(
                "warning: cannot restrict {} to owner-only",
                path.display()
            );
        }
    }
    Ok(())
}

/// Request-file path next to the log file (`sell-<mint>.json`).
/// Mints are base58 (no path separators by construction — validated in
/// [`cmd_sell`]), so the filename round-trips safely.
pub fn sell_request_path(log_file: &Path, mint: &str) -> PathBuf {
    let name = format!("sell-{mint}.json");
    log_file
        .parent()
        .map(|d| d.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Pending manual-sell requests: `(mint, path)` pairs sorted by mint
/// (deterministic drain order). Best-effort: an unreadable directory
/// yields nothing; non-request files are ignored.
pub fn pending_sell_requests(log_file: &Path) -> Vec<(String, PathBuf)> {
    let dir = log_file.parent().filter(|d| !d.as_os_str().is_empty());
    let entries = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("sell-") && n.ends_with(".json")
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut out: Vec<(String, PathBuf)> = entries
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            let mint = name.strip_prefix("sell-")?.strip_suffix(".json")?.to_string();
            if mint.is_empty() { None } else { Some((mint, p)) }
        })
        .collect();
    out.sort();
    out
}

/// `sell <mint>`: queue a manual full exit for one mint. Validates the
/// mint, refuses when no daemon runs (a queued request would sit until
/// the next boot and fire stale — fail-closed at the CLI), else drops a
/// request file the daemon drains on its gate cadence. Written
/// temp-then-rename so a crash can never leave a torn body behind.
pub fn cmd_sell(mint: &str) {
    if executor::parse_pubkey(mint).is_err() {
        eprintln!("bad MINT '{mint}': not a base58 pubkey");
        std::process::exit(1);
    }
    let (pid_file, log_file) = resolve_paths();
    if running_daemon(&pid_file).is_none() {
        eprintln!(
            "no daemon running (log {}); start the bot first",
            log_file.display()
        );
        std::process::exit(1);
    }
    let path = sell_request_path(&log_file, mint);
    let body = format!("{{\"mint\":\"{mint}\",\"ts\":{}}}", unix_secs());
    let tmp = path.with_extension("json.part");
    let staged = match write_private_file(&tmp, body.as_bytes()) {
        Ok(()) => std::fs::rename(&tmp, &path),
        Err(e) => Err(e),
    };
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("cannot queue sell request {}: {e}", path.display());
        std::process::exit(1);
    }
    println!("manual sell queued for {mint} (daemon drains it on its next round)");
}

/// Mint claimed inside a sell-request body (`{"mint":"…","ts":…}`).
/// `None` on any shape mismatch: the daemon drops such files loudly
/// instead of acting on — or retrying — a forged/torn request. Parsed
/// with `serde_json` so standard formatting (spaces, multi-line, field
/// order) is accepted; Base58 carries no quotes, so no unescaping edge
/// cases arise. Empty mints still return `None`.
pub fn sell_request_body_mint(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mint = value.get("mint")?.as_str()?;
    if mint.is_empty() {
        return None;
    }
    Some(mint.to_string())
}

/// Spawn `<current-exe> run [--live]` detached with stdout/stderr
/// appended to `log`, returning the daemon pid.
///
/// Unix: `nohup ... & echo $!` under `sh -c` reparents the child to init
/// and ignores SIGHUP, so exiting (and `cargo run` exiting) leaves it
/// running. Elsewhere: a plain detached spawn (documented limitation).
fn spawn_background(
    log: &Path,
    live: bool,
    invocation: &str,
    first_run: bool,
) -> std::io::Result<u32> {
    if let Some(parent) = log.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe()?;
    let mut child_args = vec!["run".to_string()];
    if live {
        child_args.push("--live".to_string());
    }
    #[cfg(unix)]
    {
        let script = format!(
            "BOT_DAEMON=1 BOT_INVOCATION={} BOT_FIRST_RUN={} nohup {} {} >> {} 2>&1 & echo $!",
            sh_quote(invocation),
            if first_run { "1" } else { "0" },
            sh_quote(&exe.to_string_lossy()),
            child_args
                .iter()
                .map(|a| sh_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
            sh_quote(&log.to_string_lossy()),
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "sh spawn failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .map_err(|e| std::io::Error::other(format!("invalid pid from sh: {e}")))
    }
    #[cfg(not(unix))]
    {
        let _ = (invocation, first_run);
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let child = std::process::Command::new(exe)
            .args(&child_args)
            .stdin(std::process::Stdio::null())
            .stdout(log_file.try_clone()?)
            .stderr(log_file)
            .spawn()?;
        Ok(child.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_all_commands() {
        assert_eq!(
            parse_cli(&argv(&["bot", "start"])),
            CliAction::Start { live: false }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "start", "--live"])),
            CliAction::Start { live: true }
        );
        assert_eq!(parse_cli(&argv(&["bot", "stop"])), CliAction::Stop);
        assert_eq!(
            parse_cli(&argv(&["bot", "restart", "--live"])),
            CliAction::Restart { live: true }
        );
        assert_eq!(parse_cli(&argv(&["bot", "status"])), CliAction::Status);
        assert_eq!(
            parse_cli(&argv(&["bot", "run", "--live"])),
            CliAction::Run { live: true }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "logs"])),
            CliAction::Logs {
                lines: DEFAULT_LOG_LINES,
                follow: false
            }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "logs", "-n", "20", "--follow"])),
            CliAction::Logs {
                lines: 20,
                follow: true
            }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "logs", "--lines", "5", "-f"])),
            CliAction::Logs {
                lines: 5,
                follow: true
            }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "help"])),
            CliAction::Help { error: None }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "sell", "MINT"])),
            CliAction::Sell { mint: "MINT".to_string() }
        );
        assert!(matches!(
            parse_cli(&argv(&["bot", "sell"])),
            CliAction::Help { error: Some(_) }
        ));
    }

    #[test]
    fn missing_or_unknown_command_is_help_with_cause() {
        assert!(matches!(
            parse_cli(&argv(&["bot"])),
            CliAction::Help { error: Some(_) }
        ));
        assert!(matches!(parse_cli(&[]), CliAction::Help { error: Some(_) }));
        // Old bare `--live` style no longer boots the pipeline: it must
        // say so and point at `start --live` / `run --live`.
        assert!(matches!(
            parse_cli(&argv(&["bot", "--live"])),
            CliAction::Help { error: Some(_) }
        ));
        assert!(matches!(
            parse_cli(&argv(&["bot", "frobnicate"])),
            CliAction::Help { error: Some(_) }
        ));
    }

    #[test]
    fn bad_lines_value_falls_back_to_default() {
        assert_eq!(
            parse_cli(&argv(&["bot", "logs", "-n", "bogus"])),
            CliAction::Logs {
                lines: DEFAULT_LOG_LINES,
                follow: false
            }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "logs", "-n"])),
            CliAction::Logs {
                lines: DEFAULT_LOG_LINES,
                follow: false
            }
        );
    }

    #[test]
    fn pid_info_json_roundtrips_and_rejects_garbage() {
        let info = PidInfo {
            pid: 12345,
            live: true,
            started_at: 1_700_000_000,
        };
        assert_eq!(PidInfo::from_json(&info.to_json()), Some(info));
        assert_eq!(PidInfo::from_json(""), None);
        assert_eq!(PidInfo::from_json("12345"), None);
        assert_eq!(PidInfo::from_json("{\"pid\":1}"), None);
        assert_eq!(
            PidInfo::from_json("{\"pid\":\"x\",\"live\":false,\"started_at\":0}"),
            None
        );
    }

    #[test]
    fn shell_quoting_escapes_single_quotes() {
        assert_eq!(sh_quote("abc"), "'abc'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn reopen_log_file_creates_and_appends() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "cli-reopen-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let log = dir.join("bot.log");
        // Creates parents + file, then appends without truncating.
        let mut f = reopen_log_file(&log).expect("create");
        writeln!(f, "first").expect("write");
        drop(f);
        let mut f = reopen_log_file(&log).expect("reopen");
        writeln!(f, "second").expect("write");
        drop(f);
        assert_eq!(
            std::fs::read_to_string(&log).expect("read"),
            "first\nsecond\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&log).expect("meta").permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_paths_defaults_and_overrides() {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock");
        let saved = ["HOME", "PID_FILE", "LOG_FILE"]
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect::<Vec<_>>();
        let restore = || {
            for (k, v) in &saved {
                match v {
                    Some(v) => unsafe { std::env::set_var(k, v) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        };
        // Defaults live under the home state dir.
        unsafe { std::env::set_var("HOME", "/tmp/fakehome") };
        unsafe { std::env::remove_var("PID_FILE") };
        unsafe { std::env::remove_var("LOG_FILE") };
        let (pid, log) = resolve_paths();
        assert_eq!(pid, PathBuf::from("/tmp/fakehome/.pump-sentry/bot.pid"));
        assert_eq!(log, PathBuf::from("/tmp/fakehome/.pump-sentry/bot.log"));
        // Explicit overrides win; a leading ~/ expands against HOME.
        unsafe { std::env::set_var("PID_FILE", "/tmp/x.pid") };
        unsafe { std::env::set_var("LOG_FILE", "~/custom/bot.log") };
        let (pid, log) = resolve_paths();
        assert_eq!(pid, PathBuf::from("/tmp/x.pid"));
        assert_eq!(log, PathBuf::from("/tmp/fakehome/custom/bot.log"));
        restore();
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let path = std::env::temp_dir().join(format!(
            "cli-tail-test-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, "a\nb\nc\nd\n").expect("write");
        assert_eq!(tail_lines(&path, 2).expect("tail"), vec!["c", "d"]);
        assert_eq!(tail_lines(&path, 99).expect("tail").len(), 4);
        assert!(tail_lines(&path, 0).expect("tail").is_empty());
        assert!(tail_lines(&path.join("missing"), 10).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn current_process_is_alive_but_absurd_pid_is_not() {
        assert!(process_alive(std::process::id()));
        // 0 = own process group, u32::MAX overflows pid_t to -1 = all
        // processes: both fake success on Linux without the range guard.
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn shutdown_summary_serializes() {
        let s = ShutdownSummary {
            ts: 1,
            pid: 2,
            live: false,
            paper_open: vec!["MINT".to_string()],
            live_tracked: vec![],
            watchlist_pending: 3,
            clean: true,
        };
        let json = s.to_json();
        assert!(json.contains("\"paper_open\":[\"MINT\"]"), "{json}");
        assert!(json.contains("\"watchlist_pending\":3"), "{json}");
    }

    #[test]
    fn sell_request_body_mint_parses() {
        assert_eq!(
            sell_request_body_mint(r#"{"mint":"ABC","ts":1}"#),
            Some("ABC".to_string())
        );
        // Standard JSON formatting (spaces, newlines, field order) must
        // also parse — the old hand-rolled `split_once("\"mint\":\"")`
        // dropped these as poison.
        assert_eq!(
            sell_request_body_mint(r#"{"mint": "ABC", "ts": 1}"#),
            Some("ABC".to_string())
        );
        assert_eq!(
            sell_request_body_mint("{\n  \"mint\" : \"ABC\",\n  \"ts\" : 1\n}"),
            Some("ABC".to_string())
        );
        assert_eq!(
            sell_request_body_mint(r#"{"ts":1,"mint":"ABC"}"#),
            Some("ABC".to_string())
        );
        // Anything else is poison: the drain drops it, never acts on it.
        assert_eq!(sell_request_body_mint(""), None);
        assert_eq!(sell_request_body_mint("not json"), None);
        assert_eq!(sell_request_body_mint(r#"{"mint":"","ts":1}"#), None);
        assert_eq!(sell_request_body_mint(r#"{"ts":1}"#), None);
        assert_eq!(sell_request_body_mint(r#"{"mint":123,"ts":1}"#), None);
    }

    #[test]
    fn sell_request_roundtrip() {
        // Request files sort by mint and parse back (daemon drain order
        // is deterministic; non-request files ignored).
        let dir = std::env::temp_dir().join(format!(
            "cli-sell-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let log = dir.join("bot.log");
        std::fs::write(&log, "x").expect("write");
        std::fs::write(dir.join("bot.pid"), "1").expect("write");
        std::fs::write(dir.join("notes.txt"), "nope").expect("write");
        let a = sell_request_path(&log, "MINTB");
        let b = sell_request_path(&log, "MINTA");
        std::fs::write(&a, "{}").expect("write");
        std::fs::write(&b, "{}").expect("write");
        let pending = pending_sell_requests(&log);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].0, "MINTA");
        assert_eq!(pending[1].0, "MINTB");
        assert_eq!(pending[0].1, b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_lists_all_commands() {
        for cmd in [
            "start", "stop", "restart", "status", "logs", "run", "sell", "help", "--live",
        ] {
            assert!(USAGE.contains(cmd), "usage missing {cmd}");
        }
    }

    #[test]
    fn log_rotation_shifts_and_bounces_small_logs() {
        let dir = std::env::temp_dir().join(format!(
            "cli-rotate-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let log = dir.join("bot.log");
        // Under budget: untouched.
        std::fs::write(&log, vec![0u8; 100]).expect("write");
        rotate_log_sized(&log, 1_000, 3);
        assert_eq!(std::fs::metadata(&log).expect("meta").len(), 100);
        // At budget: shift .1→.2, current→.1, fresh start.
        std::fs::write(log.with_extension("log.1"), vec![1u8; 10]).expect("write");
        std::fs::write(&log, vec![0u8; 1_000]).expect("write");
        rotate_log_sized(&log, 1_000, 2);
        assert!(std::fs::metadata(&log).is_err(), "current must move away");
        assert_eq!(std::fs::metadata(log.with_extension("log.1")).expect("m").len(), 1_000);
        assert_eq!(std::fs::metadata(log.with_extension("log.2")).expect("m").len(), 10);
        // Oldest generation dropped past retention.
        std::fs::write(&log, vec![0u8; 1_000]).expect("write");
        rotate_log_sized(&log, 1_000, 2);
        assert!(std::fs::metadata(log.with_extension("log.3")).is_err(), "no third archive");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
