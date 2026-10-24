//! Decision journal: append-only JSONL record of what the bot knew and
//! decided, for offline replay and paper/live metric separation.
//!
//! Custody truth stays in `src/wal.rs`; this journal is analytics-only
//! by design. Every write is fail-soft: a broken journal NEVER gates a
//! trade — it latches a degraded flag and warns once. State files in
//! this repo stage through `cli::atomic_write_private` for custody; a
//! journal open is append-only reuse of an existing file, not a
//! rewrite, but it is sealed owner-only (0600) on unix, matching the
//! log and WAL.
//!
//! Replay reads the same lines back and re-derives the paper ledger's
//! decisions from recorded inputs only: no future information exists
//! in the file, and a decision line without its input lines (missing
//! tick, dropped boot) fails verification rather than being assumed.
//!
//! Dropped lines (encode/write failure) COUNT and mark degradation —
//! a replay over a degraded journal still runs, but its `skipped`
//! accounting is loud about the gap.
//!
//! Secret hygiene: journal lines carry transport error text
//! (redacted at the call sites), and the file itself is owner-only
//! (0600 on unix) like the log and the WAL — an analytics-only record
//! must never be group/world-readable, and redaction here is defense
//! in depth, not a license for weaker file modes.

use std::fs::OpenOptions;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Journal file next to the log; `pump-sentry replay` reads it back.
pub fn default_journal_path() -> std::path::PathBuf {
    let (_, log_file) = crate::cli::resolve_paths();
    crate::cli::state_file_beside_log(&log_file, JOURNAL_FILE)
}
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Strategy/execution knobs snapshotted at boot. Replay seeds its
/// reconstructed book from exactly these, so recorded decisions can be
/// re-derived against the same parameter set, not today's config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BootSettings {
    pub mode: String,
    pub strategy_mode: String,
    pub start_balance: f64,
    pub fee_bps: f64,
    pub notional_usd: f64,
    pub trail_percent: f64,
    pub runner_trail_percent: f64,
    pub runner_moonbag_percent: f64,
    pub slippage_bps: u64,
    pub tip_lamports: u64,
    pub priority_fee_micro_lamports: u64,
    pub compute_unit_limit: u32,
    pub max_open_positions: usize,
    pub max_exposure_usd: f64,
    pub max_daily_loss_usd: f64,
    pub hot_wallet_max_sol: f64,
    pub mint_cooldown_secs: u64,
    pub gate_min_buyers: u32,
    pub gate_window_secs: u64,
    pub inference_endpoint: String,
    pub inference_model: String,
    pub inference_timeout_ms: u64,
    pub version: String,
}

