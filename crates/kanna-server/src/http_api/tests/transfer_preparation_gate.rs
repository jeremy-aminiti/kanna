//! The first preparation checkpoint a transferred task must clear before its
//! daemon session is ever contacted: `create_task_with_requested_id_and_inputs`
//! (`crates/kanna-server/src/http_api/tasks.rs`) creates the pipeline_item row
//! and git worktree synchronously, then — while still on the blocking pool,
//! before `DaemonClient::connect` is ever called — verifies the freshly
//! prepared worktree's committed head against `transfer_import.headOid` and
//! requires a durable `transferred_task_manifest` row in state `importing`
//! bound to exactly this task id, flipping it to `prepared` only on success.
//!
//! The `#[cfg(test)]` fake-`task_creator` shortcut
//! (`crates/kanna-server/src/http_api/tasks.rs:549`) now only intercepts
//! non-transfer requests, so a transferred create genuinely exercises this
//! gate end to end: real SQLite, a real git worktree, and a real (fake) daemon
//! on a Unix socket standing in for `kanna-daemon`. These are the only two
//! regression tests for it.

use super::*;

/// Builds an isolated `Config` + git repo + SQLite DB for one test, exactly as
/// [`super::create_task::assert_created_task_overrides_reach_daemon_spawn`]
/// does for the equivalent non-transfer daemon-spawn coverage.
struct GateFixture {
    config: Config,
    repo_root: PathBuf,
    daemon_dir: PathBuf,
    socket_path: PathBuf,
}

fn build_gate_fixture(label: &str) -> GateFixture {
    let unique = unique_test_suffix();
    let repo_root =
        crate::test_paths::unique_test_path(&format!("kanna-http-transfer-gate-{label}"));
    init_test_git_repo(&repo_root);
    let daemon_dir =
        crate::test_paths::unique_test_path(&format!("kanna-http-transfer-gate-daemon-{label}"));
    std::fs::create_dir_all(&daemon_dir).unwrap();
    let socket_path = daemon_socket_path_for_dir(&daemon_dir.to_string_lossy());
    let _ = std::fs::remove_file(&socket_path);

    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: daemon_dir.to_string_lossy().to_string(),
        db_path: Db::test_db_path(&format!("http-api-transfer-gate-{label}-{unique}")),
        kanna_cli_path: None,
        desktop_id: "desktop-1".to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: "Studio Mac".to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "127.0.0.1".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file(
            "kanna-pairings-transfer-gate",
            "json",
        ),
    };
    let db = Db::open_for_tests(&config.db_path).unwrap();
    db.insert_test_repo_with_path("repo-1", &repo_root.to_string_lossy(), "Repo One")
        .unwrap();
    drop(db);

    GateFixture {
        config,
        repo_root,
        daemon_dir,
        socket_path,
    }
}

impl GateFixture {
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir_all(&self.daemon_dir);
        let _ = std::fs::remove_dir_all(&self.repo_root);
        let _ = std::fs::remove_file(&self.config.db_path);
    }
}

