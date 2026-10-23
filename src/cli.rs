//! Supervisor CLI. Every call needs a subcommand; else usage plus nonzero exit.

use std::io::Write;
use std::path::{Path, PathBuf};

/// State dir. `None` when `HOME` is unset.
pub fn default_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pump-sentry"))
}

/// Daemon log file.
pub fn default_log_file() -> PathBuf {
    default_state_dir()
        .map(|d| d.join("bot.log"))
        .unwrap_or_else(|| PathBuf::from("logs/bot.log"))
}

/// Daemon pid file.
pub fn default_pid_file() -> PathBuf {
    default_state_dir()
        .map(|d| d.join("bot.pid"))
        .unwrap_or_else(|| PathBuf::from("logs/bot.pid"))
}

/// Kill-switch marker filename in the state dir.
pub const ENTRIES_PAUSED_FILE: &str = "entries-paused";

/// State dir for a log path. A bare filename (or a path with no
/// parent) lives in the current working directory: `Path::parent()`
/// returns `Some("")` there, so that case maps to `.` explicitly —
/// every state artifact (WAL, day-state, sell requests, lock files)
/// must agree on one directory for a given `LOG_FILE`, including the
/// relative form (AUD-012: `pending_sell_requests` used to see no
/// directory at all for `LOG_FILE=bot.log`, silently dropping every
/// queued manual sell).
pub fn state_dir_for_log(log_file: &Path) -> PathBuf {
    match log_file.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// State file next to the daemon log (WAL, day-state, kill switch):
/// one directory holds everything the operator inspects, and a
/// `LOG_FILE` override keeps them together. A bare-filename log keeps
/// bare-filename state files (both resolve to the CWD, same as
/// [`state_dir_for_log`]).
pub fn state_file_beside_log(log_file: &Path, name: &str) -> PathBuf {
    log_file
        .parent()
        .map(|d| d.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Kill-switch marker path: while the file exists, no new entry opens.
pub fn entries_paused_path() -> PathBuf {
    let (_, log_file) = resolve_paths();
    state_file_beside_log(&log_file, ENTRIES_PAUSED_FILE)
}

/// True when an operator marker at `path` is present. Fail closed: a
/// stat error other than "not found" (permissions, I/O, a path that
/// is not a directory) counts as present, so a marker we cannot rule
/// out still halts entries instead of running through it.
pub fn marker_present(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Default `logs` line count.
pub const DEFAULT_LOG_LINES: usize = 100;
/// SIGTERM grace before SIGKILL.
pub const STOP_GRACE_SECS: u64 = 45;
/// Exit poll cadence.
pub const STOP_POLL_MS: u64 = 200;
/// Settle delay after SIGKILL.
pub const SIGKILL_SETTLE_MS: u64 = 500;
/// Pause between `stop` and `start`.
pub const RESTART_PAUSE_MS: u64 = 500;
/// `logs --follow` poll cadence.
pub const LOG_FOLLOW_POLL_MS: u64 = 500;

/// Usage text.
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
  pause                   Kill switch: halt new entries (open positions keep managing)
  resume                  Clear the kill switch (entries may open again)
  discord                 Run the Discord sidecar (read-only slash commands; needs DISCORD_BOT_TOKEN)
  replay [file]           Re-derive recorded decisions offline + paper/live metrics
                          (default journal: journal.jsonl next to the log)
  research [file]         Offline loss-pattern analysis + filter proposals (read-only)
  init-config             Write ~/.pump-sentry/config.toml when missing
  upgrade                 Install the latest release (refuses when behind nothing or daemon runs; --allow-unverified to skip checksum)
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
  SOLANA_PRIVATE_KEY_FILE Owner-only (0600) file holding the keypair; preferred over the env var.

Examples:
  pump-sentry start
  pump-sentry start --live
  pump-sentry logs -n 50 --follow
  pump-sentry restart --live
  pump-sentry sell <mint>
  pump-sentry pause
  pump-sentry stop
";

/// Parsed action (pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// Background the pipeline.
    Start { live: bool },
    /// Stop the daemon.
    Stop,
    /// Stop then start.
    Restart { live: bool },
    /// Daemon liveness.
    Status,
    /// Log tail.
    Logs { lines: usize, follow: bool },
    /// Foreground pipeline.
    Run { live: bool },
    /// Write the categorized default config when missing.
    InitConfig,
    /// Install the latest release over this binary.
    /// Self-update from GitHub releases. Refuses to install
    /// unverified unless `--allow-unverified` is passed.
    Upgrade { allow_unverified: bool },
    /// Manual full exit for one mint.
    Sell { mint: String },
    /// Kill switch: halt new entries (open positions keep managing).
    Pause,
    /// Clear the kill switch.
    Resume,
    /// Run the Discord sidecar (foreground; Ctrl-C stops it).
    /// Read-only gateway process: answers slash commands from on-disk
    /// state; never trades and never holds custody.
    Discord,
    /// Re-derive recorded decisions offline; paper vs live metrics.
    Replay { file: Option<String> },
    /// Offline loss-pattern analysis over the journal (read-only).
    Research { file: Option<String> },
    /// Usage; `Some` is the cause.
    Help { error: Option<String> },
}

/// True when `--live` or `IS_LIVE=true` is set.
pub fn live_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == "--live") || std::env::var("IS_LIVE").is_ok_and(|v| v == "true")
}

/// Parse argv. Unknown or missing becomes `Help` with a cause.
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
        "init-config" => CliAction::InitConfig,
        "upgrade" => CliAction::Upgrade {
            allow_unverified: args.iter().any(|a| a == "--allow-unverified"),
        },
        "logs" => CliAction::Logs {
            lines: log_lines(args),
            follow: args.iter().any(|a| a == "--follow" || a == "-f"),
        },
        "sell" => match args.get(2).map(String::as_str).unwrap_or("") {
            "" => CliAction::Help {
                error: Some("`sell` needs a mint: `pump-sentry sell <mint>`.".to_string()),
            },
            mint => CliAction::Sell {
                mint: mint.to_string(),
            },
        },
        "pause" => CliAction::Pause,
        "resume" => CliAction::Resume,
        "discord" => CliAction::Discord,
        "replay" => CliAction::Replay {
            file: args.get(2).cloned().filter(|f| !f.is_empty() && f != "-"),
        },
        "research" => CliAction::Research {
            file: args.get(2).cloned().filter(|f| !f.is_empty() && f != "-"),
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

/// `-n` / `--lines` value; default when missing or bad. A present but
/// unparsable value warns on stderr (formerly a silent fallback).
fn log_lines(args: &[String]) -> usize {
    let glued = args.iter().find_map(|a| {
        a.strip_prefix("--lines=")
            .or_else(|| a.strip_prefix("-n").filter(|rest| !rest.is_empty()))
            .and_then(|v| v.parse::<usize>().ok())
    });
    if let Some(n) = glued {
        return n;
    }
    let spaced = args.windows(2).find_map(|w| {
        (w[0] == "-n" || w[0] == "--lines")
            .then(|| w[1].parse::<usize>().ok())
            .flatten()
    });
    if let Some(n) = spaced {
        return n;
    }
    if args.iter().any(|a| {
        a == "-n"
            || a == "--lines"
            || a.starts_with("--lines=")
            || (a.starts_with("-n") && a.len() > 2)
    }) {
        eprintln!("warning: bad log line count in {args:?}; using default {DEFAULT_LOG_LINES}");
    }
    DEFAULT_LOG_LINES
}

/// `(pid_file, log_file)` from env, `~/` expanded.
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

/// Expand leading `~/` against `HOME`.
fn expand_tilde(s: &str) -> PathBuf {
    let rest = s.strip_prefix("~/").or_else(|| (s == "~").then_some(""));
    match (rest, std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(s),
    }
}

/// Daemon identity in the pid file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PidInfo {
    pub pid: u32,
    pub live: bool,
    pub started_at: u64,
}

impl PidInfo {
    /// Serialize for the pid file. Plain scalars: cannot fail.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("pid info serializes")
    }

    /// Parse `to_json`; `None` on mismatch.
    pub fn from_json(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

/// Read the pid file; `None` when unusable.
pub fn read_pid_file(path: &Path) -> Option<PidInfo> {
    let raw = std::fs::read_to_string(path).ok()?;
    PidInfo::from_json(&raw)
}

/// True when `pid` is a live process.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: sig 0 sends nothing; it only probes.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if r == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Recorded daemon, if still alive.
pub fn running_daemon(pid_file: &Path) -> Option<PidInfo> {
    let info = read_pid_file(pid_file)?;
    process_alive(info.pid).then_some(info)
}

/// Start-lock path (serializes `start`). Distinct from the
/// state-dir process lock (`bot.lock`): the two locks must never
/// share a file, or a lost pid file would let `start` steal the lock
/// file a running trader holds (flock stays with the inode; a fresh
/// `create_new` on a new inode would not conflict).
pub fn start_lock_path(pid_file: &Path) -> PathBuf {
    let mut lock = pid_file.to_path_buf();
    lock.set_extension("start.lock");
    lock
}

/// State-dir process lock path: beside the log, one per state dir.
/// Every trader process (foreground `run` or daemon child) holds an
/// exclusive `flock` on it for its whole lifetime, so two processes
/// can never share a state dir / wallet / WAL (AUD-001).
pub fn state_lock_path(log_file: &Path) -> PathBuf {
    state_dir_for_log(log_file).join("bot.lock")
}

/// Held state-dir lock. The `File` must stay open for the lock to
/// hold; dropping it releases.
#[derive(Debug)]
pub struct StateDirLock {
    _file: std::fs::File,
}

impl StateDirLock {
    /// Try to acquire the exclusive process lock at `path`. Fails with
    /// `WouldBlock` when another trader holds it. Non-blocking: a
    /// second trader must refuse fast, before reading any state.
    pub fn try_acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::io::AsRawFd;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)?;
            // SAFETY: `flock` on an owned fd; EWOULDBLOCK means the
            // lock is taken and we simply return the error.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                return Err(std::io::Error::new(
                    err.kind(),
                    format!("state dir already owned by another process ({err})"),
                ));
            }
            Ok(Self { _file: file })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(std::io::Error::other(
                "state-dir locking requires unix (refusing to run without a single-owner guarantee)",
            ))
        }
    }
}

