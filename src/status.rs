//! Sidecar status file (docs/discord-sidecar.md §2).
//!
//! The Discord sidecar is a separate process that answers slash
//! commands from on-disk state, never from the trading loop. Its
//! `/status` and `/heartbeat` answers need readings that only exist in
//! the loop's memory (listener counters, breaker state, marked
//! equity), so the bot publishes them here: one atomic, 0600 JSON
//! file every [`STATUS_WRITE_INTERVAL_SECS`], overwritten in place —
//! a trailing status file, never a control channel. The sidecar only
//! ever reads it; lag is visible to operators as the `ts` field.
//!
//! Writes are best-effort: a failure logs and the sidecar serves the
//! last complete file. Trading is unaffected by construction.

use std::path::Path;

use alert::HeartbeatReport;

/// Status file name beside the log (same dir as WAL / day-state).
pub const STATUS_FILE: &str = "status.json";

/// Write cadence. Everything the report carries is a cached,
/// in-memory read (marks come from the tick cache), so this cadence
/// adds no RPC load and keeps sidecar lag at seconds.
pub const STATUS_WRITE_INTERVAL_SECS: u64 = 10;

/// The on-disk payload: collection timestamp plus the heartbeat
/// report verbatim (same struct the 6-hour card uses — one collector,
/// two consumers), plus the ledger-tier views the remaining Tier 1
/// commands answer from (`positions`, `pnl`, `watchlist`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusReport {
    /// Unix seconds at collection. The sidecar answers with the
    /// freshness (`age = now - ts`) alongside the data.
    pub ts: u64,
    /// Heartbeat readings, flattened (mode/strategy/counters/breaker…).
    #[serde(flatten)]
    pub report: HeartbeatReport,
    /// Paper PnL panel (`/pnl`).
    pub pnl: PnlReport,
    /// Open paper positions (`/positions`), capped at
    /// [`POSITIONS_MAX_PUBLISHED`]. Money at mark basis.
    pub positions: Vec<OpenPosition>,
    /// Live-held mints the paper book may not cover (unpaired bags).
    pub manual_mints: Vec<String>,
    /// Reconciling remotes counts (`/positions` watch-only lines).
    pub diverged_mints: Vec<String>,
    /// Mints with an unsettled buy awaiting confirmation.
    pub pending_buy_mints: Vec<String>,
    /// Watchlist (`/watchlist`): capped sample behind
    /// [`WATCHLIST_MAX_PUBLISHED`], the true total, and the gate's
    /// thresholds so progress reads against a target.
    pub watchlist: WatchlistSnapshot,
}

/// Bounded disclosure of a possibly huge list: the file stays finite
/// even when the gate window floods (gate cap is 10k), and the true
/// count ships beside it so nothing is quietly hidden.
pub const WATCHLIST_MAX_PUBLISHED: usize = 60;
pub const POSITIONS_MAX_PUBLISHED: usize = 60;

/// One open paper position as `/positions` shows it. Unpriced mint
/// (no fresh mark) carries `None` rather than a fiction; answer text
/// marks it `unmarked`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OpenPosition {
    pub id: u64,
    pub mint: String,
    pub entry_price: f64,
    pub quantity: u64,
    pub entry_notional: f64,
    pub opened_at: u64,
    /// Per-position exit horizon (secs) — the honest "stuck" line
    /// uses this, not a global timeout concept.
    pub timeout_seconds: u64,
    /// Fresh mark price when the tick cache has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark_price: Option<f64>,
    /// `quantity × mark − entry notional` (fresh mark basis, not the
    /// exit-impact realizable value — that is the `pnl` panel's job).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_usd: Option<f64>,
}

/// One watchlisted mint mid buyer-quorum: `/watchlist` lines.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WatchEntry {
    pub mint: String,
    /// Distinct non-dev buyers seen so far.
    pub buyers: u32,
    /// First submit (caps relaunch refresh).
    pub created_secs: u64,
    /// Quorum deadline.
    pub deadline_secs: u64,
}

/// `/watchlist` payload: capped entries plus the true total, with the
/// gate's thresholds so the progress lines read against a target
/// (`3/5 buyers of window 10s`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WatchlistSnapshot {
    pub total: usize,
    pub entries: Vec<WatchEntry>,
    pub min_buyers: u32,
    pub window_secs: u64,
}

