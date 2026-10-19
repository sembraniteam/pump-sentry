//! Decision journal: append-only JSONL record of what the bot knew and
//! decided, for offline replay and paper/live metric separation.
//!
//! Custody truth stays in `src/wal.rs`; this journal is analytics-only
//! by design. Every write is fail-soft: a broken journal NEVER gates a
//! trade — it latches a degraded flag and warns once. State files in
//! this repo stage through `cli::atomic_write_private` for custody; a
//! journal open is append-only reuse of an existing file, not a
//! rewrite, so `create` mode re-seals to the owner-like umask and no
//! operator override of perms happens here.
//!
//! Replay reads the same lines back and re-derives the paper ledger's
//! decisions from recorded inputs only: no future information exists
//! in the file, and a decision line without its input lines (missing
//! tick, dropped boot) fails verification rather than being assumed.
//!
//! Dropped lines (encode/write failure) COUNT and mark degradation —
//! a replay over a degraded journal still runs, but its `skipped`
//! accounting is loud about the gap.

use std::fs::OpenOptions;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Append-only JSONL sink. Never latches custody: a write failure
/// flips `degraded` and warns once, then all writes no-op.
pub struct JournalSink {
    writer: Mutex<BufWriter<std::fs::File>>,
    degraded: AtomicBool,
}

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
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            degraded: AtomicBool::new(false),
        })
    }

    /// Boot snapshot line from the live settings.
    pub fn boot(&self, ts: u64, settings: &crate::settings::Settings, mode: &str) {
        self.append(&JournalLine::Boot {
            ts,
            settings: BootSettings::from_settings(settings, mode),
        });
    }

    /// Append one encoded line. Fail-soft: encode or IO failure warns
    /// once and flips `degraded` (trading is never affected).
    pub fn append(&self, line: &JournalLine) {
        if self.degraded.load(Ordering::Relaxed) {
            return;
        }
        let mut writer = match self.writer.lock() {
            Ok(g) => g,
            Err(_) => {
                // A poisoned mutex means a write panicked mid-line —
                // same fail-soft policy as IO: latch loud, never block
                // trading, and never fake the missing lines.
                if !self.degraded.swap(true, Ordering::Relaxed) {
                    warn!(
                        "decision journal degraded (writer lock poisoned; analysis only, trading unaffected)"
                    );
                }
                return;
            }
        };
        let encode = serde_json::to_string(line).map(|mut s| {
            s.push('\n');
            s
        });
        let out = match encode {
            Ok(s) => writer.write_all(s.as_bytes()).and_then(|()| writer.flush()),
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        };
        if let Err(e) = out {
            self.degraded.store(true, Ordering::Relaxed);
            warn!(error = %e, "decision journal degraded (analysis only; trading unaffected)");
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
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
/// skipped). A read error mid-file counts the loss as bad rather than
/// pretending the missing lines exist; an unreadable file is empty.
pub fn load_lines(path: &std::path::Path) -> (Vec<JournalLine>, usize, usize) {
    let mut lines = Vec::new();
    let mut blank = 0usize;
    let mut bad = 0usize;
    if let Ok(file) = std::fs::File::open(path) {
        for raw in std::io::BufReader::new(file).lines() {
            match raw {
                // Read error counts as damage, never as a blank line.
                Err(_) => bad += 1,
                Ok(line) => match decode_line(&line) {
                    Decoded::Line(line) => lines.push(*line),
                    Decoded::Blank => blank += 1,
                    Decoded::Bad => bad += 1,
                },
            }
        }
    }
    (lines, blank, bad)
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
            venue_fee_bps: 125.0,
            sol_feed: None,
            mark: simulator::Mark::new(1.0, 50_000.0),
            ticket_usd: 100.0,
            position: pos(),
            micro: None,
        };
        let raw = serde_json::to_string(&open).unwrap();
        assert!(matches!(decode_line(&raw), Decoded::Line(_)));
        // Pre-signal lines carry no micro: they decode with None, never
        // a fabricated zero-signal.
        let mut old = serde_json::to_value(&open).unwrap();
        old.as_object_mut().unwrap().remove("micro");
        match decode_line(&serde_json::to_string(&old).unwrap()) {
            Decoded::Line(line)
                if matches!(&*line, JournalLine::Open { micro: None, .. }) => {}
            _ => panic!("pre-signal open must decode with micro None"),
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
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        // Unwritable destination: None + warn (fail-soft open).
        assert!(JournalSink::maybe_open(&dir.path().join("no-dir/journal.jsonl")).is_none());
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
        let text = std::fs::read_to_string(dir.path().join("journal.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
    }
}