impl BootSettings {
    fn from_settings(settings: &crate::settings::Settings, mode: &str) -> Self {
        Self {
            mode: mode.to_string(),
            strategy_mode: settings.strategy_mode.clone(),
            start_balance: settings.start_balance,
            fee_bps: settings.fee_bps,
            notional_usd: settings.notional_usd,
            trail_percent: settings.trail_percent,
            runner_trail_percent: settings.runner_trail_percent,
            runner_moonbag_percent: settings.runner_moonbag_percent,
            slippage_bps: settings.slippage_bps,
            tip_lamports: settings.tip_lamports,
            priority_fee_micro_lamports: settings.priority_fee_micro_lamports,
            compute_unit_limit: settings.compute_unit_limit,
            max_open_positions: settings.max_open_positions,
            max_exposure_usd: settings.max_exposure_usd,
            max_daily_loss_usd: settings.max_daily_loss_usd,
            hot_wallet_max_sol: settings.hot_wallet_max_sol,
            mint_cooldown_secs: settings.mint_cooldown_secs,
            gate_min_buyers: settings.gate_min_buyers,
            gate_window_secs: settings.gate_window_secs,
            inference_endpoint: settings.inference_endpoint.clone(),
            inference_model: settings.inference_model.clone(),
            inference_timeout_ms: settings.inference_timeout_ms,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
    /// Read-back defaults for a journal with no Boot record (or for
    /// tests): typed defaults, no mode.
    pub fn pre_boot() -> Self {
        let mut s = Self::from_settings(&crate::settings::Settings::from_file(None), "");
        s.mode = String::new();
        s.version = String::new();
        s
    }
}

impl Default for BootSettings {
    fn default() -> Self {
        Self::pre_boot()
    }
}

/// Key'd journal line. Tagged `t` like `WalLine`; foreign tags skip
/// with a count instead of aborting the replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum JournalLine {
    /// Boot: process identity, mode, and the knob snapshot.
    Boot { ts: u64, settings: BootSettings },
    /// One tick the paper ledger evaluated a held mint with. Recorded
    /// BEFORE the exit checks: decisions after it re-derive from it.
    Tick {
        ts: u64,
        mint: String,
        mark: simulator::Mark,
        /// Global SOL/USD feed at the tick (paper dispatch-leg basis).
        sol_feed: Option<f64>,
        tick_observed_at: u64,
        complete: bool,
        /// True when the exits decided on this tick are operator-
        /// initiated (`sell <mint>`): their trigger is the user call,
        /// not the market — replay re-derives them as `Manual`, exactly
        /// how the runtime booked them. Market-driven ticks run the
        /// normal trigger re-derivation.
        #[serde(default)]
        manual_exits: bool,
    },
    /// Paper open: the ledger position as booked (serde-exact), plus
    /// the decision provenance (the mark the open priced on, and the
    /// risk engine source that shaped the profile).
    Open {
        ts: u64,
        mint: String,
        /// Risk profile provenance: "model" | "heuristic".
        risk: String,
        /// Risk-engine model the decision rode (`Some` whenever an
        /// engine is configured — model or fallback alike, since a
        /// fallback open is attributed to the model whose failure
        /// produced it; `None` = no engine). Aggregated offline as
        /// per-model performance; never a gate input.
        #[serde(default)]
        model: Option<String>,
        venue_fee_bps: f64,
        sol_feed: Option<f64>,
        /// Open mark (spot + depth + exact curve when carried).
        mark: simulator::Mark,
        /// The actual ticket the open was sized on (gate passes the
        /// boot-snapshotted notional straight through; recorded so
        /// replay never infers it and mid-run settings drift shows up
        /// as a NOTE, not a numeric mismatch cascade).
        #[serde(default)]
        ticket_usd: f64,
        position: simulator::Position,
        /// Confirm-tape microstructure snapshot (`None` on pre-signal
        /// lines): the flow the entry priced on, for offline
        /// expectancy analysis per signal.
        #[serde(default)]
        micro: Option<listener::MicroSignals>,
        /// Effective exit regime (`None` on pre-regime lines): what the
        /// exit actually ran, for offline expectancy per regime.
        #[serde(default)]
        regime: Option<String>,
        /// Ex-ante instant-flip cost at the confirm mark, percent
        /// (`None` = unpriceable/pre-flip lines): opening and selling
        /// straight back at current depth — entry impact, exit impact,
        /// and venue fees all inside. Burn-in analytics read the cost
        /// edge per decision; nothing recomputes it.
        #[serde(default)]
        flip_loss_pct: Option<f64>,
    },
    /// Paper exit: the ledger trade as booked plus the fraction the
    /// book closed and the mark it closed on.
    Exit {
        ts: u64,
        mint: String,
        fraction: f64,
        tick_observed_at: u64,
        trade: simulator::ClosedTrade,
        mark: simulator::Mark,
    },
    /// Live buy dispatched through Jito (bundle id known).
    BuyAttempt {
        ts: u64,
        mint: String,
        bundle: String,
        tokens: u64,
        fill: Option<FillClaim>,
        /// Actual first-touch cost debited from paper cash, USD.
        debit_usd: Option<f64>,
    },
    /// Live buy decided NOT to dispatch; the reason is the decision.
    BuySkip {
        ts: u64,
        mint: String,
        reason: String,
    },
    /// Terminal/recheck state for a pending live buy.
    BuyOutcome {
        ts: u64,
        mint: String,
        bundle: Option<String>,
        /// "confirmed" | "dead" | "giveup" | "unknown"
        verdict: String,
        detail: Option<String>,
        debit_usd: Option<f64>,
    },
    /// Live sell dispatched through Jito.
    SellAttempt {
        ts: u64,
        mint: String,
        bundle: String,
        /// "curve" | "pumpswap" | "raydium"
        venue: String,
        tokens: u64,
        price_usd: f64,
        /// Actual tip cost debited from paper cash, USD.
        debit_usd: Option<f64>,
        /// True when this dispatch's tip already rides inside the
        /// paired exit Gap line (the tracker owns that math) — replay
        /// subtracts the debit only when UNPAIRED, so each tip counts
        /// exactly once in the live-net estimate. `#[serde(default)]`:
        /// pre-upgrade lines read as unpaired.
        #[serde(default)]
        settle_paired: bool,
    },
    /// Live sell automation exhausted its budget; parked for manual.
    SellGiveUp {
        ts: u64,
        mint: String,
        cause: String,
    },
    /// Settled execution gap (single owner of gap math: the tracker;
    /// this just mirrors its output for the metrics pass).
    Gap {
        ts: u64,
        mint: String,
        /// "entry" | "exit"
        leg: String,
        paper_price_usd: f64,
        realized_price_usd: f64,
        qty_tokens: f64,
        tip_usd: Option<f64>,
        gap_usd: f64,
        /// Dispatch path that settled (`curve` | `pumpswap` |
        /// `raydium`; `""` on pre-dimension lines). Feeds the
        /// venue-bucketed replay aggregates.
        #[serde(default)]
        venue: String,
        /// SOL-side depth at settle (0.0 = unknown, e.g. pre-dimension
        /// lines). Feeds the liquidity buckets.
        #[serde(default)]
        liquidity_usd: f64,
    },
    /// Heartbeat equity snapshot (paper book). Drawdown evidence.
    Equity {
        ts: u64,
        cash: f64,
        equity: f64,
        open_count: usize,
        day_net: f64,
        breaker_halted: bool,
    },
}

/// One landed live buy fill in USD (entry-gap evidence).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FillClaim {
    pub fill_price_usd: f64,
    pub tokens: f64,
}