/// Which process a pid belongs to, as far as the platform can tell.
/// `Some(false)` proves the pid is NOT our binary (pid reuse — refuse
/// to signal it). `None` means the platform gives no answer (residual
/// pid-reuse risk, reported by the caller).
pub fn pid_is_our_binary(pid: u32) -> Option<bool> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let path = pid_binary_path(pid)?;
    let our = std::env::current_exe().ok()?;
    if path == our {
        return Some(true);
    }
    // A different path can still be our binary (copied/upgraded);
    // compare the file name as a fallback.
    let same_name = path.file_name().is_some() && path.file_name() == our.file_name();
    Some(same_name)
}

/// Executable path behind a pid, when the platform exposes it.
#[cfg(target_os = "macos")]
fn pid_binary_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: `proc_pidpath` fills a caller-owned buffer for a pid we
    // only read; it writes at most `size` bytes and returns 0 on
    // failure. No pointers outlive the call.
    unsafe {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let n = libc::proc_pidpath(
            pid as libc::pid_t,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len() as u32,
        );
        if n <= 0 {
            return None;
        }
        use std::os::unix::ffi::OsStrExt;
        let bytes = std::ffi::CStr::from_bytes_until_nul(&buf).ok()?.to_bytes();
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
}

/// Executable path behind a pid, when the platform exposes it.
#[cfg(target_os = "linux")]
fn pid_binary_path(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Platforms without a pid→path facility: unprovable.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn pid_binary_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

/// Register THIS process in the pid file so `status`, `sell`, `stop`,
/// and `upgrade` find a foreground `run` the same way they find the
/// daemon (AUD-001). Best-effort: a failed write is warned, and the
/// state-dir lock still guarantees single ownership.
pub fn register_running(live: bool, pid_file: &Path) {
    let info = PidInfo {
        pid: std::process::id(),
        live,
        started_at: unix_secs(),
    };
    if let Err(e) = write_private_file(pid_file, info.to_json().as_bytes()) {
        eprintln!("warning: pid file write failed: {e}");
    }
}

/// Remove the pid file when it names this process (clean shutdown of
/// a self-registered run). Never touches another process's record.
pub fn unregister_running(pid_file: &Path) {
    match read_pid_file(pid_file) {
        Some(info) if info.pid == std::process::id() => {
            let _ = std::fs::remove_file(pid_file);
        }
        _ => {}
    }
}

/// Stale claim age (older is stealable).
pub const START_LOCK_STALE_SECS: u64 = 120;

/// Claim the start lock. Stale claims are stolen; one racer wins.
pub fn try_acquire_start_lock(lock_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let claim = format!("{}:{}", std::process::id(), unix_secs());
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(claim.as_bytes())?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Garbage is stealable.
            let stale = std::fs::read_to_string(lock_path)
                .ok()
                .and_then(|raw| {
                    let (pid, ts) = raw.trim().split_once(':')?;
                    Some((pid.parse::<u32>().ok()?, ts.parse::<u64>().ok()?))
                })
                .is_none_or(|(pid, ts)| {
                    !process_alive(pid) || unix_secs().saturating_sub(ts) >= START_LOCK_STALE_SECS
                });
            if !stale {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "start already in progress",
                ));
            }
            let _ = std::fs::remove_file(lock_path);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock_path)?;
            use std::io::Write;
            f.write_all(claim.as_bytes())?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Quote one shell word.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Unix seconds (0 on broken clock). Single source: warn-once semantics
