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

/// Load outcome. `Missing` (first boot) is the only fresh-day path.
/// `Damaged` carries the reason and must fail the breaker closed —
/// a corrupt tally must never read as "no losses today" (see
/// `DayBook`).
#[derive(Debug, Clone, PartialEq)]
pub enum DayStateLoad {
    Missing,
    Loaded(DayState),
    Damaged(String),
}

/// Load the tally. `Missing` only when the file was never written;
/// every other failure (unreadable, corrupt, non-finite, newer
/// schema) is `Damaged` — the caller fails closed, never fresh.
pub fn load(path: &Path) -> DayStateLoad {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DayStateLoad::Missing,
        Err(e) => {
            return DayStateLoad::Damaged(format!("unreadable: {e}"));
        }
    };
    match serde_json::from_str::<DayState>(&raw) {
        Ok(state) if state.schema > DAY_STATE_SCHEMA => DayStateLoad::Damaged(format!(
            "written by a NEWER schema ({} > ours {DAY_STATE_SCHEMA}); refusing its numbers",
            state.schema
        )),
        Ok(state) if state.day_net.is_finite() => DayStateLoad::Loaded(state),
        Ok(state) => DayStateLoad::Damaged(format!("non-finite tally (day_net {})", state.day_net)),
        Err(e) => DayStateLoad::Damaged(format!("corrupt: {e}")),
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

/// Live-boot writability probe for the day-state path (AUD-006): a
/// state dir that cannot take a write today is the exact hazard the
/// restart-persistent tally exists for, so live refuses to fund
/// against a tally it cannot record. Probe = short write + delete of
/// a dedicated file next to the tally; the probe file is never the
/// tally itself.
pub fn ensure_writable(path: &Path) -> Result<(), String> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    // A parent that already exists but is not a directory (e.g. the
    // bare log path's dir slot taken by a stray file) is the same
    // hazard: refuse with the same fail-closed wording as an
    // unwritable dir (create_dir_all would say "cannot be created",
    // but the operator-facing problem is identical).
    if dir.exists() && !dir.is_dir() {
        return Err(format!(
            "day-state dir {} is not writable: the path exists but is not a directory (the loss tally must persist; fix permissions/disk before running live)",
            dir.display()
        ));
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!(
            "day-state dir {} cannot be created: {e} (the loss tally must persist; fix permissions/disk before running live)",
            dir.display()
        ));
    }
    let probe = dir.join(format!("{}.probe", DAY_STATE_FILE));
    let payload = b"probe";
    let write = std::fs::write(&probe, payload).and_then(|()| {
        std::fs::read(&probe).and_then(|got| {
            if got == payload {
                Ok(())
            } else {
                Err(std::io::Error::other("probe readback mismatch"))
            }
        })
    });
    let _ = std::fs::remove_file(&probe);
    write.map_err(|e| {
        format!(
            "day-state dir {} is not writable: {e} (the loss tally must persist; fix permissions/disk before running live)",
            dir.display()
        )
    })
}

/// Day-loss breaker with its home on disk: `record` persists (the
/// tally must never be reset by a restart), reads stay pure. A
/// damaged state file poisons the book: entries halt for the rest of
/// the UTC day (loud card on first check), instead of restarting the
/// tally fresh and blowing past the loss cap. The poison is
/// in-memory only (never persisted: a -inf tally is not JSON) and
/// clears on the next UTC day or when the operator deletes the file.
///
/// A failed persist latches `persist_failed` the same way (AUD-006):
/// the in-memory tally stays correct for the run, but the file no
/// longer backs it, so a restart would silently read a missing or
/// older tally as a fresh day and loosen the live breaker. While the
/// latch holds, the book is halted (entries off; exits unaffected)
/// until a successful write or an explicit operator reset of the
/// state dir. The one case this cannot catch is the file being lost
/// while the process is fine and every write since has succeeded —
/// an externally deleted file is indistinguishable from a first
/// boot without a second durable marker, so an operator reset (not a
/// silent "fresh day") is the documented remedy there. Live boot
/// additionally probes state-dir writability before funding (`crate::day_state::ensure_writable`).
#[derive(Debug)]
pub struct DayBook {
    breaker: DailyBreaker,
    path: Option<PathBuf>,
    poisoned: bool,
    /// UTC day the poison was taken (open time). Rollover past this
    /// day clears the poison; `breaker.day` alone cannot tell (open
    /// stamps it to today).
    poisoned_day: u64,
    /// A `save` failed this run (AUD-006): the file may be missing or
    /// stale, so a restart must not read this tally as trustworthy —
    /// the book halts until a successful write lands.
    persist_failed: bool,
}