/// Append-only JSONL sink. Never latches custody and never blocks
/// trading: `append` only serializes (fast, in-memory) and queues the
/// line on a bounded channel to a dedicated writer thread. A full or
/// dead channel flips `degraded` and warns once, then writes no-op —
/// decisions keep flowing while the journal records the gap by its
/// absence (replay `skipped` accounting stays loud about it).
pub struct JournalSink {
    tx: Mutex<Option<std::sync::mpsc::SyncSender<WriterMsg>>>,
    degraded: Arc<AtomicBool>,
}

/// Writer-thread work item. The barrier lets `flush` (tests,
/// shutdown) wait until every line queued before it hit the disk.
enum WriterMsg {
    Line(String),
    Barrier(std::sync::mpsc::SyncSender<()>),
}

/// Bounded queue depth: burst headroom for tick storms; beyond it the
/// sink degrades (drops + warns) rather than back-pressuring trading.
const JOURNAL_QUEUE_DEPTH: usize = 1024;

impl JournalSink {
    /// Fail-soft open: errors return None with a warning, never panic.
    pub fn maybe_open(path: &Path) -> Option<Self> {
        match Self::open(path) {
            Ok(sink) => Some(sink),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "decision journal unavailable (analytic-only, trading continues unlocked)");
                None
            }
        }
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(path)?;
        // Re-seal an existing file to owner-only: a journal
        // created by an older release (procedural umask, often 0644)
        // must not stay group/world-readable now that it carries
        // redacted-but-sensitive transport details.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let degraded = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel::<WriterMsg>(JOURNAL_QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("journal-writer".to_string())
            .spawn({
                let degraded = degraded.clone();
                move || writer_loop(rx, file, &degraded)
            })
            .map_err(std::io::Error::other)?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            degraded,
        })
    }

    /// Boot snapshot line from the live settings.
    pub fn boot(&self, ts: u64, settings: &crate::settings::Settings, mode: &str) {
        self.append(&JournalLine::Boot {
            ts,
            settings: BootSettings::from_settings(settings, mode),
        });
    }

    /// Append one encoded line. Fail-soft and non-blocking: encode
    /// or queue failure warns once and flips `degraded` (trading is
    /// never affected, the writer thread owns all disk waits).
    pub fn append(&self, line: &JournalLine) {
        if self.degraded.load(Ordering::Relaxed) {
            return;
        }
        let line_text = match serde_json::to_string(line) {
            Ok(mut s) => {
                s.push('\n');
                s
            }
            Err(e) => {
                self.degrade(format!("encode failed: {e}"));
                return;
            }
        };
        let tx = match self.tx.lock() {
            Ok(g) => g.clone(),
            Err(_) => {
                self.degrade("sender lock poisoned".to_string());
                return;
            }
        };
        let Some(tx) = tx else {
            self.degrade("writer gone".to_string());
            return;
        };
        match tx.try_send(WriterMsg::Line(line_text)) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.degrade("queue full (burst beyond 1024); dropping".to_string());
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.degrade("writer thread gone".to_string());
            }
        }
    }

    /// Block (bounded) until every line queued before the call hits the
    /// disk. For tests and shutdown — never on the trading hot path.
    pub fn flush(&self) -> bool {
        if self.degraded.load(Ordering::Relaxed) {
            return false;
        }
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let tx = match self.tx.lock() {
            Ok(g) => g.clone(),
            Err(_) => return false,
        };
        let Some(tx) = tx else { return false };
        // Bounded, never blocking: a full queue degrades instead of
        // hanging the caller.
        if tx.try_send(WriterMsg::Barrier(ack_tx)).is_err() {
            self.degrade("flush barrier not accepted".to_string());
            return false;
        }
        if ack_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            self.degrade("writer unresponsive".to_string());
            return false;
        }
        true
    }

    fn degrade(&self, reason: String) {
        if !self.degraded.swap(true, Ordering::Relaxed) {
            warn!("decision journal degraded ({reason}; analysis only, trading unaffected)");
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }
}