/// live in `crate::settings::now_secs`; this alias keeps call sites stable.
pub fn unix_secs() -> u64 {
    crate::settings::now_secs()
}

/// Append one log line (best-effort).
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

/// Last `n` lines. Bounded: seeks back in 64 KiB windows.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let len = file.metadata()?.len();
    const WINDOW: u64 = 65536;
    let mut pos = len;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let take = WINDOW.min(pos);
        pos -= take;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; take as usize];
        file.read_exact(&mut chunk)?;
        let mut next = Vec::with_capacity(chunk.len() + buf.len());
        next.extend_from_slice(&chunk);
        next.extend_from_slice(&buf);
        buf = next;
        let newlines = buf.iter().filter(|b| **b == b'\n').count();
        if newlines > n {
            let cut = newlines - n;
            let mut seen = 0usize;
            if let Some(at) = buf.iter().position(|b| {
                if *b == b'\n' {
                    seen += 1;
                }
                seen >= cut
            }) {
                buf.drain(..=at);
            }
            break;
        }
        if pos == 0 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > n {
        lines.drain(..lines.len() - n);
    }
    Ok(lines)
}

/// Open the daemon log for append, owner-only on unix.
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

/// Point stdout/stderr at a log file (unix `dup2`).
#[cfg(unix)]
pub fn redirect_stdout_stderr_to(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    for target in [1, 2] {
        loop {
            // SAFETY: `fd` is open; retried on EINTR.
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

/// First shutdown request.
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

/// Shutdown snapshot: what was open at the signal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ShutdownSummary {
    pub ts: u64,
    pub pid: u32,
    pub live: bool,
    pub paper_open: Vec<String>,
    pub live_tracked: Vec<String>,
    pub watchlist_pending: usize,
    pub clean: bool,
}

impl ShutdownSummary {
    /// Serialize for the shutdown card. Plain scalars: cannot fail.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("shutdown summary serializes")
    }
}

/// `start`: refuse when running, else spawn detached `run`.
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
    // Clear stale pid files for a clean `status`.
    if pid_file.exists() {
        let _ = std::fs::remove_file(&pid_file);
    }
    let lock_path = start_lock_path(&pid_file);
    if let Err(e) = try_acquire_start_lock(&lock_path) {
        eprintln!("another start is already in progress ({e}); refusing to spawn a second daemon");
        std::process::exit(1);
    }
    // Rotate a huge log before the child inherits the fd.
    rotate_log_if_huge(&log_file);
    // Owner-only log from the first byte.
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
            // Health gate: bad config exits within seconds.
            let grace_secs = if live { 8 } else { 3 };
            std::thread::sleep(std::time::Duration::from_secs(grace_secs));
            if !process_alive(pid) {
                let _ = std::fs::remove_file(&pid_file);
                let _ = std::fs::remove_file(&lock_path);
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
            let _ = std::fs::remove_file(&lock_path);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            let _ = std::fs::remove_file(&lock_path);
            std::process::exit(1);
        }
    }
}

/// `stop`: SIGTERM, wait, SIGKILL on timeout.
pub fn cmd_stop() -> ! {
    let code = stop_daemon();
    std::process::exit(code);
}

/// Shared stop body so `restart` reuses it.
pub fn stop_daemon() -> i32 {
    let (pid_file, log_file) = resolve_paths();
    let Some(info) = read_pid_file(&pid_file) else {
        println!("bot is not running (no pid file).");
        return 0;
    };
    if !process_alive(info.pid) {
        let _ = std::fs::remove_file(&pid_file);
        println!(
            "bot is not running (pid {} is dead; pid file cleaned).",
            info.pid
        );
        return 0;
    }
    // Pid reuse guard: a recycled pid must never take a SIGKILL meant
    // for the trader. Where the platform can resolve the binary we
    // refuse on mismatch; where it cannot, the residual risk is named
    // instead of silently assumed away (AUD-001).
    match pid_is_our_binary(info.pid) {
        Some(false) => {
            eprintln!(
                "refusing to signal pid {}: it is not pump-sentry (binary path mismatch; possible pid reuse). Check `status` and the pid file.",
                info.pid
            );
            return 1;
        }
        Some(true) => {}
        None => {
            eprintln!(
                "warning: cannot verify pid {} is pump-sentry on this platform; a reused pid could be signalled instead (residual risk)",
                info.pid
            );
        }
    }
    println!("stopping bot (pid {})...", info.pid);
    #[cfg(unix)]
    {
        // SAFETY: plain signal send; absence handled by the poll loop.
        unsafe {
            libc::kill(info.pid as libc::pid_t, libc::SIGTERM);
        }
        let rounds = STOP_GRACE_SECS * 1000 / STOP_POLL_MS;
        for _ in 0..rounds {
            if !process_alive(info.pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(STOP_POLL_MS));
        }
        if process_alive(info.pid) {
            eprintln!("daemon did not stop within {STOP_GRACE_SECS}s; SIGKILL...");
            unsafe {
                libc::kill(info.pid as libc::pid_t, libc::SIGKILL);
            }
            std::thread::sleep(std::time::Duration::from_millis(SIGKILL_SETTLE_MS));
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("stop is only fully supported on unix; remove the pid file manually.");
    }
    let stopped = !process_alive(info.pid);
    // Keep the pid file when the daemon survived.
    if stopped {
        let _ = std::fs::remove_file(&pid_file);
    }
    append_log_line(
        &log_file,
        &format!(
            "=== bot stopped at unix_ts={} (pid {}) ===",
            unix_secs(),
            info.pid
        ),
    );
    if stopped {
        println!("bot stopped.");
        0
    } else {
        eprintln!("failed to stop pid {}; inspect manually.", info.pid);
        1
    }
}

/// `restart`: best-effort stop, then start.
pub fn cmd_restart(live: bool) -> ! {
    let code = stop_daemon();
    if code != 0 {
        eprintln!("stop failed; restart aborted.");
        std::process::exit(code);
    }
    std::thread::sleep(std::time::Duration::from_millis(RESTART_PAUSE_MS));
    cmd_start(live, "restart");
}

/// `status`: one line on liveness.
pub fn cmd_status() -> ! {
    let (pid_file, log_file) = resolve_paths();
    let paused_note = marker_present(&state_file_beside_log(&log_file, ENTRIES_PAUSED_FILE))
        .then(|| "entries PAUSED via kill switch; `pump-sentry resume` clears it".to_string());
    match running_daemon(&pid_file) {
        Some(info) => {
            println!(
                "running (pid {}, {} mode, started unix_ts={}, log {})",
                info.pid,
                if info.live { "live" } else { "paper" },
                info.started_at,
                log_file.display()
            );
            if let Some(note) = paused_note {
                println!("{note}");
            }
            std::process::exit(0);
        }
        None => {
            if let Some(stale) = read_pid_file(&pid_file) {
                let _ = std::fs::remove_file(&pid_file);
                println!(
                    "stopped (stale pid {} cleaned; log {})",
                    stale.pid,
                    log_file.display()
                );
            } else {
                println!("stopped (log {})", log_file.display());
            }
            if let Some(note) = paused_note {
                println!("{note}");
            }
            std::process::exit(1);
        }
    }
}

/// `logs`: print the tail, optionally follow.
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
                _ = tokio::time::sleep(std::time::Duration::from_millis(LOG_FOLLOW_POLL_MS)) => {},
            }
            let len = std::fs::metadata(&log_file).map(|m| m.len()).unwrap_or(pos);
            if len < pos {
                pos = 0;
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

/// Read from `pos` to end.
fn read_from(mut file: std::fs::File, pos: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(pos))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Log size cap: rotate at or above this.
pub const LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;
/// Archives kept (`log.1` newest).
pub const LOG_RETAIN_ARCHIVES: u32 = 3;

/// Rotation at (re)start (mid-run rotation is impossible).
fn rotate_log_if_huge(log: &Path) {
    rotate_log_sized(log, LOG_ROTATE_BYTES, LOG_RETAIN_ARCHIVES);
}

/// Rotation core with explicit budget.
fn rotate_log_sized(log: &Path, cap_bytes: u64, retain: u32) {
    let huge = std::fs::metadata(log).is_ok_and(|m| m.len() >= cap_bytes);
    if !huge {
        return;
    }
    let archive = |i: u32| log.with_extension(format!("log.{i}"));
    if retain == 0 {
        if std::fs::remove_file(log).is_err() {
            eprintln!("warning: oversized log {} unrotatable", log.display());
        }
        return;
    }
    if std::fs::remove_file(archive(retain)).is_err() {
        // Missing oldest is fine.
    }
    for i in (1..retain).rev() {
        let _ = std::fs::rename(archive(i), archive(i + 1));
    }
    if std::fs::rename(log, archive(1)).is_err() {
        eprintln!("warning: oversized log {} unrotatable", log.display());
    }
}

/// Write a file owner-only (0600 on unix).
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
            eprintln!("warning: cannot restrict {} to owner-only", path.display());
        }
    }
    Ok(())
}

