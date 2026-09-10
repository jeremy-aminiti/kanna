//! The first preparation checkpoint a transferred task must clear before its
//! daemon session is ever contacted: `create_task_with_requested_id_and_inputs`
//! (`crates/kanna-server/src/http_api/tasks.rs`) creates the pipeline_item row
//! and git worktree synchronously, installs context/history/inputs, then —
//! before `DaemonClient::connect` is ever called — invokes the transfer
//! engine's full Git/SQLite read-back proof. That proof atomically records the
//! content commitment and flips the correctly bound manifest to `prepared`.
//!
//! The transfer-only in-process entry genuinely exercises this gate end to
//! end: real SQLite, a real git worktree, and a real (fake) daemon on a Unix
//! socket standing in for `kanna-daemon`.

use super::*;
use crate::http_api::create_transferred_task_in_process;

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

async fn create_transferred_task(
    fixture: &GateFixture,
    task_id: &str,
    transfer_id: &str,
    head_oid: &str,
    base_oid: &str,
    mut body: serde_json::Value,
) -> (StatusCode, String) {
    for (suffix, oid) in [("head", head_oid), ("base", base_oid)] {
        let reference = format!("refs/kanna/transfers/{transfer_id}/{head_oid}/{suffix}");
        let output = Command::new("git")
            .args(["update-ref", &reference, oid])
            .current_dir(&fixture.repo_root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let workflow_definition = std::fs::read_to_string(
        fixture
            .repo_root
            .join(".kanna/workflows")
            .join(format!("{TEST_PROVIDER_NEUTRAL_WORKFLOW}.json")),
    )
    .unwrap();
    body["transferImport"]["workflowDefinition"] =
        serde_json::Value::String(workflow_definition.clone());
    body["transferImport"]["baseOid"] = serde_json::Value::String(base_oid.to_owned());
    body["baseRef"] = serde_json::Value::String(format!(
        "refs/kanna/transfers/{transfer_id}/{head_oid}/base"
    ));
    // The transfer's pinned definition is authoritative; do not let the
    // ordinary workflow-name field overwrite pipeline_def with a name.
    body["workflowName"] = serde_json::Value::Null;
    let request: crate::mobile_api::CreateTaskRequest = serde_json::from_value(body).unwrap();
    let source_payload =
        crate::transfer_engine::payload::parse_outgoing_transfer_payload(&serde_json::json!({
            "target_peer_id": "peer-destination",
            "task": {
                "cloud_task_id": format!("cloud-{transfer_id}"),
                "source_peer_id": "peer-source",
                "source_task_id": "source-task",
                "local_task_id": task_id,
                "resume_session_id": null,
                "prompt": "resume the transferred agent",
                "stage": "in progress",
                "branch": "refs/heads/source-task",
                "head_oid": head_oid,
                "base_oid": base_oid,
                "workflow_definition": workflow_definition,
                "pipeline": TEST_PROVIDER_NEUTRAL_WORKFLOW,
                "agent_type": "pty",
                "agent_provider": "claude"
            },
            "repo": {
                "mode": "task-bundle",
                "bundle": {
                    "artifact_id": "unused-repository-artifact",
                    "filename": "transfer.bundle",
                    "ref_name": "refs/heads/source-task",
                    "base_ref_name": "refs/heads/main"
                }
            },
            "input_ledger": {
                "artifact_id": "unused-input-artifact",
                "filename": crate::transfer_engine::payload::TASK_INPUT_LEDGER_FILENAME,
                "sha256": "c".repeat(64),
                "count": 0
            },
            "artifacts": []
        }))
        .unwrap();
    let state = Arc::new(super::AppState::new(fixture.config.clone()));
    match create_transferred_task_in_process(
        state,
        request,
        task_id.to_string(),
        Vec::new(),
        source_payload,
    )
    .await
    {
        Ok(response) => (StatusCode::OK, serde_json::to_string(&response).unwrap()),
        Err(error) => error,
    }
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

    let (status, body) = create_transferred_task(
        &fixture,
        "abad0001",
        "transfer-mismatch",
        &expected_head,
        &expected_head,
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
/// exactly the task id being created is fully proved — including context and
/// the empty ordered ledger — before the daemon is spawned.
#[tokio::test]
async fn transfer_import_records_complete_proof_before_daemon_spawn() {
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
    let daemon_db_path = fixture.config.db_path.clone();
    let daemon = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let command = read_test_daemon_command(&mut reader, &mut write_half).await;
        let session_id = match command {
            DaemonCommand::Spawn { session_id, .. } => session_id,
            other => panic!("expected PTY Spawn command, got {other:?}"),
        };
        let db = Db::open(&daemon_db_path).unwrap();
        assert!(
            db.transferred_task_manifest_content_commitment("transfer-admitted")
                .unwrap()
                .is_some(),
            "daemon observed Spawn before the complete transfer proof"
        );
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

    let (status, body) = create_transferred_task(
        &fixture,
        "abad0003",
        "transfer-admitted",
        &expected_head,
        &expected_head,
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
async fn interrupted_transfer_preparation_is_completed_before_one_recovery_spawn() {
    use kanna_daemon::protocol::{Command as DaemonCommand, Event as DaemonEvent};
    use tokio::io::{AsyncWriteExt, BufReader};

    let fixture = build_gate_fixture("recovery-unprepared");
    let expected_head = repo_head_oid(&fixture.repo_root);
    let db = Db::open(&fixture.config.db_path).unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-recovery",
        "repo-1",
        Some("abad0004"),
        &expected_head,
        &expected_head,
    )
    .unwrap();
    let mut interrupted_body = transfer_import_body("transfer-recovery", &expected_head);
    interrupted_body["transferImport"]["workflowDefinition"] = serde_json::Value::String(
        std::fs::read_to_string(
            fixture
                .repo_root
                .join(".kanna/workflows")
                .join(format!("{TEST_PROVIDER_NEUTRAL_WORKFLOW}.json")),
        )
        .unwrap(),
    );
    let interrupted_request: crate::mobile_api::CreateTaskRequest =
        serde_json::from_value(interrupted_body).unwrap();
    let prepared = crate::task_creator::prepare_task_for_api_with_error(
        &db,
        &fixture.config,
        interrupted_request,
        Some("abad0004".to_string()),
    )
    .expect("simulate crash after task/create-intent persistence");
    assert_eq!(crate::task_creator::prepared_task_id(&prepared), "abad0004");
    assert!(
        db.transferred_task_context("abad0004").unwrap().is_none(),
        "the simulated crash must precede transfer context persistence"
    );
    drop(db);

    let listener = tokio::net::UnixListener::bind(&fixture.socket_path).unwrap();
    let daemon_db_path = fixture.config.db_path.clone();
    let daemon = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let command = read_test_daemon_command(&mut reader, &mut write_half).await;
        let session_id = match command {
            DaemonCommand::Spawn { session_id, .. } => session_id,
            other => panic!("expected one recovery Spawn, got {other:?}"),
        };
        let db = Db::open(&daemon_db_path).unwrap();
        assert!(
            db.transferred_task_manifest_content_commitment("transfer-recovery")
                .unwrap()
                .is_some(),
            "daemon observed Spawn before the complete transfer proof was durable"
        );
        assert!(db.transferred_task_context("abad0004").unwrap().is_some());
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
    let (status, body) = create_transferred_task(
        &fixture,
        "abad0004",
        "transfer-recovery",
        &expected_head,
        &expected_head,
        transfer_import_body("transfer-recovery", &expected_head),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    daemon.await.unwrap();
    let db = Db::open(&fixture.config.db_path).unwrap();
    let (_, _, _, task, state) = db
        .transferred_task_manifest("transfer-recovery")
        .unwrap()
        .unwrap();
    assert_eq!(task.as_deref(), Some("abad0004"));
    assert_eq!(state, "prepared");
    assert!(
        db.transferred_task_manifest_content_commitment("transfer-recovery")
            .unwrap()
            .is_some(),
        "the complete Git/SQLite proof must be durable before recovery Spawn"
    );
    assert!(db.transferred_task_context("abad0004").unwrap().is_some());
    fixture.cleanup();
}

/// Branches one task head and two independent candidate bases directly off
/// the fixture repo's own `main`, so a test can bundle the head against
/// either base and get a distinct base OID -- and so the transferred head's
/// tree still carries `main`'s own `.kanna/config.json` and provider stubs
/// (`init_test_git_repo` writes those onto `main` only; a task branch that
/// does not descend from it forks a worktree with no agent provider the task
/// spawn can find). Mirrors
/// `transfer_engine::git::tests::source_repo_with_head_and_two_bases`'s
/// shape, which this file cannot call directly (that helper closes over
/// `git.rs`'s private `git()` runner). Leaves `repo_root` checked out back on
/// `main` when it returns.
fn add_task_head_and_two_bases(repo_root: &Path) {
    let run = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(repo_root)
            .status()
            .unwrap()
            .success());
    };
    run(&["branch", "base-a"]);
    run(&["checkout", "-b", "base-b"]);
    std::fs::write(repo_root.join("alt.txt"), b"alt").unwrap();
    run(&["add", "alt.txt"]);
    run(&["commit", "-m", "alt base"]);
    run(&["checkout", "main"]);
    run(&["checkout", "-b", "task-1"]);
    std::fs::write(repo_root.join("task.txt"), b"task").unwrap();
    run(&["add", "task.txt"]);
    run(&["commit", "-m", "task work"]);
    run(&["checkout", "main"]);
}

fn create_bundle(source: &Path, dest: &Path, refs: &[&str]) {
    let mut args = vec!["bundle", "create", dest.to_str().unwrap()];
    args.extend_from_slice(refs);
    assert!(Command::new("git")
        .args(&args)
        .current_dir(source)
        .status()
        .unwrap()
        .success());
}

fn transfer_import_body_with_refs(
    transfer_id: &str,
    head_oid: &str,
    fork_ref: &str,
    diff_base_ref: &str,
) -> serde_json::Value {
    serde_json::json!({
        "repoId": "repo-1",
        "prompt": "resume the transferred agent",
        "workflowName": TEST_PROVIDER_NEUTRAL_WORKFLOW,
        "agentProvider": "claude",
        "baseRef": fork_ref,
        "diffBaseRef": diff_base_ref,
        "transferImport": {
            "transferId": transfer_id,
            "headOid": head_oid,
            "sourceMachine": "peer-source",
        },
    })
}

/// `pipeline_item.base_ref` is a straight pass-through of whatever
/// `import_task_bundle_refs` returns as the private base ref
/// (`import_verified_task_bundle` returns it unchanged;
/// `build_create_request` maps it to `diffBaseRef` unchanged;
/// `prepare_create_task_for_api`/`prepare_task_spawn` store `diffBaseRef` as
/// `pipeline_item.base_ref` unchanged — see
/// `docs/2026-09-10-transfer-ref-publication-pipeline-item-e2e-gap.md`). This
/// test drives the real production `import_task_bundle_refs` against this
/// fixture's actual repo to get that ref for real, feeds it through this
/// gate's real router/SQLite/git-worktree/fake-daemon boundary exactly as
/// `build_create_request` would wire it, and then proves the persisted column
/// -- not just the git ref -- survives a conflicting and an identical replay
/// of the same import.
#[tokio::test]
async fn a_persisted_base_ref_survives_a_conflicting_and_an_identical_replay() {
    use kanna_daemon::protocol::{Command as DaemonCommand, Event as DaemonEvent};
    use tokio::io::{AsyncWriteExt, BufReader};

    let fixture = build_gate_fixture("base-ref-binding");
    // Branched directly off this repo's own `main` (rather than an unrelated
    // fresh repo) so the transferred head's tree still carries `main`'s own
    // `.kanna/config.json` and provider stubs: the destination repo doubles
    // as its own "source" here, which is legitimate -- a bundle's objects are
    // already present when its source and destination are the same repo, and
    // `import_task_bundle_refs` still runs its full advertise/unbundle/verify
    // sequence for real.
    add_task_head_and_two_bases(&fixture.repo_root);
    let head_oid = crate::transfer_engine::git::commit_oid(&fixture.repo_root, "task-1").unwrap();
    let base_a_oid = crate::transfer_engine::git::commit_oid(&fixture.repo_root, "base-a").unwrap();
    let base_b_oid = crate::transfer_engine::git::commit_oid(&fixture.repo_root, "base-b").unwrap();

    let bundle_temp = tempfile::tempdir().unwrap();
    let bundle_a = bundle_temp.path().join("bundle-a.bundle");
    create_bundle(&fixture.repo_root, &bundle_a, &["task-1", "base-a"]);

    // The real production entry point `import_verified_task_bundle` calls,
    // run for real against this fixture's own repo -- the same repo the
    // task's worktree below forks from.
    let (head_ref, base_ref) = crate::transfer_engine::git::import_task_bundle_refs(
        &fixture.repo_root,
        &bundle_a,
        "transfer-baseref",
        "task-1",
        &head_oid,
        "base-a",
        &base_a_oid,
    )
    .expect("real bundle import establishes the private pair");

    let db = Db::open(&fixture.config.db_path).unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-baseref",
        "repo-1",
        Some("abad0006"),
        &head_oid,
        &base_a_oid,
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

    let (status, body) = create_transferred_task(
        &fixture,
        "abad0006",
        "transfer-baseref",
        &head_oid,
        &base_a_oid,
        transfer_import_body_with_refs("transfer-baseref", &head_oid, &head_ref, &base_ref),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    daemon.await.unwrap();

    let db = Db::open(&fixture.config.db_path).unwrap();
    let item = db.get_pipeline_item("abad0006").unwrap().unwrap();
    assert_eq!(
        item.base_ref.as_deref(),
        Some(base_ref.as_str()),
        "pipeline_item.base_ref must be exactly the private base ref import_task_bundle_refs returned"
    );
    assert_eq!(
        crate::transfer_engine::git::commit_oid(
            &fixture.repo_root,
            item.base_ref.as_deref().unwrap()
        )
        .unwrap(),
        base_a_oid,
        "the persisted base ref must still resolve to the original transferred base OID"
    );
    drop(db);

    // A later, conflicting import attempt for the *same* transfer and head
    // but a *different* base must be refused at the git layer (proved on its
    // own in git.rs's real-Git regressions) and must not disturb the pair the
    // task above is already bound to.
    let bundle_b = bundle_temp.path().join("bundle-b.bundle");
    create_bundle(&fixture.repo_root, &bundle_b, &["task-1", "base-b"]);
    crate::transfer_engine::git::import_task_bundle_refs(
        &fixture.repo_root,
        &bundle_b,
        "transfer-baseref",
        "task-1",
        &head_oid,
        "base-b",
        &base_b_oid,
    )
    .expect_err("a conflicting base for the same transfer and head must be refused");

    let db = Db::open(&fixture.config.db_path).unwrap();
    let item_after_conflict = db.get_pipeline_item("abad0006").unwrap().unwrap();
    assert_eq!(
        item_after_conflict.base_ref.as_deref(),
        Some(base_ref.as_str()),
        "a refused conflicting import must not move the persisted base ref"
    );
    assert_eq!(
        crate::transfer_engine::git::commit_oid(
            &fixture.repo_root,
            item_after_conflict.base_ref.as_deref().unwrap()
        )
        .unwrap(),
        base_a_oid,
        "a refused conflicting import must not move what the persisted base ref resolves to"
    );
    drop(db);

    // An identical retry of the original import must still converge on the
    // exact same pair the task is bound to.
    let retry = crate::transfer_engine::git::import_task_bundle_refs(
        &fixture.repo_root,
        &bundle_a,
        "transfer-baseref",
        "task-1",
        &head_oid,
        "base-a",
        &base_a_oid,
    )
    .expect("an identical retry of the original import must succeed");
    assert_eq!(retry, (head_ref, base_ref.clone()));

    let db = Db::open(&fixture.config.db_path).unwrap();
    let item_after_retry = db.get_pipeline_item("abad0006").unwrap().unwrap();
    assert_eq!(
        item_after_retry.base_ref.as_deref(),
        Some(base_ref.as_str())
    );

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
    for path in [
        "/v1/tasks/abad0005/actions/resume",
        "/v1/tasks/abad0005/actions/rerun-stage",
    ] {
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