impl DayBook {
    /// Load the persisted tally when it belongs to the same UTC day,
    /// then arm persistence. `None` = in-memory only (tests, or a run
    /// with no state dir). A stale bucket is a fresh day by definition.
    /// A damaged file poisons the book (entries halt today).
    pub fn open(path: Option<PathBuf>, now_secs: u64) -> Self {
        let mut book = Self {
            breaker: DailyBreaker::new(now_secs),
            path,
            poisoned: false,
            poisoned_day: 0,
            persist_failed: false,
        };
        let Some(path) = book.path.as_deref() else {
            return book;
        };
        match load(path) {
            DayStateLoad::Missing => {}
            DayStateLoad::Loaded(state) => {
                if state.day == now_secs / 86_400 {
                    book.breaker.day_net = state.day_net;
                    book.breaker.halt_alerted_day = state.halt_alerted_day;
                    tracing::info!(file = %path.display(), day_net = state.day_net, "day-loss tally resumed from disk");
                } else {
                    tracing::debug!(file = %path.display(), saved_day = state.day, "day-state from an older day; fresh tally");
                }
            }
            DayStateLoad::Damaged(reason) => {
                book.poisoned = true;
                book.poisoned_day = now_secs / 86_400;
                book.breaker.day_net = f64::NEG_INFINITY;
                tracing::error!(file = %path.display(), reason = %reason, "day-state DAMAGED; entries halted for today (fail-closed: delete the file to resume; a running book clears on UTC rollover)");
            }
        }
        book
    }

    /// Clear a same-run poison once the UTC day rolls over.
    fn maybe_unpoison(&mut self, now_secs: u64) {
        if self.poisoned && now_secs / 86_400 != self.poisoned_day {
            self.poisoned = false;
            self.poisoned_day = 0;
            self.breaker.day_net = 0.0;
        }
    }

    /// True while persistence is broken (corrupt file poison or a
    /// failed write latch, AUD-006).
    fn halted_for_persistence(&self) -> bool {
        self.poisoned || self.persist_failed
    }

    /// Public read of the persistence halt for status publishing: a
    /// poisoned tally is `-inf` (not JSON-finite), so the collector
    /// must publish the damage flag with a finite placeholder instead.
    /// Rolls the poison window first, like every other read.
    pub fn persistence_halted(&mut self, now_secs: u64) -> bool {
        self.maybe_unpoison(now_secs);
        self.halted_for_persistence()
    }

    /// Fold one closed trade in and persist it (write runs on the
    /// blocking pool: a slow fsync never pins a Tokio worker holding
    /// [`crate::pipeline::Pipeline`]'s day-state lock). A failed write
    /// latches halt (AUD-006): the in-memory tally stays authoritative
    /// for the run, but the file no longer backs it, so the breaker
    /// must not keep allowing entries it could not survive a restart
    /// with.
    pub async fn record(&mut self, net_pnl: f64, now_secs: u64) {
        self.maybe_unpoison(now_secs);
        self.breaker.record(net_pnl, now_secs);
        self.persist().await;
    }

    /// Same contract as `DailyBreaker::tripped`; a day rollover
    /// persists (today's zero replaces yesterday's tally). Poisoned =
    /// halted (a corrupt tally must never read as "no losses"); a
    /// failed persist halts the same way until a write succeeds.
    pub async fn tripped(&mut self, cap_usd: f64, now_secs: u64) -> bool {
        self.maybe_unpoison(now_secs);
        if self.halted_for_persistence() {
            return true;
        }
        let rolled = now_secs / 86_400 != self.breaker.day;
        let tripped = self.breaker.tripped(cap_usd, now_secs);
        if rolled {
            self.persist().await;
            if self.persist_failed {
                return true;
            }
        }
        tripped
    }

    /// Same contract as `DailyBreaker::halt_state`; the one-shot card
    /// marker persists so a restart cannot re-fire the day's card.
    /// Poisoned = halted with a firing card (loud, once per day). A
    /// failed persist halts the same way and fires the card once per
    /// run-day until the next write succeeds.
    pub async fn halt_state(&mut self, cap_usd: f64, now_secs: u64) -> (bool, bool) {
        self.maybe_unpoison(now_secs);
        if self.halted_for_persistence() {
            // Loud once per run-day (per-UTC-day one-shot): stamp the
            // card day so the (true, true) edge re-fires exactly once
            // per day even in a long-lived run.
            self.breaker.day = now_secs / 86_400;
            if self.breaker.halt_alerted_day == self.breaker.day {
                return (true, false);
            }
            self.breaker.halt_alerted_day = self.breaker.day;
            return (true, true);
        }
        let before = self.breaker.halt_alerted_day;
        let out = self.breaker.halt_state(cap_usd, now_secs);
        if self.breaker.halt_alerted_day != before {
            self.persist().await;
        }
        out
    }

