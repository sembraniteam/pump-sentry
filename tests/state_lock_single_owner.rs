//! Two trader processes must never share a state
//! dir. A second `run` on an owned state dir must refuse before it
//! reads the config, loads the wallet, or touches the WAL/day-state.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};

/// Spawn `pump-sentry run` against an owned state dir and assert it
/// exits fast with the single-owner refusal and no state writes.
#[test]
fn second_run_refuses_to_start_while_state_dir_is_owned() {
    let exe = env!("CARGO_BIN_EXE_pump-sentry");
    let dir = tempfile::tempdir().expect("tmpdir");
    let log = dir.path().join("bot.log");

    // Hold the state-dir lock exactly like a running trader: exclusive
    // flock on `<state>/bot.lock`. Same primitive `run_pipeline` uses,
    // so this proves the collision path, not a reimplementation.
    let lock_path = dir.path().join("bot.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("lock file");
    // SAFETY: flock on an owned fd; only probes exclusion.
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "test must hold the lock");

    let mut child = Command::new(exe)
        .arg("run")
        .env("LOG_FILE", &log)
        .env("PID_FILE", dir.path().join("bot.pid"))
        .env("CONFIG_FILE", dir.path().join("config.toml"))
        .env("IS_LIVE", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second run");

    // The refusal is immediate; bound the wait so a regression cannot
    // hang the suite forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("poll") {
            Some(status) => break status,
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "second run must exit promptly, not run alongside the lock holder"
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };
    assert!(
        !status.success(),
        "a second trader on the same state dir must exit nonzero"
    );

    // The refusal precedes every state read/write (the second process
    // never reaches wallet/WAL load).
    let mut errbuf = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut errbuf);
    }
    let stderr = String::from_utf8_lossy(&errbuf);
    assert!(
        stderr.contains("state dir"),
        "refusal must name the state dir: {stderr}"
    );
    for name in [
        "config.toml",
        "day-state.json",
        "live-wal.jsonl",
        "journal.jsonl",
    ] {
        assert!(
            !dir.path().join(name).exists(),
            "refused process must not create {name}"
        );
    }
}
