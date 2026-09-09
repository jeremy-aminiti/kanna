//! The activity debounce spans three processes: the daemon classifies a
//! terminal frame, `kanna-server` folds that verdict into `pipeline_item
//! .activity`, and `kanna-mcp` is what an orchestrator actually reads. A
//! smoother proven only against a mock server proves nothing about that chain,
//! so these tests drive the real one — see `common/mod.rs` for the harness.
//!
//! `crates/kanna-server/src/terminal_watcher.rs` keeps a narrower unit test of
//! the same premise next to the watcher itself; this file is what proves the
//! cross-process timing.

mod common;

use common::{await_stored_activity, start_chain, ACTIVITY_CONFIRM_DELAY, EVENTUAL_PROGRESS_GUARD};
use serde_json::json;
use std::time::{Duration, Instant};

const TASK_ID: &str = "task-debounce";

#[tokio::test]
async fn a_spurious_idle_frame_between_busy_frames_preserves_unread_output() {
    let (server, daemon, mut mcp) = start_chain("spurious-idle", TASK_ID).await;

    // The dropped busy marker: one frame classified idle while the agent is
    // mid-turn. Waiting for it to land proves the misread really did reach the
    // read surface an orchestrator polls.
    daemon.classify(TASK_ID, "idle");
    await_stored_activity(&server, TASK_ID, "unread").await;

    let started = Instant::now();
    mcp.call_get_task(2, TASK_ID);
    // The agent was never idle: the next frame carries the marker again, well
    // inside the confirmation window.
    tokio::time::sleep(Duration::from_millis(200)).await;
    daemon.classify(TASK_ID, "busy");
    let task = mcp.recv_task();
    let elapsed = started.elapsed();

    assert_eq!(
        task["activity"],
        json!("unread"),
        "a fresh busy frame must not mark unread output read"
    );
    // A first read that had seen `working` would have returned immediately, so
    // this also proves that the stopped-looking value was confirmed rather
    // than returned from the first sample.
    assert!(
        elapsed >= ACTIVITY_CONFIRM_DELAY,
        "the answer must have come from a confirmation read, not from a lucky first read (took {elapsed:?})"
    );
}

#[tokio::test]
async fn a_spurious_idle_frame_between_busy_frames_returns_working_after_output_is_read() {
    let (server, daemon, mut mcp) = start_chain("spurious-idle-read", TASK_ID).await;

    daemon.classify(TASK_ID, "idle");
    await_stored_activity(&server, TASK_ID, "unread").await;

    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/tasks/{TASK_ID}/actions/mark-read",
            server.base_url
        ))
        .send()
        .await
        .expect("mark output read");
    assert!(
        response.status().is_success(),
        "mark-read failed: {response:?}"
    );
    await_stored_activity(&server, TASK_ID, "idle").await;

    let started = Instant::now();
    mcp.call_get_task(2, TASK_ID);
    tokio::time::sleep(Duration::from_millis(200)).await;
    daemon.classify(TASK_ID, "busy");
    let task = mcp.recv_task();
    let elapsed = started.elapsed();

    assert_eq!(
        task["activity"],
        json!("working"),
        "a busy task whose output was read must return to the working display state"
    );
    assert!(
        elapsed >= ACTIVITY_CONFIRM_DELAY,
        "the answer must have come from a confirmation read, not a lucky first read (took {elapsed:?})"
    );
}

#[tokio::test]
async fn a_genuine_stop_is_reported_within_the_confirmation_delay() {
    let (server, daemon, mut mcp) = start_chain("genuine-stop", TASK_ID).await;

    // The agent really stopped: nothing re-classifies it busy afterwards.
    daemon.classify(TASK_ID, "idle");
    await_stored_activity(&server, TASK_ID, "unread").await;

    let started = Instant::now();
    mcp.call_get_task(2, TASK_ID);
    // The response itself is the confirmation-complete event. Thirty seconds
    // contains a wedged cross-process fixture; scheduler delay is not a
    // debounce regression.
    let task = mcp.recv_task_within(EVENTUAL_PROGRESS_GUARD);
    let elapsed = started.elapsed();

    assert_eq!(
        task["activity"],
        json!("unread"),
        "a stop that holds keeps its own activity value: {task}"
    );
    assert!(
        elapsed >= ACTIVITY_CONFIRM_DELAY,
        "a stop is only reported after it is confirmed (took {elapsed:?})"
    );
}