/// Request path next to the log (`sell-<mint>.json`).
pub fn sell_request_path(log_file: &Path, mint: &str) -> PathBuf {
    state_file_beside_log(log_file, &format!("sell-{mint}.json"))
}

/// Owner-only atomic write: staged temp beside the target, then
/// rename. A failed write removes the staging file and leaves any
/// existing target untouched (state files are never half-written).
/// The staged file is fsynced before the rename (hard error
/// otherwise: its content must be durable), and the parent directory
/// is fsynced best-effort afterwards so the rename itself survives a
/// host crash — a lost rename only resurrects the previous complete
/// file (every reader handles that), never a torn one.
pub fn atomic_write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("part");
    let staged = write_private_file(&tmp, contents)
        .and_then(|()| std::fs::File::open(&tmp))
        .and_then(|f| f.sync_all().map(|()| f))
        .and_then(|_| std::fs::rename(&tmp, path));
    if staged.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return staged;
    }
    // Best-effort: a failed dir sync keeps the complete file (rename
    // is atomic) and only risks resurrecting the previous complete
    // file after a host crash — loud, never silent.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Ok(dir) = std::fs::File::open(parent)
        && let Err(e) = dir.sync_all()
    {
        eprintln!("warning: cannot fsync state dir {}: {e}", parent.display());
    }
    staged
}

/// Pending sell requests, sorted by mint. A missing state dir is empty
/// (nothing written yet); any other read failure is loud — a broken
/// path must never look like "no requests". The dir comes from
/// [`state_dir_for_log`], so a bare relative `LOG_FILE` (parent `""`)
/// reads the CWD exactly where `cmd_sell` writes (AUD-012).
pub fn pending_sell_requests(log_file: &Path) -> Vec<(String, PathBuf)> {
    let dir = state_dir_for_log(log_file);
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|entry| match entry {
                Ok(e) => Some(e.path()),
                Err(e) => {
                    eprintln!("warning: unreadable state-dir entry: {e}");
                    None
                }
            })
            .filter(|p| {
                p.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("sell-") && n.ends_with(".json")
                })
            })
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            eprintln!(
                "warning: cannot read state dir {} : {e}; no sell requests this round",
                dir.display()
            );
            Vec::new()
        }
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            let mint = name
                .strip_prefix("sell-")?
                .strip_suffix(".json")?
                .to_string();
            if mint.is_empty() {
                None
            } else {
                Some((mint, p))
            }
        })
        .collect();
    out.sort();
    out
}

/// `sell <mint>`: queue a manual exit. Needs a running daemon.
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
    if let Err(e) = atomic_write_private(&path, body.as_bytes()) {
        eprintln!("cannot queue sell request {}: {e}", path.display());
        std::process::exit(1);
    }
    println!("manual sell queued for {mint} (daemon drains it on its next round)");
}

/// Kill switch on: write the marker. New entries halt within one gate
/// round; open positions, exits, and `sell` are unaffected, and the
/// marker survives restarts until `resume`.
pub fn cmd_pause() -> ! {
    let path = entries_paused_path();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = format!("paused by `pump-sentry pause` at unix_ts={}\n", unix_secs());
    if let Err(e) = write_private_file(&path, body.as_bytes()) {
        eprintln!("cannot write kill-switch marker {}: {e}", path.display());
        std::process::exit(1);
    }
    println!(
        "entries paused (marker {}): no new position opens; open positions keep managing (exits, `sell <mint>`). Resume with `pump-sentry resume`.",
        path.display()
    );
    std::process::exit(0);
}