/// The committed head `commit_oid` will see once the task's worktree is
/// forked from this fixture's freshly initialized repo — the value a real
/// transfer payload would have captured on the source machine.
fn repo_head_oid(repo_root: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn transfer_import_body(transfer_id: &str, head_oid: &str) -> serde_json::Value {
    serde_json::json!({
        "repoId": "repo-1",
        "prompt": "resume the transferred agent",
        "workflowName": TEST_PROVIDER_NEUTRAL_WORKFLOW,
        "agentProvider": "claude",
        "transferImport": {
            "transferId": transfer_id,
            "headOid": head_oid,
            "sourceMachine": "peer-source",
        },
    })
}

async fn put_task(
    app: &axum::Router,
    task_id: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::put(format!("/v1/tasks/{task_id}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// A transferred create whose durable manifest is bound to a *different*
/// local task id — the shape a stale or mismatched import leaves behind, and
/// the "seeded local_task_id" case the checkpoint exists to catch — is
/// refused before the daemon is ever dialed, and leaves no orphaned task
/// behind.
///
/// Before this test, the gate's own failure paths did not roll back the
/// pipeline_item row and git worktree `prepare_task_for_api_with_error` had
/// already created — unlike the transfer-context and imported-inputs gates
/// immediately above them in the same function, which do. A rejected import
/// leaked an orphaned, never-admitted task and worktree on every failure.
/// Fixed alongside this test by routing every failure branch in that block
/// through `rollback_prepared_task_for_api`, matching its siblings.
#[tokio::test]
async fn transfer_import_bound_to_another_task_is_refused_before_any_daemon_contact() {
    let fixture = build_gate_fixture("mismatch");
    let expected_head = repo_head_oid(&fixture.repo_root);

    let db = Db::open(&fixture.config.db_path).unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-mismatch",
        "repo-1",
        Some("decoy0002"),
        &expected_head,
        &expected_head,
    )
    .unwrap();
    drop(db);

    let listener = tokio::net::UnixListener::bind(&fixture.socket_path).unwrap();
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let connected_for_daemon = std::sync::Arc::clone(&connected);
    let daemon = tokio::spawn(async move {
        if listener.accept().await.is_ok() {
            connected_for_daemon.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let app = super::router(Arc::new(super::AppState::new(fixture.config.clone())));
    let (status, body) = put_task(
        &app,
        "abad0001",
        transfer_import_body("transfer-mismatch", &expected_head),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("lacks an importing manifest"), "{body}");

    // The whole request future above already resolved, so nothing could still
    // be racing to dial the daemon afterward: this reads the same fact the
    // request's own control flow already decided.
    assert!(
        !connected.load(std::sync::atomic::Ordering::SeqCst),
        "the gate must reject a mismatched manifest before contacting the daemon at all"
    );
    daemon.abort();

    // The task and worktree `prepare_task_for_api_with_error` created for
    // this rejected attempt must not survive it.
    let db = Db::open(&fixture.config.db_path).unwrap();
    assert!(
        db.get_pipeline_item("abad0001").unwrap().is_none(),
        "a rejected transfer import must not leave an orphaned pipeline_item behind"
    );
    let worktree_path = fixture
        .repo_root
        .join(".kanna-worktrees")
        .join("task-abad0001");
    assert!(
        !worktree_path.exists(),
        "a rejected transfer import must not leave an orphaned git worktree behind: {worktree_path:?}"
    );

    // The decoy manifest itself is untouched by the rejected attempt.
    let (_, _, _, bound_task, manifest_state) = db
        .transferred_task_manifest("transfer-mismatch")
        .unwrap()
        .unwrap();
    assert_eq!(bound_task.as_deref(), Some("decoy0002"));
    assert_eq!(manifest_state, "importing");

    fixture.cleanup();
}

/// The matching valid path: a manifest durably `importing` and bound to
/// exactly the task id being created, with a head that matches what the
/// worktree actually produced, is admitted — the daemon is spawned and the
/// manifest is flipped to `prepared`.
#[tokio::test]
async fn transfer_import_with_a_prepared_manifest_reaches_the_daemon_spawn() {
    use kanna_daemon::protocol::{Command as DaemonCommand, Event as DaemonEvent};
    use tokio::io::{AsyncWriteExt, BufReader};

    let fixture = build_gate_fixture("admitted");
    let expected_head = repo_head_oid(&fixture.repo_root);

    let db = Db::open(&fixture.config.db_path).unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-admitted",
        "repo-1",
        Some("abad0003"),
        &expected_head,
        &expected_head,
    )
    .unwrap();
    drop(db);

    let listener = tokio::net::UnixListener::bind(&fixture.socket_path).unwrap();
    let daemon = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let command = read_test_daemon_command(&mut reader, &mut write_half).await;
        let session_id = match command {
            DaemonCommand::Spawn { session_id, .. } => session_id,
            other => panic!("expected PTY Spawn command, got {other:?}"),
        };
        write_half
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&DaemonEvent::SessionCreated { session_id }).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });

    let app = super::router(Arc::new(super::AppState::new(fixture.config.clone())));
    let (status, body) = put_task(
        &app,
        "abad0003",
        transfer_import_body("transfer-admitted", &expected_head),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created: CreateTaskResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(created.task_id, "abad0003");

    daemon.await.unwrap();

    let db = Db::open(&fixture.config.db_path).unwrap();
    let (_, _, _, bound_task, manifest_state) = db
        .transferred_task_manifest("transfer-admitted")
        .unwrap()
        .unwrap();
    assert_eq!(bound_task.as_deref(), Some("abad0003"));
    assert_eq!(
        manifest_state, "prepared",
        "admission through the checkpoint must flip the manifest from importing to prepared"
    );
    assert!(db.get_pipeline_item("abad0003").unwrap().is_some());

    fixture.cleanup();
}

#[tokio::test]
async fn existing_unprepared_transfer_task_is_refused_before_recovery_spawn() {
    let fixture = build_gate_fixture("recovery-unprepared");
    let expected_head = repo_head_oid(&fixture.repo_root);
    let db = Db::open(&fixture.config.db_path).unwrap();
    db.insert_test_pipeline_item(
        "abad0004",
        "repo-1",
        "resume",
        None,
        "in progress",
        "2026-09-09T00:00:00Z",
    )
    .unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-recovery",
        "repo-1",
        Some("abad0004"),
        &expected_head,
        &expected_head,
    )
    .unwrap();
    drop(db);

    let listener = tokio::net::UnixListener::bind(&fixture.socket_path).unwrap();
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = connected.clone();
    let daemon = tokio::spawn(async move {
        if listener.accept().await.is_ok() {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let app = super::router(Arc::new(super::AppState::new(fixture.config.clone())));
    let (status, body) = put_task(
        &app,
        "abad0004",
        transfer_import_body("transfer-recovery", &expected_head),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!connected.load(std::sync::atomic::Ordering::SeqCst));
    daemon.abort();
    fixture.cleanup();
}

#[tokio::test]
async fn ordinary_put_resume_and_rerun_refuse_unprepared_bound_task() {
    let fixture = build_gate_fixture("ordinary-recovery");
    let head = repo_head_oid(&fixture.repo_root);
    let db = Db::open(&fixture.config.db_path).unwrap();
    db.insert_test_pipeline_item(
        "abad0005",
        "repo-1",
        "resume",
        None,
        "in progress",
        "2026-09-09T00:00:00Z",
    )
    .unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-ordinary",
        "repo-1",
        Some("abad0005"),
        &head,
        &head,
    )
    .unwrap();
    drop(db);
    let listener = tokio::net::UnixListener::bind(&fixture.socket_path).unwrap();
    let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = connected.clone();
    let daemon = tokio::spawn(async move {
        if listener.accept().await.is_ok() {
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let app = super::router(Arc::new(super::AppState::new(fixture.config.clone())));
    let (status, _) = put_task(
        &app,
        "abad0005",
        serde_json::json!({"repoId":"repo-1","prompt":"ordinary retry"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    for path in ["/v1/tasks/abad0005/resume", "/v1/tasks/abad0005/rerun"] {
        let response = app
            .clone()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
    }
    assert!(!connected.load(std::sync::atomic::Ordering::SeqCst));
    daemon.abort();
    fixture.cleanup();
}
