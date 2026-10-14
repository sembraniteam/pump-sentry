//! Failure backoff and throttled warnings for poll loops.
//!
//! Both tickers (curve + vault) share this: consecutive RPC failures
//! sleep progressively longer (capped) instead of hammering a sick node
//! at full rate, and persistent-but-expected conditions warn at most
//! every few minutes instead of every round.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Extra sleep step per consecutive failure, capped.
const BACKOFF_STEP_SECS: u64 = 5;
/// Backoff ceiling: liveness matters more than politeness past this.
const BACKOFF_MAX_SECS: u64 = 30;
/// Minimum gap between repeated warnings for one condition.
const WARN_INTERVAL_SECS: u64 = 300;

/// Consecutive-failure backoff. `Clone`/shared via `Arc` at the call
/// sites (both tickers are `Clone`).
#[derive(Debug, Default)]
pub struct RoundBackoff {
    failures: AtomicU64,
}

impl RoundBackoff {
    pub fn new() -> Self {
        Self {
            failures: AtomicU64::new(0),
        }
    }

    /// Record a failed round; returns the extra sleep before retrying.
    pub fn fail_sleep(&self) -> Duration {
        let n = self
            .failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        Duration::from_secs((n.saturating_mul(BACKOFF_STEP_SECS)).min(BACKOFF_MAX_SECS))
    }

    /// Record a clean round (resets the climb).
    pub fn success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }
}

/// Warn at most once per interval per call site. Each site owns one
/// `static AtomicU64` slot (zero = never warned). Returns true when the
/// caller should log.
pub fn warn_throttled(slot: &AtomicU64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = slot.load(Ordering::Relaxed);
    if now.saturating_sub(last) < WARN_INTERVAL_SECS {
        return false;
    }
    slot.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_climbs_then_resets() {
        let b = RoundBackoff::new();
        assert_eq!(b.fail_sleep(), Duration::from_secs(5));
        assert_eq!(b.fail_sleep(), Duration::from_secs(10));
        for _ in 0..20 {
            b.fail_sleep();
        }
        assert_eq!(b.fail_sleep(), Duration::from_secs(30));
        b.success();
        assert_eq!(b.fail_sleep(), Duration::from_secs(5));
    }

    #[test]
    fn throttle_fires_once_per_interval() {
        static SLOT: AtomicU64 = AtomicU64::new(0);
        assert!(warn_throttled(&SLOT));
        assert!(!warn_throttled(&SLOT));
    }
}