impl Drop for JournalSink {
    fn drop(&mut self) {
        // Shutdown path only (the sink lives in an `Arc` for the run):
        // give queued lines a bounded chance to land, never hang exit.
        self.flush();
    }
}

/// Dedicated writer: the only thread that ever touches the journal
/// file. Slow disks stall here, never the trading hot path. An IO
/// error degrades once and stops the loop — queued lines behind the
/// failure are dropped (fail-soft: counted by their absence, replay
/// `skipped` stays loud), trading continues.
fn writer_loop(
    rx: std::sync::mpsc::Receiver<WriterMsg>,
    file: std::fs::File,
    degraded: &AtomicBool,
) {
    let mut writer = BufWriter::new(file);
    let fail = |reason: String, degraded: &AtomicBool| {
        if !degraded.swap(true, Ordering::Relaxed) {
            warn!("decision journal degraded ({reason}; analysis only, trading unaffected)");
        }
    };
    for msg in rx {
        match msg {
            WriterMsg::Line(text) => {
                if writer
                    .write_all(text.as_bytes())
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    fail("write failed".to_string(), degraded);
                    break;
                }
            }
            WriterMsg::Barrier(ack) => {
                if writer.flush().is_err() {
                    fail("flush failed".to_string(), degraded);
                    break;
                }
                let _ = ack.send(());
            }
        }
    }
}

/// One decoded line, or a skip reason. Boxed: journal lines are
/// big-ish and the skip arms are the common case.
pub enum Decoded {
    Line(Box<JournalLine>),
    /// Blank.
    Blank,
    /// Foreign tag or malformed line (never half-adopted).
    Bad,
}

/// Decode one JSONL line. Strict: a line missing a load-bearing field
/// (or carrying the wrong type) is damage — counted in `skipped`,
/// never half-adopted.
pub fn decode_line(raw: &str) -> Decoded {
    if raw.trim().is_empty() {
        return Decoded::Blank;
    }
    match serde_json::from_str::<JournalLine>(raw) {
        Ok(line) => Decoded::Line(Box::new(line)),
        Err(_) => Decoded::Bad,
    }
}
/// Read the whole journal back; never half-adopt a bad line (counted,
/// skipped). Missing file (`NotFound`) is an empty journal —
/// legitimate for a first run; any OTHER open error and a mid-file
/// read failure surface as `read_error` for fail-closed callers: the
/// file is the record of record, so "unreadable" must never print as
/// "nothing recorded". Read failure stops the loop after one damage
/// count, keeping the lines decoded before it.
pub fn load_lines(path: &Path) -> (Vec<JournalLine>, usize, usize, Option<String>) {
    let mut lines = Vec::new();
    let mut blank = 0usize;
    let mut bad = 0usize;
    match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return (lines, blank, bad, Some(e.to_string())),
        Ok(file) => {
            for raw in std::io::BufReader::new(file).lines() {
                match raw {
                    Ok(line) => match decode_line(&line) {
                        Decoded::Line(line) => lines.push(*line),
                        Decoded::Blank => blank += 1,
                        Decoded::Bad => bad += 1,
                    },
                    // Read error counts as damage, never as a blank
                    // line. One count, then stop: a reader that keeps
                    // erroring (a directory fd, dying disk) must not
                    // spin "bad" counts forever, and the lines decoded
                    // before the failure stay usable.
                    Err(_) => {
                        bad += 1;
                        return (
                            lines,
                            blank,
                            bad,
                            Some(
                                "read failed mid-file; lines before it kept, the rest unproven"
                                    .to_string(),
                            ),
                        );
                    }
                }
            }
        }
    }
    (lines, blank, bad, None)
}

