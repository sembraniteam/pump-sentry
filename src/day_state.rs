//! Restart-persistent day-loss tally (`day-state.json`).
//!
//! The day-loss breaker is the one risk limit whose state must outlive
//! a restart: bouncing the process must never reset the day's realized
//! loss. The file holds scalars only; a missing, stale, or corrupt file
//! degrades to a fresh day (never to a wrong number).
use crate::gate::DailyBreaker;
use std::path::{Path, PathBuf};

/// Filename inside the state dir (next to the log/WAL).
pub const DAY_STATE_FILE: &str = "day-state.json";

/// Persisted scalars.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DayState {
    /// File schema version. `0` = written before versioning existed
    /// (defaultable). A file from a NEWER schema is not ours to
    /// interpret — it degrades to a fresh day, loudly.
    #[serde(default)]
    pub schema: u32,
    /// UTC day bucket (`unix_secs / 86_400`).
    pub day: u64,
    /// Realized net PnL within `day` (negative = losing).
    pub day_net: f64,
    /// Day bucket the halt card already fired for (`0` = none).
    /// Defaultable: files written before the field existed.
    #[serde(default)]
    pub halt_alerted_day: u64,
}

/// Current file schema. Bump on any shape change that old readers
/// must not adopt (they refuse newer numbers, never guess).
pub const DAY_STATE_SCHEMA: u32 = 1;

/// Load the tally. `None` (fresh day) when missing, corrupt, or
/// non-finite; only real damage warns — a first boot is silent.
pub fn load(path: &Path) -> Option<DayState> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "day-state unreadable; day-loss tally starts fresh (restart would reset it)");
            return None;
        }
    };
    match serde_json::from_str::<DayState>(&raw) {
        Ok(state) if state.schema > DAY_STATE_SCHEMA => {
            tracing::warn!(file = %path.display(), schema = state.schema, ours = DAY_STATE_SCHEMA,
                "day-state written by a NEWER version; refusing to adopt its numbers — day-loss tally starts fresh (upgrade the binary or delete the file)");
            None
        }
        Ok(state) if state.day_net.is_finite() => Some(state),
        Ok(state) => {
            tracing::warn!(file = %path.display(), day_net = state.day_net, "day-state carries a non-finite tally; day-loss starts fresh");
            None
        }
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "day-state corrupt; day-loss tally starts fresh");
            None
        }
    }
}

/// Write atomically (staged temp + rename, owner-only). Atomic so a
/// crash mid-write can never leave a truncated tally behind.
pub fn save(path: &Path, state: &DayState) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec(state).expect("day state serializes");
    crate::cli::atomic_write_private(path, &json)
}

/// Day-loss breaker with its home on disk: `record` persists (the
/// tally must never be reset by a restart), reads stay pure.
#[derive(Debug)]
pub struct DayBook {
    breaker: DailyBreaker,
    path: Option<PathBuf>,
}

impl DayBook {
    /// Load the persisted tally when it belongs to the same UTC day,
    /// then arm persistence. `None` = in-memory only (tests, or a run
    /// with no state dir). A stale bucket is a fresh day by definition.
    pub fn open(path: Option<PathBuf>, now_secs: u64) -> Self {
        let mut book = Self {
            breaker: DailyBreaker::new(now_secs),
            path,
        };
        if let Some(path) = book.path.as_deref()
            && let Some(state) = load(path)
        {
            if state.day == now_secs / 86_400 {
                book.breaker.day_net = state.day_net;
                book.breaker.halt_alerted_day = state.halt_alerted_day;
                tracing::info!(file = %path.display(), day_net = state.day_net, "day-loss tally resumed from disk");
            } else {
                tracing::debug!(file = %path.display(), saved_day = state.day, "day-state from an older day; fresh tally");
            }
        }
        book
    }

    /// Fold one closed trade in and persist it. A failed write warns:
    /// the in-memory tally stays authoritative for this run.
    pub fn record(&mut self, net_pnl: f64, now_secs: u64) {
        self.breaker.record(net_pnl, now_secs);
        self.persist();
    }

    /// Same contract as `DailyBreaker::tripped`; a day rollover
    /// persists (today's zero replaces yesterday's tally).
    pub fn tripped(&mut self, cap_usd: f64, now_secs: u64) -> bool {
        let rolled = now_secs / 86_400 != self.breaker.day;
        let tripped = self.breaker.tripped(cap_usd, now_secs);
        if rolled {
            self.persist();
        }
        tripped
    }

