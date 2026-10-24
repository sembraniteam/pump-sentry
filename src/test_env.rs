//! Test-only process-global environment/CWD guard .
//!
//! Process-global environment variables and the working directory are
//! shared by every test thread in this binary. All mutations from `src/`
//! tests must go through [`lock`] plus [`scoped`]/[`chdir`], so that
//! parallel tests in the same test binary never observe each other's
//! mutations (distinct `Mutex` instances per module protected nothing).

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialize all process-global env/CWD mutations in this test binary.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // Poison-tolerant on purpose: the guarded region may panic by
    // design (e.g. restore-on-unwind tests), and a poisoned test-env
    // lock must not cascade failures into every other test thread.
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Restore saved variables on drop, even while unwinding from a panic.
struct Restore {
    saved: Vec<(String, Option<String>)>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}

/// Hold the guard lock and apply `vars` for the guard's lifetime.
///
/// `Some(v)` sets the variable, `None` removes it. The pre-call value of
/// every named variable (set or absent) is restored on drop — including
/// when the guarded region panics.
pub(crate) fn scoped(vars: &[(&str, Option<&str>)]) -> Scoped {
    // Take the lock BEFORE snapshotting or mutating: otherwise another
    // guarded test could be mutated underneath while we build this guard.
    let guard = lock();
    let saved = vars
        .iter()
        .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
        .collect();
    for (k, v) in vars {
        match v {
            Some(v) => unsafe { std::env::set_var(k, v) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    Scoped {
        _restore: Restore { saved },
        _lock: guard,
    }
}

/// Guard holding the test-env lock plus the env restore.
///
/// Field order matters: Rust drops fields in declaration order, so
/// `_restore` runs first and the environment is back to its saved
/// values before `_lock` releases the mutex.
pub(crate) struct Scoped {
    _restore: Restore,
    _lock: MutexGuard<'static, ()>,
}

/// Change the working directory for the guard's lifetime; the previous
/// directory is restored on drop even while unwinding.
///
/// The caller must already hold [`lock`] (chdir is process-global).
pub(crate) fn chdir(dir: &std::path::Path) -> std::io::Result<CwdGuard> {
    let previous = std::env::current_dir()?;
    std::env::set_current_dir(dir)?;
    Ok(CwdGuard {
        previous: Some(previous),
    })
}

pub(crate) struct CwdGuard {
    previous: Option<std::path::PathBuf>,
}

impl CwdGuard {
    /// Explicit early restore; the original directory on drop as well.
    pub(crate) fn restore(&mut self) -> std::io::Result<()> {
        if let Some(p) = self.previous.take() {
            std::env::set_current_dir(p)?;
        }
        Ok(())
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helper restores values even when the
    /// guarded region unwinds from a panic.
    #[test]
    fn scoped_restores_on_unwind() {
        let saved = std::env::var("PS_TEST_ENV_UNWIND").ok();
        let result = std::panic::catch_unwind(|| {
            let _guard = scoped(&[("PS_TEST_ENV_UNWIND", Some("inner"))]);
            assert_eq!(std::env::var("PS_TEST_ENV_UNWIND").as_deref(), Ok("inner"));
            panic!("boom");
        });
        assert!(result.is_err(), "expected the panic to propagate");
        let _guard = lock();
        assert_eq!(std::env::var("PS_TEST_ENV_UNWIND").ok(), saved);
        // Re-assert cleanup in case the var did not exist before.
        unsafe { std::env::remove_var("PS_TEST_ENV_UNWIND") };
    }

    #[test]
    fn scoped_sets_removes_and_restores() {
        {
            let _guard = scoped(&[("PS_TEST_ENV_SCOPED", Some("outer"))]);
            assert_eq!(std::env::var("PS_TEST_ENV_SCOPED").as_deref(), Ok("outer"));
        }
        {
            let _guard = scoped(&[("PS_TEST_ENV_SCOPED", None)]);
            assert_eq!(std::env::var("PS_TEST_ENV_SCOPED").ok(), None);
        }
        assert_eq!(std::env::var("PS_TEST_ENV_SCOPED").ok(), None);
    }
}