/// The tail slice of a [`load_recent_lines`] read.
#[derive(Debug, Default)]
pub struct JournalTail {
    /// Decoded lines, newest last (capped at the requested line cap).
    pub lines: Vec<JournalLine>,
    /// Decoded-record lines inside the scanned span. When
    /// `truncated`, this counts only the byte-window, not the file.
    pub scanned: usize,
    /// Foreign/undecodable lines inside the scanned span.
    pub bad: usize,
    /// True when the byte cap skipped some (possibly most) of the
    /// file: the window's first line may be torn (it is dropped, not
    /// invented as damage), and `scanned`/`bad` say nothing about the
    /// history before it.
    pub truncated: bool,
    /// Fail-closed read error (missing file is None, an empty journal).
    pub error: Option<String>,
}

/// Read only the trailing slice of the journal: same line grammar
/// and fail-closed semantics as [`load_lines`], but memory stays
/// bounded no matter how large the file grows (the sidecar answers
/// `/research` and `/token` from recent history, never a full replay).
/// **Both caps bind: `max_bytes` (fread from the end) and `max_lines`
/// (kept).** The byte cap bounds the read itself, so an unbounded
/// journal cannot stretch a Discord ack past the 3-second window
/// (memory attendance alone does not bound scan time). Within the
/// scanned span every line is counted, so callers say honestly how
/// much history the slice covers — and, when `truncated`, that older
/// history was not scanned at all.
pub fn load_recent_lines(path: &Path, max_lines: usize, max_bytes: u64) -> JournalTail {
    JournalTail::from_file(path, max_lines, max_bytes)
}
impl JournalTail {
    fn from_file(path: &Path, max_lines: usize, max_bytes: u64) -> Self {
        use std::collections::VecDeque;
        use std::io::{Read, Seek, SeekFrom};
        // An empty cap still keeps one line (bounded windows, never
        // zero-work surprises for the caller below).
        let cap = max_lines.max(1);
        let file_len = match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                return Self {
                    error: Some(e.to_string()),
                    ..Self::default()
                };
            }
            Ok(m) => m.len(),
        };
        if file_len > max_bytes {
            // File exceeds the cap: read only the last `max_bytes`
            // (the read itself is bounded, so scan time cannot chase an
            // unbounded journal). One extra leading byte tells whether
            // the window begins exactly on a line boundary: keep the
            // whole window then; otherwise the first line is torn and
            // is dropped, never invented as damage (its history is
            // explicitly unscanned, not broken).
            let mut buffer = Vec::new();
            return match std::fs::File::open(path).and_then(|mut f| {
                f.seek(SeekFrom::Start(file_len - max_bytes - 1))?;
                (&mut f).take(max_bytes + 1).read_to_end(&mut buffer)
            }) {
                Err(e) => Self {
                    truncated: true,
                    error: Some(e.to_string()),
                    ..Self::default()
                },
                Ok(_) => {
                    let body = match buffer.first() {
                        Some(&b'\n') => &buffer[1..],
                        _ => buffer
                            .iter()
                            .position(|&b| b == b'\n')
                            .map(|i| &buffer[i + 1..])
                            .unwrap_or(&[]),
                    };
                    let mut held: VecDeque<JournalLine> = VecDeque::new();
                    let mut scanned = 0usize;
                    let mut bad = 0usize;
                    for raw in String::from_utf8_lossy(body).lines() {
                        match decode_line(raw) {
                            Decoded::Line(line) => {
                                scanned += 1;
                                if held.len() >= cap {
                                    held.pop_front();
                                }
                                held.push_back(*line);
                            }
                            Decoded::Blank => {}
                            Decoded::Bad => bad += 1,
                        }
                    }
                    Self {
                        lines: held.into_iter().collect(),
                        scanned,
                        bad,
                        truncated: true,
                        error: None,
                    }
                }
            };
        }
        // File fits inside the byte cap: a full scan (whole-span
        // counting, torn-line handling identical to `load_lines`).
        let mut held: VecDeque<JournalLine> = VecDeque::new();
        let mut scanned = 0usize;
        let mut bad = 0usize;
        match std::fs::File::open(path) {
            Err(e) => {
                return Self {
                    error: Some(e.to_string()),
                    ..Self::default()
                };
            }
            Ok(file) => {
                for raw in std::io::BufReader::new(file).lines() {
                    match raw {
                        Ok(line) => match decode_line(&line) {
                            Decoded::Line(line) => {
                                scanned += 1;
                                if held.len() >= cap {
                                    held.pop_front();
                                }
                                held.push_back(*line);
                            }
                            Decoded::Blank => {}
                            Decoded::Bad => bad += 1,
                        },
                        // Read error counts as damage, never as a blank
                        // line. One count, then stop: a reader that
                        // keeps erroring (a directory fd, dying disk)
                        // must not spin "bad" counts forever, and the
                        // lines decoded before the failure stay usable.
                        Err(_) => {
                            bad += 1;
                            return Self {
                                lines: held.into_iter().collect(),
                                scanned,
                                bad,
                                truncated: false,
                                error: Some(
                                    "read failed mid-file; lines kept before it, the rest unproven"
                                        .to_string(),
                                ),
                            };
                        }
                    }
                }
            }
        }
        Self {
            lines: held.into_iter().collect(),
            scanned,
            bad,
            truncated: false,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos() -> simulator::Position {
        simulator::Position {
            id: 7,
            mint: "M".into(),
            side: simulator::Side::Long,
            entry_price: 1.5,
            quantity: 42,
            entry_notional: 63.0,
            entry_fee: 0.7875,
            tp_percent: 60.0,
            sl_percent: -25.0,
            trail_percent: 30.0,
            trailing_stop: true,
            best_price: 1.5,
            opened_at: 100,
            timeout_seconds: 300,
            moonbag_percent: 10.0,
        }
    }

    #[test]
    fn journal_is_owner_only_and_reseals_looser_files() {
        // The decision journal may carry redacted transport
        // detail; owner-only (0600) is required on a fresh file, and a
        // pre-existing looser file (older release, umask) is re-sealed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("journal.jsonl");
            let sink = JournalSink::open(&path).unwrap();
            sink.append(&JournalLine::Boot {
                ts: 1,
                settings: BootSettings::pre_boot(),
            });
            assert!(sink.flush());
            drop(sink);
            let mode = std::fs::metadata(&path).expect("file").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "fresh journal must be owner-only");
            // A group/world-readable old file is re-sealed on open.
            let old = dir.path().join("old.jsonl");
            std::fs::write(&old, "stale\n").expect("write");
            std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644))
                .expect("chmod 644");
            let sink = JournalSink::open(&old).expect("open");
            assert!(sink.flush());
            drop(sink);
            let mode = std::fs::metadata(&old).expect("file").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "pre-existing journal is re-sealed");
        }
    }

    #[test]
    fn roundtrip_all_tags() {
        let line = JournalLine::Boot {
            ts: 1,
            settings: BootSettings::pre_boot(),
        };
        let raw = serde_json::to_string(&line).unwrap();
        assert!(matches!(decode_line(&raw), Decoded::Line(_)));
        assert!(matches!(decode_line(""), Decoded::Blank));
        assert!(matches!(decode_line("not json"), Decoded::Bad));
        assert!(matches!(decode_line("{\"t\":\"future\"}"), Decoded::Bad));

        let tick = JournalLine::Tick {
            ts: 2,
            mint: "M".into(),
            mark: simulator::Mark::price_only(1.0),
            sol_feed: Some(100.0),
            tick_observed_at: 2,
            complete: false,
            manual_exits: false,
        };
        let raw = serde_json::to_string(&tick).unwrap();
        assert!(matches!(decode_line(&raw), Decoded::Line(_)));

        let exit = JournalLine::Exit {
            ts: 3,
            mint: "M".into(),
            fraction: 1.0,
            tick_observed_at: 2,
            trade: simulator::ClosedTrade {
                id: 7,
                mint: "M".into(),
                side: simulator::Side::Long,
                entry_price: 1.5,
                exit_price: 2.0,
                quantity: 42,
                gross_pnl: 21.0,
                fees: 1.0,
                net_pnl: 20.0,
                net_pnl_percent: 31.7,
                reason: simulator::ExitReason::TakeProfit,
                fraction_closed: 1.0,
                opened_at: 100,
                closed_at: 140,
                hold_seconds: 40,
                dispatch_usd: 0.01,
            },
            mark: simulator::Mark::price_only(2.0),
        };
        let raw = serde_json::to_string(&exit).unwrap();
        assert!(matches!(decode_line(&raw), Decoded::Line(_)));

        let open = JournalLine::Open {
            ts: 1,
            mint: "M".into(),
            risk: "model".into(),
            model: Some("test-model".into()),
            venue_fee_bps: 125.0,
            sol_feed: None,
            mark: simulator::Mark::new(1.0, 50_000.0),
            ticket_usd: 100.0,
            position: pos(),
            micro: None,
            regime: None,
            flip_loss_pct: Some(6.5),
        };
        let raw = serde_json::to_string(&open).unwrap();
        assert!(matches!(decode_line(&raw), Decoded::Line(_)));
        // Pre-signal lines carry no micro: they decode with None, never
        // a fabricated zero-signal. The flip preview is newer still:
        // pre-flip open lines decode with None (absent, not zero).
        let mut old = serde_json::to_value(&open).unwrap();
        old.as_object_mut().unwrap().remove("micro");
        old.as_object_mut().unwrap().remove("regime");
        old.as_object_mut().unwrap().remove("model");
        old.as_object_mut().unwrap().remove("flip_loss_pct");
        match decode_line(&serde_json::to_string(&old).unwrap()) {
            Decoded::Line(line)
                if matches!(
                    &*line,
                    JournalLine::Open {
                        micro: None,
                        regime: None,
                        model: None,
                        flip_loss_pct: None,
                        ..
                    }
                ) => {}
            _ => panic!("pre-signal open must decode with micro/regime/model None"),
        }
    }

    #[test]
    fn sink_writes_jsonl_and_reports_unavailable_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let sink = JournalSink::open(&path).unwrap();
        sink.append(&JournalLine::Boot {
            ts: 5,
            settings: BootSettings::pre_boot(),
        });
        assert!(!sink.is_degraded());
        assert!(sink.flush(), "queued line lands before the read");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        // Unwritable destination: None + warn (fail-soft open).
        assert!(JournalSink::maybe_open(&dir.path().join("no-dir/journal.jsonl")).is_none());
        // Read side distinguishes: the staged journal decodes, a missing
        // file is an empty journal (NotFound, first run), and another
        // open failure surfaces as `read_error` — never as a silent
        // empty journal.
        let (lines, blank, bad, err) = load_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!((blank, bad), (0, 0));
        assert!(err.is_none());
        let (lines, _, _, err) = load_lines(&dir.path().join("never-written.jsonl"));
        assert!(lines.is_empty());
        assert!(err.is_none(), "NotFound is an empty journal, not damage");
        let (lines, _, _, err) = load_lines(dir.path());
        assert!(lines.is_empty());
        assert!(err.is_some(), "open error on a directory must surface");
    }

    #[test]
    fn nan_payload_skips_on_read_and_sink_stays_usable() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JournalSink::open(&dir.path().join("journal.jsonl")).unwrap();
        // A NaN ledger value cannot be honestly encoded; it writes as
        // JSON null and decode-side cannot fabricate a number, so the
        // whole line is skipped at read (counted as Bad), never zero.
        let nan_line = JournalLine::Equity {
            ts: 1,
            cash: f64::NAN,
            equity: 1.0,
            open_count: 0,
            day_net: 0.0,
            breaker_halted: false,
        };
        let raw = serde_json::to_string(&nan_line).unwrap();
        assert!(matches!(decode_line(&raw), Decoded::Bad));
        // A subsequent valid append still lands.
        sink.append(&JournalLine::Boot {
            ts: 2,
            settings: BootSettings::pre_boot(),
        });
        assert!(sink.flush());
        let text = std::fs::read_to_string(dir.path().join("journal.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn burst_appends_never_block_and_never_lose_quietly() {
        // The hot-path contract: a tick storm must not stall the
        // caller, and whatever lands must be complete lines. Either
        // every line lands, or the sink degrades loudly — never a
        // block, never silent loss.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let sink = JournalSink::open(&path).unwrap();
        let started = std::time::Instant::now();
        for i in 0..5000u64 {
            sink.append(&JournalLine::Tick {
                ts: i,
                mint: "M".into(),
                mark: simulator::Mark::price_only(1.0),
                sol_feed: Some(100.0),
                tick_observed_at: i,
                complete: false,
                manual_exits: false,
            });
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "5000 queued appends must not block trading"
        );
        if sink.flush() {
            assert!(!sink.is_degraded());
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text.lines().count(), 5000, "no silent loss");
            let (lines, _, bad, _) = load_lines(&path);
            assert_eq!(lines.len(), 5000);
            assert_eq!(bad, 0);
        } else {
            assert!(sink.is_degraded(), "flush failure means loud degradation");
        }
    }

    #[test]
    fn recent_tail_bounds_memory_but_counts_everything() {
        use std::fmt::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let mut text = String::new();
        for i in 0..10u64 {
            let line = JournalLine::Tick {
                ts: i,
                mint: "M".into(),
                mark: simulator::Mark::price_only(1.0),
                sol_feed: Some(100.0),
                tick_observed_at: i,
                complete: false,
                manual_exits: false,
            };
            writeln!(text, "{}", serde_json::to_string(&line).unwrap()).unwrap();
        }
        text.push_str("\nnot json\n");
        std::fs::write(&path, &text).unwrap();
        let bytes = text.len() as u64;
        let tail = load_recent_lines(&path, 4, bytes);
        assert_eq!(tail.error, None);
        assert!(!tail.truncated, "fitting file scans fully");
        assert_eq!(
            tail.scanned, 10,
            "total covers the whole file, not the slice"
        );
        assert_eq!(tail.lines.len(), 4, "memory bounded by the cap");
        assert_eq!(tail.bad, 1);
        // Newest-last order kept: tail holds ts 6..10.
        let ts: Vec<u64> = tail
            .lines
            .iter()
            .map(|l| match l {
                JournalLine::Tick { ts, .. } => *ts,
                _ => u64::MAX,
            })
            .collect();
        assert_eq!(ts, vec![6, 7, 8, 9]);
        // Missing file is an empty journal, never an error.
        let tail = load_recent_lines(&dir.path().join("absent.jsonl"), 4, bytes);
        assert_eq!((tail.lines.len(), tail.scanned), (0, 0));
        assert!(tail.error.is_none());
    }

    #[test]
    fn recent_tail_byte_cap_skips_history_loudly() {
        // L1: the byte cap bounds the actual read, so a huge journal
        // cannot stretch a Discord ack; the truncation is visible to
        // callers and the torn first line is dropped, never decoded
        // as damage.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.jsonl");
        let tick = |ts: u64| -> String {
            let line = JournalLine::Tick {
                ts,
                mint: "M".into(),
                mark: simulator::Mark::price_only(1.0),
                sol_feed: Some(100.0),
                tick_observed_at: ts,
                complete: false,
                manual_exits: false,
            };
            format!("{}\n", serde_json::to_string(&line).unwrap())
        };
        let first = tick(1);
        let tail_text: String = (2..=5).map(tick).collect();
        std::fs::write(&path, format!("{first}{tail_text}")).unwrap();
        let total_bytes = (first.len() + tail_text.len()) as u64;
        // Window covers the last 4 lines exactly (no torn line: the
        // byte window starts right at the boundary).
        let tail = load_recent_lines(&path, 100, tail_text.len() as u64);
        assert!(tail.truncated);
        assert_eq!(tail.scanned, 4, "complete lines inside the window count");
        assert_eq!(tail.lines.len(), 4);
        let ts: Vec<u64> = tail
            .lines
            .iter()
            .map(|l| match l {
                JournalLine::Tick { ts, .. } => *ts,
                _ => u64::MAX,
            })
            .collect();
        assert_eq!(ts, vec![2, 3, 4, 5]);
        assert!(tail.error.is_none(), "an unscanned prefix is not an error");
        // Mid-line window start: the torn line is dropped, not "bad".
        let tail = load_recent_lines(&path, 100, total_bytes - first.len() as u64 / 2);
        assert!(tail.truncated);
        assert_eq!(tail.scanned, 4, "torn lead line dropped, complete kept");
        assert_eq!(tail.bad, 0, "a torn line is unscanned history, not damage");
        // A window with no complete line at all yields an empty tail.
        let tail = load_recent_lines(&path, 100, (first.len() / 2) as u64);
        assert!(tail.truncated);
        assert!(tail.lines.is_empty());
        assert_eq!(tail.scanned, 0);
    }
}