    /// Realized net PnL today.
    pub fn day_net(&self) -> f64 {
        self.breaker.day_net
    }

    /// Best-effort write of the current scalars. Skipped while
    /// poisoned (a -inf tally is not JSON-serializable; the poison is
    /// in-memory only and clears on UTC rollover). A write failure
    /// latches `persist_failed` (AUD-006): the book halts until a
    /// later write succeeds — a recovered write clears the latch and
    /// the next persistence keeps the breaker honest across restarts.
    /// The open+write+fsync+rename runs on the blocking pool: the
    /// caller holds the pipeline's day-state lock across the await, so
    /// the wait must not happen on a runtime worker (trade-close path).
    async fn persist(&mut self) {
        if self.poisoned {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let state = DayState {
            schema: DAY_STATE_SCHEMA,
            day: self.breaker.day,
            day_net: self.breaker.day_net,
            halt_alerted_day: self.breaker.halt_alerted_day,
        };
        let persisted_path = path.clone();
        let write_path = path.clone();
        let joined = tokio::task::spawn_blocking(move || save(&write_path, &state)).await;
        let outcome = match joined {
            Ok(res) => res,
            Err(join) => Err(std::io::Error::other(format!(
                "day-state writer aborted: {join}"
            ))),
        };
        match outcome {
            Ok(()) => {
                if self.persist_failed {
                    self.persist_failed = false;
                    tracing::info!(
                        file = %path.display(),
                        "day-state write recovered; persistence latch cleared"
                    );
                }
            }
            Err(e) => {
                if !self.persist_failed {
                    tracing::error!(
                        file = %persisted_path.display(), error = %e,
                        "day-state WRITE FAILED: the tally cannot persist, so a restart would read it as lost — entries halt until a write succeeds (exits keep running; fix the state dir)"
                    );
                }
                self.persist_failed = true;
            }
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

    fn loaded(path: &Path) -> DayState {
        match load(path) {
            DayStateLoad::Loaded(state) => state,
            other => panic!("expected Loaded, got {other:?}"),
        }
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
        assert_eq!(load(&path), DayStateLoad::Loaded(state));
    }

    #[test]
    fn newer_schema_refuses_to_adopt_numbers() {
        // A future writer's file is not ours to interpret: damaged
        // (fail closed), never a guessed tally.
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
        assert!(
            matches!(load(&path), DayStateLoad::Damaged(_)),
            "newer schema is damaged, never fresh"
        );
    }

    #[test]
    fn missing_is_fresh_but_corrupt_is_damaged() {
        let dir = scratch("degrade");
        let path = dir.join(DAY_STATE_FILE);
        assert_eq!(
            load(&path),
            DayStateLoad::Missing,
            "missing file is a fresh day"
        );
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Unreadable (a directory in the file's place) is damaged, not
        // fresh: a broken path must never look like "no losses".
        let blocked = dir.join("blocked");
        std::fs::create_dir(&blocked).expect("mkdir");
        assert!(
            matches!(load(&blocked), DayStateLoad::Damaged(_)),
            "unreadable path is damaged"
        );
        std::fs::write(&path, "{not json").expect("write");
        assert!(
            matches!(load(&path), DayStateLoad::Damaged(_)),
            "corrupt file is damaged, never fresh"
        );
        std::fs::write(&path, "{\"day\":1,\"day_net\":-3.0}").expect("write");
        // Older files predate the card marker AND the schema field.
        assert_eq!(
            load(&path),
            DayStateLoad::Loaded(DayState {
                schema: 0,
                day: 1,
                day_net: -3.0,
                halt_alerted_day: 0
            })
        );
    }

    #[tokio::test]
    async fn damaged_file_poisons_the_book_until_rollover() {
        // A corrupt tally halts entries for the day (fail closed) with
        // one loud card — a restart cannot silently reset the loss.
        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let dir = scratch("poison");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(DAY_STATE_FILE);
        std::fs::write(&path, "{not json").expect("write");
        let mut book = DayBook::open(Some(path.clone()), now);
        assert!(book.poisoned, "damaged file poisons the book");
        assert!(
            book.tripped(1_000_000.0, now).await,
            "poisoned book halts entries"
        );
        assert_eq!(
            book.halt_state(1_000_000.0, now).await,
            (true, true),
            "first card fires"
        );
        assert_eq!(
            book.halt_state(1_000_000.0, now + 30).await,
            (true, false),
            "no card spam after the first"
        );
        // A restart with the same damaged file stays halted.
        let mut restarted = DayBook::open(Some(path.clone()), now + 60);
        assert!(restarted.poisoned);
        assert!(restarted.tripped(1_000_000.0, now + 60).await);
        // Next UTC day, same damaged file: still halted — a restart
        // cannot prove today's tally is zero (an earlier process may
        // have traded and crashed). Fail closed until the operator
        // clears the file.
        let next = (day + 1) * 86_400 + 10;
        let mut rolled = DayBook::open(Some(path.clone()), next);
        assert!(rolled.poisoned);
        assert!(
            rolled.tripped(1_000_000.0, next).await,
            "damaged file still halts after restart"
        );
        // Operator clears the file: fresh day resumes.
        std::fs::remove_file(&path).expect("operator clears the file");
        let mut cleared = DayBook::open(Some(path.clone()), next + 60);
        assert!(!cleared.poisoned);
        assert!(!cleared.tripped(1_000_000.0, next + 60).await);
        cleared.record(-1.0, next + 60).await;
        assert_eq!(
            loaded(&path).day_net,
            -1.0,
            "recovery persists a clean tally"
        );
        // A running (long-lived) book crossing midnight with damage:
        // yesterday's unknown tally is moot, so the poison clears and
        // the clean tally persists over the damaged file.
        std::fs::write(&path, "{not json").expect("re-damage");
        book.tripped(1_000_000.0, next + 120).await;
        assert!(!book.poisoned, "UTC rollover clears a running poison");
        assert!(!book.tripped(1_000_000.0, next + 120).await);
        book.record(-2.0, next + 120).await;
        let state = loaded(&path);
        assert_eq!((state.day, state.day_net), (day + 1, -2.0));
    }

    #[tokio::test]
    async fn book_resumes_today_and_resets_older() {
        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let path = scratch("resume").join(DAY_STATE_FILE);
        let mut book = DayBook::open(Some(path.clone()), now);
        book.record(-7.0, now).await;
        book.record(2.0, now).await;
        assert_eq!(book.day_net(), -5.0);
        let state = loaded(&path);
        assert_eq!((state.day, state.day_net), (day, -5.0));

        // Restart: same day resumes, the tally must not reset.
        let mut restarted = DayBook::open(Some(path.clone()), now + 60);
        assert_eq!(restarted.day_net(), -5.0);
        assert!(
            restarted.tripped(3.0, now + 60).await,
            "resumed loss still trips"
        );
        restarted.record(-1.0, now + 120).await;
        assert_eq!(loaded(&path).day_net, -6.0);

        // Next UTC day: fresh, and the rollover is persisted.
        let next = (day + 1) * 86_400 + 10;
        let mut rolled = DayBook::open(Some(path.clone()), next);
        assert_eq!(rolled.day_net(), 0.0);
        assert!(!rolled.tripped(3.0, next).await);
        rolled.record(-1.0, next).await;
        let state = loaded(&path);
        assert_eq!((state.day, state.day_net), (day + 1, -1.0));
    }

    #[tokio::test]
    async fn halt_card_marker_survives_a_restart() {
        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let path = scratch("halt-card").join(DAY_STATE_FILE);
        let mut book = DayBook::open(Some(path.clone()), now);
        book.record(-10.0, now).await;
        assert_eq!(book.halt_state(5.0, now).await, (true, true), "first card");
        let mut restarted = DayBook::open(Some(path), now + 30);
        assert_eq!(
            restarted.halt_state(5.0, now + 30).await,
            (true, false),
            "restart must not re-fire today's card"
        );
    }

    #[tokio::test]
    async fn unbacked_book_records_without_touching_disk() {
        let now = 20_000 * 86_400 + 10;
        let mut book = DayBook::open(None, now);
        book.record(-4.0, now).await;
        assert_eq!(book.day_net(), -4.0);
        assert!(book.tripped(1.0, now).await);
    }

    #[tokio::test]
    async fn failed_persist_latches_halt_until_a_write_succeeds() {
        // AUD-006: a disk-full/read-only/permission write failure
        // after a correct boot cannot loosen the live breaker. The
        // tally is in memory for this run, but a restart would read a
        // missing/older file as a fresh day, so the book halts until a
        // write lands. Scenario: the day-state file is missing (a
        // fresh-day load at open) and the directory is read-only, so
        // the first `save` fails: exactly the "unbacked but halted"
        // case. (A path under a regular file is a different failure —
        // `load` is damaged, which poisons the book at open; covered
        // by `damaged_file_poisons_the_book_until_rollover`.)
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let dir = scratch("latch");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(DAY_STATE_FILE);
        assert!(
            matches!(load(&path), DayStateLoad::Missing),
            "fresh-day load"
        );
        #[cfg(unix)]
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555))
            .expect("make dir read-only");
        let mut book = DayBook::open(Some(path.clone()), now);
        book.record(-2.0, now).await;
        assert!(book.persist_failed, "failed write latches");
        assert!(
            book.tripped(1_000_000.0, now).await,
            "a failed persist halts entries (fail-closed)"
        );
        assert_eq!(
            book.halt_state(1_000_000.0, now).await,
            (true, true),
            "loud card fires"
        );
        assert_eq!(
            book.halt_state(1_000_000.0, now + 30).await,
            (true, false),
            "no card spam"
        );
        // Still-losing in-memory tally must not be readable as a win:
        // the file never got the number, but the run knows.
        assert!(
            book.tripped(1.9, now + 60).await,
            "in-memory loss still trips"
        );
        // A later `record` retries the write (the latch is a state,
        // not a skip-list): it must not loosen the breaker even while
        // still failing.
        book.record(-1.0, now + 80).await;
        assert!(
            book.tripped(2.9, now + 80).await,
            "still failing writes keep the halt"
        );
        // Recovery: make the dir writable and record again — the
        // successful write clears the latch (items resume).
        #[cfg(unix)]
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore dir");
        book.record(-0.1, now + 90).await;
        assert!(!book.persist_failed, "a successful write clears the latch");
        assert!(
            !book.tripped(1_000_000.0, now + 90).await,
            "entries resume after recovery (cap far above the tally)"
        );
        assert_eq!(loaded(&path).day_net, -3.1, "recovered tally persisted");
    }

    #[test]
    fn live_boot_refuses_an_unwritable_day_state_path() {
        // AUD-006: live cannot boot against a tally it cannot write.
        let dir = scratch("probe");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let tally = dir.join(DAY_STATE_FILE);
        assert!(ensure_writable(&tally).is_ok());
        // The probe leaves nothing behind.
        assert!(!dir.join(format!("{DAY_STATE_FILE}.probe")).exists());
        // A probe file left behind by a crashed boot never blocks: the
        // probe overwrites it, and the real tally path is untouched.
        std::fs::write(dir.join(format!("{DAY_STATE_FILE}.probe")), b"stale")
            .expect("write stale probe");
        assert!(
            ensure_writable(&tally).is_ok(),
            "stale probe does not block"
        );
        assert!(!tally.exists(), "the probe never touches the tally itself");
        // A parent that is a regular file (state dir slot taken by a
        // stray file) refuses with the not-writable wording.
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"").expect("file");
        let err =
            ensure_writable(&blocked.join(DAY_STATE_FILE)).expect_err("blocked path must refuse");
        assert!(err.contains("not writable"), "{err}");
        // A read-only directory refuses the write probe directly.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = dir.join("read-only");
            std::fs::create_dir_all(&ro).expect("mkdir");
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).expect("chmod");
            let err =
                ensure_writable(&ro.join(DAY_STATE_FILE)).expect_err("read-only dir must refuse");
            assert!(err.contains("not writable"), "{err}");
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755))
                .expect("restore for cleanup");
        }
    }

    #[tokio::test]
    async fn day_rollover_persists_zero_and_survives_write_failures() {
        // The latch never masks a fresh day: a normal rollover still
        // persists zero when the dir is writable and unpoisons cleanly.
        let day = 20_000u64;
        let now = day * 86_400 + 10;
        let path = scratch("rollover-latch").join(DAY_STATE_FILE);
        let mut book = DayBook::open(Some(path.clone()), now);
        book.record(-8.0, now).await;
        let next = (day + 1) * 86_400 + 10;
        assert!(!book.tripped(1.0, next).await, "fresh day is not halted");
        assert_eq!(loaded(&path).day_net, 0.0, "rollover persisted zero");
        assert!(!book.persist_failed);
    }
}