    /// Same contract as `DailyBreaker::halt_state`; the one-shot card
    /// marker persists so a restart cannot re-fire the day's card.
    pub fn halt_state(&mut self, cap_usd: f64, now_secs: u64) -> (bool, bool) {
        let before = self.breaker.halt_alerted_day;
        let out = self.breaker.halt_state(cap_usd, now_secs);
        if self.breaker.halt_alerted_day != before {
            self.persist();
        }
        out
    }

    /// Realized net PnL today.
    pub fn day_net(&self) -> f64 {
        self.breaker.day_net
    }

    /// Best-effort write of the current scalars.
    fn persist(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let state = DayState {
            schema: DAY_STATE_SCHEMA,
            day: self.breaker.day,
            day_net: self.breaker.day_net,
            halt_alerted_day: self.breaker.halt_alerted_day,
        };
        if let Err(e) = save(path, &state) {
            tracing::warn!(file = %path.display(), error = %e, "day-state write failed; tally is in memory only this run (a restart would lose it)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "day-state-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_load_roundtrips() {
        let path = scratch("roundtrip").join(DAY_STATE_FILE);
        let state = DayState {
            schema: DAY_STATE_SCHEMA,
            day: 20_000,
            day_net: -12.5,
            halt_alerted_day: 20_000,
        };
        save(&path, &state).expect("save");
        assert_eq!(load(&path), Some(state));
    }

    #[test]
    fn newer_schema_refuses_to_adopt_numbers() {
        // A future writer's file is not ours to interpret: fresh day,
        // loudly — never a guessed tally.
        let dir = scratch("future");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(DAY_STATE_FILE);
        std::fs::write(
            &path,
            format!(
                "{{\"schema\":{},\"day\":{},\"day_net\":-99.0,\"halt_alerted_day\":0}}",
                DAY_STATE_SCHEMA + 1,
                20_000
            ),
        )
        .expect("write");
        assert_eq!(load(&path), None, "newer schema degrades to fresh day");
    }

    #[test]
    fn missing_corrupt_and_stale_degrade_to_fresh() {
        let dir = scratch("degrade");
        let path = dir.join(DAY_STATE_FILE);
        assert_eq!(load(&path), None, "missing file is a fresh day");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(&path, "{not json").expect("write");
        assert_eq!(load(&path), None, "corrupt file is a fresh day");
        std::fs::write(&path, "{\"day\":1,\"day_net\":-3.0}").expect("write");
        // Older files predate the card marker AND the schema field.
        assert_eq!(
            load(&path),
            Some(DayState {
                schema: 0,
                day: 1,
                day_net: -3.0,
                halt_alerted_day: 0
            })
        );
    }

    #[test]
    fn book_resumes_today_and_resets_older() {
        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let path = scratch("resume").join(DAY_STATE_FILE);
        let mut book = DayBook::open(Some(path.clone()), now);
        book.record(-7.0, now);
        book.record(2.0, now);
        assert_eq!(book.day_net(), -5.0);
        let state = load(&path).expect("persisted");
        assert_eq!((state.day, state.day_net), (day, -5.0));

        // Restart: same day resumes, the tally must not reset.
        let mut restarted = DayBook::open(Some(path.clone()), now + 60);
        assert_eq!(restarted.day_net(), -5.0);
        assert!(restarted.tripped(3.0, now + 60), "resumed loss still trips");
        restarted.record(-1.0, now + 120);
        assert_eq!(load(&path).expect("persisted").day_net, -6.0);

        // Next UTC day: fresh, and the rollover is persisted.
        let next = (day + 1) * 86_400 + 10;
        let mut rolled = DayBook::open(Some(path.clone()), next);
        assert_eq!(rolled.day_net(), 0.0);
        assert!(!rolled.tripped(3.0, next));
        rolled.record(-1.0, next);
        let state = load(&path).expect("persisted");
        assert_eq!((state.day, state.day_net), (day + 1, -1.0));
    }

    #[test]
    fn halt_card_marker_survives_a_restart() {
        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let path = scratch("halt-card").join(DAY_STATE_FILE);
        let mut book = DayBook::open(Some(path.clone()), now);
        book.record(-10.0, now);
        assert_eq!(book.halt_state(5.0, now), (true, true), "first card");
        let mut restarted = DayBook::open(Some(path), now + 30);
        assert_eq!(
            restarted.halt_state(5.0, now + 30),
            (true, false),
            "restart must not re-fire today's card"
        );
    }

    #[test]
    fn unbacked_book_records_without_touching_disk() {
        let now = 20_000 * 86_400 + 10;
        let mut book = DayBook::open(None, now);
        book.record(-4.0, now);
        assert_eq!(book.day_net(), -4.0);
        assert!(book.tripped(1.0, now));
    }
}
