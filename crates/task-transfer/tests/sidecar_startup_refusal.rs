//! A sidecar that refuses to start must reach the test as its own words.
//!
//! `crates/task-transfer/tests/support/sidecar.rs` is what stands between a
//! refused child and a suite that waits forever, so the coordination it does
//! needs process-level coverage of its own: the failure being guarded lives in
//! a child process, two pipes and a detached reader thread, and no unit test
//! sees any of them. These spawn a real sidecar **through that helper** and
//! take each of the three paths a test can be on when the child is already
//! gone — a pending side effect, a pending control response, and a control
//! write — asserting each one fails in bounded time carrying the child's own
//! refusal.
//!
//! Two of those paths hide the refusal if the helper is careless. A control
//! write onto a closed pipe fails with `BrokenPipe`, which explains nothing;
//! and the child's exit status is readable the instant it exits, while its
//! stderr is still travelling through a pipe into a thread that may not have
//! been scheduled — so a diagnostic that reads the buffer on seeing the exit
//! can report `<none>` and lose the reason entirely.
//!
//! The refusal used here names no protected path: pointing `KANNA_DB_PATH` at
//! a SQLite URI is refused by the same `database_access` check on the same
//! line of `RuntimeConfig::from_env` as the desktop-path refusal these tests
//! exist for, without this suite going anywhere near the operator's database.
//! It therefore needs no sandbox fence and runs on every platform. The
//! protected path itself is probed by `sidecar_database_guard.rs`, inside the
//! fence, where it belongs.

#[path = "support/sidecar.rs"]
mod sidecar;

use kanna_task_transfer::protocol::ControlRequest;
use sidecar::SidecarProcess;
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::time::{Duration, Instant};

/// A refused child is gone in milliseconds, so every failure here is a report
/// rather than a wait. The bound is generous because what is under test is
/// that it is bounded at all — the defect it replaces never returned.
const REFUSAL_REPORT_BOUND: Duration = Duration::from_secs(30);

/// A sidecar spawned exactly the way the suites spawn one, that will not start.
fn refused_sidecar(temp: &Path) -> SidecarProcess {
    SidecarProcess::spawn(temp, |command| {
        command.env("KANNA_DB_PATH", "file:kanna-v2.db?mode=rwc");
    })
}

fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else {
        String::from("<non-string panic payload>")
    }
}

/// Every path must say what the sidecar said, not merely that it is unhappy.
fn assert_carries_the_refusal(path: &str, elapsed: Duration, payload: Box<dyn Any + Send>) {
    let message = panic_message(&payload);
    assert!(
        elapsed < REFUSAL_REPORT_BOUND,
        "{path} took {elapsed:?} to report a child that had already exited: {message}"
    );
    assert!(
        message.contains("REFUSED:"),
        "{path} lost the guard's refusal: {message}"
    );
    assert!(
        message.contains("database access requires a filesystem path"),
        "{path} reported a refusal without its reason: {message}"
    );
    assert!(
        !message.contains("sidecar stderr: <none>"),
        "{path} read the child's stderr before its reader finished: {message}"
    );
}

#[test]
fn a_pending_side_effect_reports_the_refused_child_that_will_never_cause_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut sidecar = refused_sidecar(temp.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let started = Instant::now();
    let failure = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(sidecar.expect_alive(
            "peer work that needs a running sidecar",
            std::future::pending::<()>(),
        ))
    }))
    .expect_err("waiting on a sidecar that exited must fail, not hang");

    assert_carries_the_refusal("a pending side effect", started.elapsed(), failure);
}

#[test]
fn a_pending_control_response_reports_the_refused_child_instead_of_a_closed_pipe() {
    let temp = tempfile::tempdir().unwrap();
    let mut sidecar = refused_sidecar(temp.path());

    // The child's stdout reaches EOF on its own schedule, independently of the
    // stderr this failure has to carry.
    let started = Instant::now();
    let failure = catch_unwind(AssertUnwindSafe(|| {
        sidecar.next_response("a control response that will never come")
    }))
    .expect_err("a control response from a sidecar that exited must fail");

    assert_carries_the_refusal("a pending control response", started.elapsed(), failure);
}

#[test]
fn a_control_write_after_the_child_exits_reports_the_refusal_not_the_errno() {
    let temp = tempfile::tempdir().unwrap();
    let mut sidecar = refused_sidecar(temp.path());
    assert!(
        sidecar.wait_for_exit(Duration::from_secs(20)),
        "a sidecar refused at startup must exit"
    );

    let started = Instant::now();
    let failure = catch_unwind(AssertUnwindSafe(|| {
        sidecar.write_control(&ControlRequest::ListPeerTaskSnapshots {
            request_id: "probe".into(),
        })
    }))
    .expect_err("writing to a sidecar that exited must fail");

    assert_carries_the_refusal("a control write after exit", started.elapsed(), failure);
}