/// Paper-ledger panel (`/pnl`). Lifetime figures come from the book
/// (they survive only until a fresh start-balance boot); the day
/// figure rides the flattened heartbeat (`day_net`). Shipped as one
/// struct so `/pnl` cannot answer half-book (cash updated, PnL stale).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PnlReport {
    /// Realized net edge minus dispatch bleed (all time, this book).
    pub realized_pnl: f64,
    /// Open positions against the realizable exit basis.
    pub unrealized_pnl: f64,
    /// No-impact mark-to-market of the open book.
    pub mark_value: f64,
    /// What closing the full book now would actually net
    /// (size impact + venue fees applied).
    pub realizable_value: f64,
    /// Dispatch costs debited past available cash (0 = solvent).
    pub overdraft_usd: f64,
    pub closed_count: usize,
    /// Wins over closed exits (`None` = none closed yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub win_rate: Option<f64>,
}

/// Sidecar status file path, beside the log (same dir as WAL and
/// day-state; both processes resolve it identically).
pub fn status_path() -> std::path::PathBuf {
    let (_, log_file) = crate::cli::resolve_paths();
    crate::cli::state_file_beside_log(&log_file, STATUS_FILE)
}

/// Read and parse the status file (the sidecar's only data source).
/// A torn/foreign file is an `InvalidData` error — the caller answers
/// best-effort from the last honest read, never invents values.
pub fn read(path: &Path) -> std::io::Result<StatusReport> {
    let raw = std::fs::read(path)?;
    serde_json::from_slice(&raw).map_err(std::io::Error::other)
}

/// Serialize and atomically replace the status file (0600, tmp+rename —
/// the sidecar never observes a torn file). Infallible fields: the
/// report carries only plain numbers/strings.
pub fn write(path: &Path, report: &StatusReport) -> std::io::Result<()> {
    let json = serde_json::to_vec(report).map_err(std::io::Error::other)?;
    crate::cli::atomic_write_private(path, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bare-minimum report fixture (all fields are plain values).
    fn fixture(ts: u64) -> StatusReport {
        StatusReport {
            ts,
            report: HeartbeatReport {
                mode: "paper".to_string(),
                strategy: "adaptive".to_string(),
                uptime_secs: 125,
                logs_seen: 10,
                candidates: 2,
                parse_ok: 2,
                dropped: 0,
                watchlist_pending: 1,
                open_positions: 1,
                manual_bags: 0,
                diverged: 0,
                pending_buys: 0,
                breaker_halted: false,
                day_net: -1.5,
                cash_usd: 98.5,
                equity_usd: 98.5,
                data_paused: false,
                rpc_endpoints: 1,
                rpc_healthy: 1,
                rpc_failover_reads: 0,
                rpc_inconsistent: false,
                oldest_open_age_secs: 60,
                stream_gaps_shed: 0,
                stream_pages_capped: 0,
                giveups_window: 0,
            },
            pnl: PnlReport {
                realized_pnl: -1.5,
                unrealized_pnl: 0.0,
                mark_value: 0.0,
                realizable_value: 0.0,
                overdraft_usd: 0.0,
                closed_count: 0,
                win_rate: None,
            },
            positions: Vec::new(),
            manual_mints: Vec::new(),
            diverged_mints: Vec::new(),
            pending_buy_mints: Vec::new(),
            watchlist: WatchlistSnapshot {
                total: 0,
                entries: Vec::new(),
                min_buyers: 5,
                window_secs: 10,
            },
        }
    }

    #[test]
    fn roundtrips_into_private_json() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join(STATUS_FILE);
        write(&path, &fixture(1_700_000_000)).expect("write");
        let raw = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
        // Flattened: mode sits at the top level, beside ts.
        assert_eq!(value["ts"], 1_700_000_000);
        assert_eq!(value["mode"], "paper");
        assert_eq!(value["strategy"], "adaptive");
        assert_eq!(value["open_positions"], 1);
        assert_eq!(value["equity_usd"], 98.5);
        // 0600 like every other state file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn overwrites_in_place_never_appends() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join(STATUS_FILE);
        write(&path, &fixture(1)).expect("write");
        let first = std::fs::read(&path).expect("read");
        write(&path, &fixture(2)).expect("write");
        let second = std::fs::read(&path).expect("read");
        assert_ne!(first, second, "old ts replaced");
        assert!(serde_json::from_slice::<serde_json::Value>(&second).is_ok());
        // No tmp leftovers after rename.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftover, vec![STATUS_FILE.to_string()]);
    }
}