/// Kill switch off: remove the marker.
pub fn cmd_resume() -> ! {
    let path = entries_paused_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("entries resumed (removed {})", path.display());
            std::process::exit(0);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "entries were not paused (no {}); nothing to resume",
                path.display()
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("cannot remove kill-switch marker {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// Mint in a sell-request body. `None` on mismatch — including a `mint`
/// that is not a base58 pubkey (same check as `cmd_sell`, so a queued
/// `"ABC"` can never match a file name the daemon would honor).
pub fn sell_request_body_mint(body: &str) -> Option<String> {
    /// `cmd_sell`'s request body (`{"mint": "...", "ts": ...}`).
    #[derive(serde::Deserialize)]
    struct SellRequest {
        mint: String,
    }
    let request: SellRequest = serde_json::from_str(body).ok()?;
    if request.mint.is_empty() || executor::parse_pubkey(&request.mint).is_err() {
        return None;
    }
    Some(request.mint)
}

/// Release repo mirrored from `install.sh`.
pub const RELEASE_REPO: &str = "sembraniteam/pump-sentry";
/// `releases/latest` endpoint (same source `install.sh` reads).
pub const RELEASE_API_URL: &str =
    "https://api.github.com/repos/sembraniteam/pump-sentry/releases/latest";

/// Parsed `vX.Y.Z` (leading `v` optional, missing parts read as 0).
/// Derived `Ord` compares numerically, never lexicographically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleaseVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl ReleaseVersion {
    /// This binary's version (`Cargo.toml`).
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
    }

    /// Parse `v1.2.3` / `1.2`; leading/trailing whitespace tolerated.
    /// `None` on non-numeric parts (a bad tag never compares equal).
    pub fn parse(tag: &str) -> Option<Self> {
        let t = tag.trim().strip_prefix('v').unwrap_or(tag.trim());
        let mut parts = t.split('.');
        let num = |p: Option<&str>| -> Option<u64> {
            match p {
                None => Some(0),
                Some(s) if s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty() => s.parse().ok(),
                _ => None,
            }
        };
        let v = Self {
            major: num(parts.next())?,
            minor: num(parts.next())?,
            patch: num(parts.next())?,
        };
        // No fourth part: `1.2.3.4` is not our scheme.
        if parts.next().is_some() {
            return None;
        }
        Some(v)
    }

    /// Canonical tag (`vX.Y.Z`).
    pub fn tag(&self) -> String {
        format!("v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Release asset for this platform (`app-os-arch.tar.gz`, mirroring
/// `install.sh`). `None` on unsupported OS/arch — refusing beats
/// downloading the wrong binary.
pub fn release_asset_name() -> Option<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("pump-sentry-{os}-{arch}.tar.gz"))
}

/// `tag_name` out of a `releases/latest` JSON body. `None` when the
/// shape is wrong (no releases, API error page) — never guess a tag.
pub fn latest_tag_from_json(body: &str) -> Option<String> {
    /// `releases/latest` body: only the tag matters here.
    #[derive(serde::Deserialize)]
    struct ReleaseLatest {
        #[serde(default)]
        tag_name: Option<String>,
    }
    serde_json::from_str::<ReleaseLatest>(body).ok()?.tag_name
}

/// GET bytes via `curl` (same dependency `install.sh` requires).
fn curl_fetch(url: &str) -> std::io::Result<Vec<u8>> {
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", url])
        .output()
        .map_err(|e| {
            std::io::Error::other(format!(
                "need 'curl' for upgrade ({e}); install.sh requires it too"
            ))
        })?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "download failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// `upgrade`: install the newest release over this binary, else say so.
/// Refuses while the daemon runs (replacing a live trader's binary is
/// never safe) and on unsupported platforms.
pub fn cmd_upgrade(allow_unverified: bool) -> ! {
    let current = ReleaseVersion::current();
    let body = match curl_fetch(RELEASE_API_URL) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => {
            eprintln!("cannot reach release info (network/API down?)");
            std::process::exit(1);
        }
    };
    let tag = match latest_tag_from_json(&body) {
        Some(t) => t,
        None => {
            eprintln!("no releases published yet");
            std::process::exit(1);
        }
    };
    let latest = match ReleaseVersion::parse(&tag) {
        Some(v) => v,
        None => {
            eprintln!("latest tag '{tag}' is not a version; refusing to guess");
            std::process::exit(1);
        }
    };
    if latest <= current {
        if latest == current {
            println!("already on the latest version ({})", current.tag());
        } else {
            println!(
                "this build ({}) is ahead of the latest release ({}); nothing to do",
                current.tag(),
                latest.tag()
            );
        }
        std::process::exit(0);
    }
    let (pid_file, _) = resolve_paths();
    if let Some(info) = running_daemon(&pid_file) {
        eprintln!(
            "bot is running (pid {}); stop it first (`stop`), then upgrade",
            info.pid
        );
        std::process::exit(1);
    }
    let asset = match release_asset_name() {
        Some(a) => a,
        None => {
            eprintln!(
                "unsupported platform {}-{}; install from source",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            std::process::exit(1);
        }
    };
    let base = format!("https://github.com/{RELEASE_REPO}/releases/download/{tag}");
    println!("upgrading {} → {} ...", current.tag(), latest.tag());
    let bytes = match curl_fetch(&format!("{base}/{asset}")) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("download failed: {e} (no asset for this platform yet?)");
            std::process::exit(1);
        }
    };
    let sha = curl_fetch(&format!("{base}/{asset}.sha256")).ok();
    let dir = std::env::temp_dir().join(format!("pump-upgrade-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot stage upgrade: {e}");
        std::process::exit(1);
    }
    let tgz = dir.join(&asset);
    if let Err(e) = std::fs::write(&tgz, &bytes) {
        eprintln!("cannot stage upgrade: {e}");
        std::process::exit(1);
    }
    // Verified installs only by default: an unverified binary
    // replaces the running bot, so `--allow-unverified` must be an
    // explicit operator override, never silent acceptance.
    let verified = match sha.as_deref() {
        Some(body) => {
            let want = String::from_utf8_lossy(body)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
                eprintln!("checksum file malformed; cannot verify");
                false
            } else {
                match sha256_of_file(&tgz) {
                    Some(have) if have != want => {
                        eprintln!("checksum mismatch for {asset}; refusing to install");
                        let _ = std::fs::remove_dir_all(&dir);
                        std::process::exit(1);
                    }
                    Some(_) => true,
                    None => {
                        eprintln!("no sha256 tool; cannot verify");
                        false
                    }
                }
            }
        }
        None => {
            eprintln!("no checksum file published; cannot verify");
            false
        }
    };
    if !verified && !allow_unverified {
        eprintln!(
            "refusing to install unverified binary (retry with --allow-unverified to override)"
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::process::exit(1);
    }
    if !verified {
        eprintln!("warning: installing UNVERIFIED binary (--allow-unverified)");
    }
    match std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tgz)
        .arg("-C")
        .arg(&dir)
        .status()
    {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("extract failed (need 'tar')");
            let _ = std::fs::remove_dir_all(&dir);
            std::process::exit(1);
        }
    }
    let staged = dir.join("pump-sentry");
    if !staged.is_file() {
        eprintln!("archive has no pump-sentry binary");
        let _ = std::fs::remove_dir_all(&dir);
        std::process::exit(1);
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot locate this binary: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            std::process::exit(1);
        }
    };
    if let Err(e) = replace_binary(&staged, &exe) {
        eprintln!("cannot replace {}: {e}", exe.display());
        let _ = std::fs::remove_dir_all(&dir);
        std::process::exit(1);
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "upgraded {} → {} (restart the bot to run it)",
        current.tag(),
        latest.tag()
    );
    std::process::exit(0);
}

