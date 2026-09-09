//! A sidecar that may not open its database must say so and exit.
//!
//! `RuntimeConfig::from_env` falls back to the desktop's production database
//! when nothing names one, and `kanna_runtime_defaults::database_access`
//! refuses that path for an isolated, test or worktree process. The refusal
//! itself is correct; what made it expensive is that it is invisible from
//! outside — the sidecar prints it, exits, and every wait on the work it was
//! going to do simply never ends. This pins the visible half: the refusal
//! reaches stderr and the process is gone in moments, so a caller waiting on
//! the sidecar fails with a reason instead of stalling.
//!
//! The OS sandbox below is independent of Kanna's guard: the process cannot
//! read or write the owner's SQLite database even if the guard were removed.
#![cfg(target_os = "macos")]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Well above the moment a startup refusal takes, and far below the wait a
/// caller stalled on the missing sidecar would otherwise serve out.
const REFUSAL_DEADLINE: Duration = Duration::from_secs(20);

#[test]
fn a_sidecar_with_no_database_of_its_own_refuses_immediately_instead_of_running() {
    let fixture = tempfile::tempdir().unwrap();
    let protected = kanna_runtime_defaults::database_access::production_database_paths().unwrap();
    // Use the OS account even when the surrounding test runner changed HOME:
    // HOME/Library/Application Support/build.kanna/kanna-v2.db.
    let account_home = protected[0].ancestors().nth(4).unwrap();

    // Deny all SQLite access, whatever path is eventually resolved. This is a
    // test safety fence, not the product protection being asserted.
    let profile = r#"(version 1)
        (allow default)
        (deny file-read-data file-write* (regex #".*\.(db|sqlite)(-wal|-shm|-journal)?$"))"#;
    let sentinel = fixture.path().join("fence.db");
    std::fs::write(&sentinel, b"untouched").unwrap();
    let fence = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", profile, "/usr/bin/touch"])
        .arg(&sentinel)
        .output()
        .expect("macOS sandbox must be available before attempting the regression");
    assert!(
        !fence.status.success(),
        "OS database fence must reject writes"
    );

    // Everything the sidecar needs except a database: the only thing left to
    // resolve is the desktop fallback the guard exists to refuse.
    let mut child = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", profile])
        .arg(env!("CARGO_BIN_EXE_kanna-task-transfer"))
        .env_clear()
        .env("HOME", account_home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("KANNA_TRANSFER_ROOT", fixture.path())
        .env(
            "KANNA_TRANSFER_REGISTRY_DIR",
            fixture.path().join("registry"),
        )
        .env("KANNA_TRANSFER_PEER_ID", "peer-guard-probe")
        .env("KANNA_TRANSFER_DISPLAY_NAME", "Guard Probe")
        .env("KANNA_TRANSFER_DISCOVERY", "registry")
        .env("KANNA_TRANSFER_PORT", "0")
        .env("KANNA_DAEMON_DIR", fixture.path().join("daemon"))
        .current_dir(fixture.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sidecar binary should spawn");

    let deadline = Instant::now() + REFUSAL_DEADLINE;
    let exited = loop {
        match child.try_wait().expect("sidecar status should be readable") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    if exited.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("sidecar kept running without a database it is allowed to open; a caller waiting on it would never be told why");
    }
    let output = child
        .wait_with_output()
        .expect("sidecar output should be readable");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !exited.expect("checked above").success(),
        "sidecar must not report success after refusing its database: {stderr}"
    );
    assert!(
        stderr.contains("REFUSED:")
            && stderr.contains(kanna_runtime_defaults::DEFAULT_DB_NAME)
            && stderr.contains("Supply an isolated database path"),
        "the refusal must name the guard and the path it protects: {stderr}"
    );
    assert_eq!(std::fs::read(sentinel).unwrap(), b"untouched");
}