/// Atomically replace the running binary. `fs::copy` over `exe`
/// fails with ETXTBSY on unix because the `upgrade` process itself
/// executes from that file — truncating a running executable is
/// refused. Staging beside it + `rename` instead only swaps the
/// directory entry; the running process keeps its old inode and
/// exits normally. Same-filesystem staging is required (rename does
/// not cross devices), hence the `.new` sibling, cleaned on failure.
fn replace_binary(staged: &Path, exe: &Path) -> std::io::Result<()> {
    let tmp = exe.with_extension("new");
    // Stale sibling from an interrupted upgrade must not be trusted.
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(staged, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Err(e) = std::fs::rename(&tmp, exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// SHA-256 hex of a file via system tooling (`sha256sum`, else
/// `shasum`), mirroring `install.sh`. `None` when neither exists.
/// A missing tool skips to the next candidate (AUD-019: the old
/// `?`-propagation made the `shasum` fallback unreachable whenever
/// `sha256sum` was absent, pushing operators toward
/// `--allow-unverified` on stock macOS).
fn sha256_of_file(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    sha256_via_tools(move |tool, args| {
        std::process::Command::new(tool)
            .args(args)
            .arg(&path)
            .output()
            .ok()
    })
}

/// Tool-selection core (pure via the injected runner): try each tool
/// in order; a missing/unspawnable tool (`None`) tries the next, a
/// failed run retries the next, and only a 64-hex stdout digest wins.
fn sha256_via_tools(
    mut run: impl FnMut(&str, &[&str]) -> Option<std::process::Output>,
) -> Option<String> {
    for tool in [["sha256sum"].as_slice(), ["shasum", "-a", "256"].as_slice()] {
        let Some(out) = run(tool[0], &tool[1..]) else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let hex = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(hex);
        }
    }
    None
}

/// Spawn detached `run`, returning the daemon pid.
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
        assert_eq!(parse_cli(&argv(&["bot", "pause"])), CliAction::Pause);
        assert_eq!(parse_cli(&argv(&["bot", "resume"])), CliAction::Resume);
        assert_eq!(
            parse_cli(&argv(&["bot", "restart", "--live"])),
            CliAction::Restart { live: true }
        );
        assert_eq!(parse_cli(&argv(&["bot", "status"])), CliAction::Status);
        assert_eq!(
            parse_cli(&argv(&["bot", "init-config"])),
            CliAction::InitConfig
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "upgrade"])),
            CliAction::Upgrade {
                allow_unverified: false
            }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "upgrade", "--allow-unverified"])),
            CliAction::Upgrade {
                allow_unverified: true
            }
        );
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
            CliAction::Sell {
                mint: "MINT".to_string()
            }
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
    fn state_files_sit_beside_the_log() {
        assert_eq!(
            state_file_beside_log(Path::new("/tmp/ps/bot.log"), ENTRIES_PAUSED_FILE),
            PathBuf::from("/tmp/ps/entries-paused")
        );
        assert_eq!(
            state_file_beside_log(Path::new("/tmp/ps/bot.log"), "day-state.json"),
            PathBuf::from("/tmp/ps/day-state.json")
        );
        // Bare filename: the state file stays bare too.
        assert_eq!(
            state_file_beside_log(Path::new("bot.log"), ENTRIES_PAUSED_FILE),
            PathBuf::from(ENTRIES_PAUSED_FILE)
        );
    }

    #[test]
    fn marker_present_fails_closed_and_atomic_write_stages() {
        let dir = std::env::temp_dir().join(format!("pump-cli-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let marker = dir.join(ENTRIES_PAUSED_FILE);
        assert!(!marker_present(&marker), "no marker: not paused");
        write_private_file(&marker, b"paused\n").expect("write marker");
        assert!(marker_present(&marker), "marker present");
        // An undecidable stat (a path whose parent is a file) counts
        // as present: a marker we cannot rule out must halt entries.
        let file = dir.join("not-a-dir");
        write_private_file(&file, b"").expect("write file");
        assert!(
            marker_present(&file.join(ENTRIES_PAUSED_FILE)),
            "unstatable marker fails closed"
        );

        // Atomic write: target lands with the staged bytes, the staging
        // file is gone, and no half-written target can exist.
        let target = dir.join("day-state.json");
        atomic_write_private(&target, b"{\"day\":1}").expect("atomic write");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read back"),
            "{\"day\":1}"
        );
        assert!(
            !target.with_extension("part").exists(),
            "staging file retired"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(
            parse_cli(&argv(&["bot", "logs", "--lines=7"])),
            CliAction::Logs {
                lines: 7,
                follow: false
            }
        );
        assert_eq!(
            parse_cli(&argv(&["bot", "logs", "-n42"])),
            CliAction::Logs {
                lines: 42,
                follow: false
            }
        );
    }

    #[test]
    fn release_versions_parse_and_compare_numerically() {
        assert_eq!(
            ReleaseVersion::parse("v1.2.3"),
            Some(ReleaseVersion {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            ReleaseVersion::parse("1.2"),
            Some(ReleaseVersion {
                major: 1,
                minor: 2,
                patch: 0
            })
        );
        assert_eq!(
            ReleaseVersion::parse("  v0.2.0  ").map(|v| v.tag()),
            Some("v0.2.0".to_string())
        );
        // Numeric, never lexicographic: 1.10 beats 1.9.
        assert!(
            ReleaseVersion::parse("v1.10.0").unwrap() > ReleaseVersion::parse("v1.9.0").unwrap()
        );
        for bad in ["", "v", "latest", "v1.x.3", "1.2.3.4", "v-1.2.3"] {
            assert_eq!(ReleaseVersion::parse(bad), None, "{bad}");
        }
        assert_eq!(
            ReleaseVersion::current().tag(),
            format!("v{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn release_asset_matches_installer_naming() {
        let asset = release_asset_name().expect("tier-1 platform");
        assert!(asset.starts_with("pump-sentry-"), "{asset}");
        assert!(asset.ends_with(".tar.gz"), "{asset}");
    }

    #[test]
    fn replace_binary_swaps_contents_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "cli-replace-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let exe = dir.join("pump-sentry");
        let staged = dir.join("staged");
        std::fs::write(&exe, b"v0").expect("write");
        std::fs::write(&staged, b"v1").expect("write");
        replace_binary(&staged, &exe).expect("replace");
        assert_eq!(std::fs::read(&exe).expect("read"), b"v1");
        assert!(
            !exe.with_extension("new").exists(),
            "no staging sibling left behind"
        );
        // Missing staged source surfaces an error, never a half-write.
        assert!(replace_binary(&dir.join("absent"), &exe).is_err());
        assert_eq!(std::fs::read(&exe).expect("read"), b"v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_tag_comes_from_tag_name_only() {
        let body = r#"{"tag_name":"v0.3.0","name":"x","body":"... v9.9.9 ..."}"#;
        assert_eq!(latest_tag_from_json(body), Some("v0.3.0".to_string()));
        assert_eq!(latest_tag_from_json("{}"), None);
        assert_eq!(latest_tag_from_json("not json"), None);
        assert_eq!(latest_tag_from_json(r#"{"tag_name":42}"#), None);
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
        let _guard = crate::test_env::scoped(&[
            ("HOME", Some("/tmp/fakehome")),
            ("PID_FILE", None),
            ("LOG_FILE", None),
        ]);
        let (pid, log) = resolve_paths();
        assert_eq!(pid, PathBuf::from("/tmp/fakehome/.pump-sentry/bot.pid"));
        assert_eq!(log, PathBuf::from("/tmp/fakehome/.pump-sentry/bot.log"));
        unsafe { std::env::set_var("PID_FILE", "/tmp/x.pid") };
        unsafe { std::env::set_var("LOG_FILE", "~/custom/bot.log") };
        let (pid, log) = resolve_paths();
        assert_eq!(pid, PathBuf::from("/tmp/x.pid"));
        assert_eq!(log, PathBuf::from("/tmp/fakehome/custom/bot.log"));
    }

    #[test]
    fn start_lock_blocks_live_steals_stale() {
        let dir = std::env::temp_dir().join(format!(
            "cli-lock-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let lock = dir.join("bot.start.lock");
        try_acquire_start_lock(&lock).expect("acquire");
        assert!(try_acquire_start_lock(&lock).is_err());
        std::fs::write(&lock, format!("{}:{}", u32::MAX, unix_secs())).expect("write");
        try_acquire_start_lock(&lock).expect("steal dead");
        std::fs::write(&lock, "garbage").expect("write");
        try_acquire_start_lock(&lock).expect("steal garbage");
        std::fs::write(&lock, format!("{}:1", std::process::id())).expect("write");
        try_acquire_start_lock(&lock).expect("steal ancient");
        assert_eq!(
            start_lock_path(&dir.join("bot.pid")),
            dir.join("bot.start.lock")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_lock_excludes_second_holder_and_reacquires_after_drop() {
        // AUD-001 core primitive: one holder at a time on one path,
        // released on drop. This is the same `flock` the trader
        // process holds for its whole lifetime.
        // NOTE: the dir embeds an atomic counter so parallel shards of
        // this test never share a lock file, and the reacquire polls
        // briefly: under parallel load the OS can report the just-
        // released flock as still held for a beat after `close`.
        static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = CTR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "cli-state-lock-test-{}-{:?}-{n}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let lock_path = dir.join("bot.lock");
        let first = StateDirLock::try_acquire(&lock_path).expect("first holder");
        let second = StateDirLock::try_acquire(&lock_path);
        assert!(
            second.is_err(),
            "a second holder must fail while the first holds"
        );
        let err = second.expect_err("checked above");
        assert!(err.to_string().contains("state dir already owned"), "{err}");
        drop(first);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let third = loop {
            match StateDirLock::try_acquire(&lock_path) {
                Ok(g) => break g,
                Err(e) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let _ = e;
                    continue;
                }
                Err(e) => panic!("reacquire after drop: {e}"),
            }
        };
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_lock_sits_beside_the_log() {
        // The lock is keyed on the STATE dir (the log's directory), so
        // a second process with an overridden PID_FILE but the same
        // LOG_FILE still collides (AUD-001 trigger).
        assert_eq!(
            state_lock_path(Path::new("/tmp/ps/bot.log")),
            PathBuf::from("/tmp/ps/bot.lock")
        );
        // Bare filename: the CWD is the state dir.
        assert_eq!(
            state_lock_path(Path::new("bot.log")),
            PathBuf::from("./bot.lock")
        );
    }

    #[test]
    fn pid_registration_roundtrips_and_never_touches_foreign_records() {
        let dir = std::env::temp_dir().join(format!(
            "cli-pid-reg-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pid_file = dir.join("bot.pid");
        register_running(true, &pid_file);
        let info = read_pid_file(&pid_file).expect("registered");
        assert_eq!(info.pid, std::process::id());
        assert!(info.live);
        // A record naming another pid must survive our unregister.
        std::fs::write(&pid_file, r#"{"pid":999999,"live":false,"started_at":1}"#)
            .expect("foreign record");
        unregister_running(&pid_file);
        assert!(read_pid_file(&pid_file).is_some(), "foreign record kept");
        // Our own record goes.
        register_running(false, &pid_file);
        unregister_running(&pid_file);
        assert!(read_pid_file(&pid_file).is_none(), "own record removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_target_check_identifies_this_process() {
        // This test process: resolvable and matching its own binary.
        assert_eq!(pid_is_our_binary(std::process::id()), Some(true));
        assert_eq!(pid_is_our_binary(0), None);
        // An unrelated live process must read as NOT ours where the
        // platform can resolve binaries (`sleep` stays alive for the
        // probe; the child is reaped below).
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep");
        let verdict = pid_is_our_binary(child.id());
        if pid_binary_path(child.id()).is_some() {
            assert_eq!(verdict, Some(false), "sleep is not pump-sentry");
        } else {
            assert_eq!(verdict, None, "platform gives no answer");
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn bare_log_paths_deliver_sell_requests() {
        // AUD-012: `LOG_FILE=bot.log` has parent Some(""), which used
        // to make the drain see no directory at all. Request path and
        // drain dir must agree on the CWD. Chdir is process-global:
        // serialized like the env-mutating tests (AUD-022 guard).
        let _guard = crate::test_env::lock();
        assert_eq!(
            sell_request_path(Path::new("bot.log"), "MINT"),
            PathBuf::from("sell-MINT.json")
        );
        let dir = std::env::temp_dir().join(format!(
            "cli-bare-sell-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let log = dir.join("bot.log");
        std::fs::write(&log, "").expect("log");
        let mut cwd_guard = crate::test_env::chdir(&dir).expect("chdir");
        let request = sell_request_path(Path::new("bot.log"), "MINTA");
        write_private_file(&request, br#"{"mint":"MINTA","ts":1}"#).expect("queue");
        let found = pending_sell_requests(Path::new("bot.log"));
        cwd_guard.restore().expect("restore cwd");
        assert_eq!(found.len(), 1, "bare-path request must be delivered");
        assert_eq!(found[0].0, "MINTA");
        // Absolute paths still work.
        let abs = sell_request_path(&log, "MINTB");
        write_private_file(&abs, br#"{"mint":"MINTB","ts":1}"#).expect("queue");
        let found = pending_sell_requests(&log);
        assert_eq!(found.len(), 2, "both queued requests are seen");
        assert!(
            found.iter().any(|(m, _)| m == "MINTB"),
            "absolute-path request is delivered"
        );
        assert!(
            found.iter().any(|(m, _)| m == "MINTA"),
            "bare-path request stays discoverable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_falls_back_when_first_tool_is_absent() {
        // AUD-019: sha256sum missing must reach the shasum fallback,
        // not return None from the whole function.
        let tmp = std::env::temp_dir().join(format!(
            "cli-sha-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let file = tmp.join("blob");
        std::fs::write(&file, b"payload").expect("write");
        let digest = "a".repeat(64);
        let got = sha256_via_tools(|tool, _args| {
            if tool == "sha256sum" {
                None // tool not installed
            } else {
                Some(std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: format!("{digest}  {file:?}\n").into_bytes(),
                    stderr: Vec::new(),
                })
            }
        });
        assert_eq!(got.as_deref(), Some(digest.as_str()));
        // Both absent: None.
        assert_eq!(
            sha256_via_tools(|_tool, _args| None),
            None,
            "no tool anywhere stays None"
        );
        // A failed run falls through to the next tool.
        let mut first = true;
        let got = sha256_via_tools(|tool, _args| {
            if first {
                first = false;
                return None;
            }
            assert_eq!(tool, "shasum", "fallback order");
            Some(std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: b"zz-not-hex\n".to_vec(),
                stderr: Vec::new(),
            })
        });
        assert_eq!(got, None, "non-hex output is rejected");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sha256_uses_real_shasum_when_sha256sum_absent_from_path() {
        // End-to-end shape of AUD-019 with a PATH that only knows a
        // fake `shasum`: the child PATH is overridden (never the
        // process env), so `sha256sum` lookup fails and `shasum` runs.
        let tmp = std::env::temp_dir().join(format!(
            "cli-sha-path-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let digest = "b".repeat(64);
        let fake = tmp.join("bin");
        std::fs::create_dir_all(&fake).expect("mkdir");
        let script = fake.join("shasum");
        std::fs::write(&script, format!("#!/bin/sh\necho {digest}\n")).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let file = tmp.join("blob");
        std::fs::write(&file, b"payload").expect("write");
        let fake_path = fake.clone();
        let got = sha256_via_tools(move |tool, args| {
            std::process::Command::new(tool)
                .args(args)
                .arg(&file)
                .env("PATH", &fake_path)
                .output()
                .ok()
        });
        assert_eq!(got.as_deref(), Some(digest.as_str()));
        let _ = std::fs::remove_dir_all(&tmp);
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
    fn tail_lines_tolerates_non_utf8() {
        let path = std::env::temp_dir().join(format!(
            "cli-tail-bin-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, [b'a', b'\n', 0xff, b'\n', b'z', b'\n']).expect("write");
        let tail = tail_lines(&path, 3).expect("tail");
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0], "a");
        assert!(tail[1].contains('\u{FFFD}'), "lossy: {tail:?}");
        assert_eq!(tail[2], "z");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn current_process_is_alive_but_absurd_pid_is_not() {
        assert!(process_alive(std::process::id()));
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
        // A real 32-byte base58 pubkey (WSOL mint). Short strings like
        // "ABC" decode as bytes but are not pubkeys — must reject.
        let mint = ids::WSOL_MINT.to_string();
        assert_eq!(
            sell_request_body_mint(&format!(r#"{{"mint":"{mint}","ts":1}}"#)),
            Some(mint.to_string())
        );
        assert_eq!(
            sell_request_body_mint(&format!(r#"{{"mint": "{mint}", "ts": 1}}"#)),
            Some(mint.to_string())
        );
        assert_eq!(
            sell_request_body_mint(&format!("{{\n  \"mint\" : \"{mint}\",\n  \"ts\" : 1\n}}")),
            Some(mint.to_string())
        );
        assert_eq!(
            sell_request_body_mint(&format!(r#"{{"ts":1,"mint":"{mint}"}}"#)),
            Some(mint.to_string())
        );
        assert_eq!(sell_request_body_mint(r#"{"mint":"ABC","ts":1}"#), None);
        assert_eq!(sell_request_body_mint(""), None);
        assert_eq!(sell_request_body_mint("not json"), None);
        assert_eq!(sell_request_body_mint(r#"{"mint":"","ts":1}"#), None);
        assert_eq!(sell_request_body_mint(r#"{"ts":1}"#), None);
        assert_eq!(sell_request_body_mint(r#"{"mint":123,"ts":1}"#), None);
    }

    #[test]
    fn sell_request_roundtrip() {
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
            "start",
            "stop",
            "restart",
            "status",
            "logs",
            "run",
            "sell",
            "init-config",
            "upgrade",
            "help",
            "--live",
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
        std::fs::write(&log, vec![0u8; 100]).expect("write");
        rotate_log_sized(&log, 1_000, 3);
        assert_eq!(std::fs::metadata(&log).expect("meta").len(), 100);
        std::fs::write(log.with_extension("log.1"), vec![1u8; 10]).expect("write");
        std::fs::write(&log, vec![0u8; 1_000]).expect("write");
        rotate_log_sized(&log, 1_000, 2);
        assert!(std::fs::metadata(&log).is_err(), "current must move away");
        assert_eq!(
            std::fs::metadata(log.with_extension("log.1"))
                .expect("m")
                .len(),
            1_000
        );
        assert_eq!(
            std::fs::metadata(log.with_extension("log.2"))
                .expect("m")
                .len(),
            10
        );
        std::fs::write(&log, vec![0u8; 1_000]).expect("write");
        rotate_log_sized(&log, 1_000, 2);
        assert!(
            std::fs::metadata(log.with_extension("log.3")).is_err(),
            "no third archive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
