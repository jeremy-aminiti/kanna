//! The foreign-history checkpoint: a destination task must receive its
//! source's full ordered stage/main/post/revision history (not only the
//! *latest* result of each kind), with every record's original run identity
//! preserved, and its first agent-visible prompt must name its own branch —
//! never the imported private fork ref.
//!
//! Same harness as `transfer_preparation_gate` (real HTTP creation path, real
//! SQLite, a real (fake) daemon on a Unix socket), extended with a second
//! production-shaped workflow stage whose prompt substitutes the carried task
//! and both result bindings but deliberately does not opt into imported
//! revision feedback, so
//! this drives the actual production prompt-building and persistence paths
//! rather than asserting only against hand-written helpers.

use super::*;
use crate::http_api::create_transferred_task_in_process;

/// Isolated `Config` + git repo + SQLite DB for one test. A local copy of
/// `transfer_preparation_gate`'s `GateFixture`/`build_gate_fixture`: the two
/// modules are siblings under `tests`, whose private items are not visible
/// across a sibling boundary, and that file's own doc comment already notes
/// this same fixture shape is duplicated per test file rather than shared.
struct GateFixture {
    config: Config,
    repo_root: PathBuf,
    daemon_dir: PathBuf,
    socket_path: PathBuf,
}

fn build_gate_fixture(label: &str) -> GateFixture {
    let unique = unique_test_suffix();
    let repo_root =
        crate::test_paths::unique_test_path(&format!("kanna-http-transfer-history-{label}"));
    init_test_git_repo(&repo_root);
    let daemon_dir =
        crate::test_paths::unique_test_path(&format!("kanna-http-transfer-history-daemon-{label}"));
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
        db_path: Db::test_db_path(&format!("http-api-transfer-history-{label}-{unique}")),
        kanna_cli_path: None,
        desktop_id: "desktop-1".to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: "Studio Mac".to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "127.0.0.1".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        lan_routing_port: 4460,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file(
            "kanna-pairings-transfer-history",
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

async fn create_transferred_task(
    fixture: &GateFixture,
    task_id: &str,
    transfer_id: &str,
    head_oid: &str,
    mut body: serde_json::Value,
) -> (StatusCode, String) {
    for suffix in ["head", "base"] {
        let reference = format!("refs/kanna/transfers/{transfer_id}/{head_oid}/{suffix}");
        let output = Command::new("git")
            .args(["update-ref", &reference, head_oid])
            .current_dir(&fixture.repo_root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    body["diffBaseRef"] = serde_json::Value::String(format!(
        "refs/kanna/transfers/{transfer_id}/{head_oid}/base"
    ));
    let workflow_name = body["workflowName"]
        .as_str()
        .unwrap_or(TEST_PROVIDER_NEUTRAL_WORKFLOW);
    let workflow_definition = body["transferImport"]["workflowDefinition"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::fs::read_to_string(
                fixture
                    .repo_root
                    .join(".kanna/workflows")
                    .join(format!("{workflow_name}.json")),
            )
            .unwrap()
        });
    body["transferImport"]["workflowDefinition"] =
        serde_json::Value::String(workflow_definition.clone());
    let history = body["transferImport"]["history"].clone();
    let previous_stage_result = body["transferImport"]["previousStageResult"].clone();
    let previous_main_result = body["transferImport"]["previousMainResult"].clone();
    let revision_feedback = body["transferImport"]["revisionFeedback"].clone();
    let stage = body["stage"].as_str().unwrap_or("in progress").to_string();
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
                "prompt": request.prompt.clone(),
                "stage": stage,
                "branch": "refs/heads/source-task",
                "head_oid": head_oid,
                "base_oid": head_oid,
                "workflow_definition": workflow_definition,
                "previous_stage_result": previous_stage_result,
                "previous_main_result": previous_main_result,
                "revision_feedback": revision_feedback,
                "history": history,
                "pipeline": request.workflow_name.clone(),
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

async fn get_task_path(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Adds the production `single-reviewer` shape under an isolated test name.
/// Its review prompt exercises all independently carried prompt bindings and
/// deliberately predates `$REVISION_FEEDBACK`, as shipped workflows do.
/// Committed and published to `origin/main` exactly like
/// `init_test_git_repo` does for its own workflow.
fn write_history_checkpoint_workflow(repo_root: &Path) {
    std::fs::write(
        repo_root.join(".kanna/workflows/history-checkpoint.json"),
        serde_json::json!({
            "name": "history-checkpoint",
            "stages": [
                {
                    "name": "in progress",
                    "agent": "implement",
                    "prompt": "$TASK_PROMPT",
                    "policy": { "transition": "manual", "revision_transition": "auto" },
                    "post": {
                        "name": "commit",
                        "agent": "commit",
                        "prompt": "Commit the relevant work for this task before review. Original task: $TASK_PROMPT. Previous implementation result: $PREV_MAIN_RESULT"
                    }
                },
                {
                    "name": "review",
                    "agent": "review",
                    "prompt": "Review branch $BRANCH for task quality and test coverage against base $BASE_REF. Original task: $TASK_PROMPT. Previous stage result: $PREV_RESULT. Previous implementation result: $PREV_MAIN_RESULT",
                    "policy": { "transition": "auto" }
                },
                {
                    "name": "pr",
                    "agent": "pr",
                    "prompt": "Create a PR for the reviewed work on branch $BRANCH.",
                    "policy": { "transition": "manual" },
                    "post": {
                        "name": "approve",
                        "agent": "approve",
                        "prompt": "Approve the PR for branch $BRANCH after PR creation and signal the merge master. Previous result: $PREV_RESULT"
                    }
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(repo_root)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["commit", "-m", "add history checkpoint workflow"])
        .current_dir(repo_root)
        .status()
        .unwrap()
        .success());
    publish_test_origin_main(repo_root);
}

/// A transfer landing on `review` (a second hop's typical stage, since the
/// task already passed `in progress` on an earlier machine) whose
/// `transferImport` carries both the latest-result scalars *and* the full
/// ordered history two machines back. This is what `build_payload` produces
/// for a task that was itself imported and has since run further — the exact
/// shape a second hop must not collapse back down to "only the latest".
#[tokio::test]
async fn a_transferred_task_persists_ordered_history_and_substitutes_its_own_branch() {
    use kanna_daemon::protocol::{Command as DaemonCommand, Event as DaemonEvent};
    use tokio::io::{AsyncWriteExt, BufReader};

    let fixture = build_gate_fixture("history-checkpoint");
    write_history_checkpoint_workflow(&fixture.repo_root);
    let expected_head = repo_head_oid(&fixture.repo_root);

    let db = Db::open(&fixture.config.db_path).unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-history",
        "repo-1",
        Some("abcd0001"),
        &expected_head,
        &expected_head,
    )
    .unwrap();
    drop(db);

    let body = serde_json::json!({
        "repoId": "repo-1",
        "prompt": "Original task prompt",
        "workflowName": "history-checkpoint",
        "stage": "review",
        "agentProvider": "claude",
        "transferImport": {
            "transferId": "transfer-history",
            "headOid": expected_head,
            "sourceMachine": "peer-source",
            // Persisting context (and, alongside it, the ordered history
            // below) is gated on a pinned workflow definition being present
            // — the shape a real TaskBundle transfer always carries — so a
            // fixture that omits it would silently skip the very path this
            // test exists to exercise.
            "previousStageResult": serde_json::json!({"status":"succeeded","summary":"s".repeat(300)}).to_string(),
            "previousMainResult": serde_json::json!({"status":"succeeded","summary":"m".repeat(300)}).to_string(),
            "revisionFeedback": "first review directive\nsecond review directive",
            "history": [
                {
                    "sequence": 0,
                    "originPeerId": "peer-hop0",
                    "originTaskId": "task-hop0-original",
                    "originRunId": "run-hop0-implement",
                    "stage": "in progress",
                    "kind": "main",
                    "agent": "implement",
                    "result": serde_json::json!({"status":"succeeded","summary":"h".repeat(300)}).to_string()
                },
                {
                    "sequence": 1,
                    "originPeerId": "peer-hop0",
                    "originTaskId": "task-hop0-original",
                    "originRunId": "run-hop0-commit",
                    "stage": "in progress",
                    "kind": "post",
                    "agent": "commit",
                    "result": "{\"status\":\"succeeded\"}"
                }
            ]
        },
    });

    let listener = tokio::net::UnixListener::bind(&fixture.socket_path).unwrap();
    let daemon = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let command = read_test_daemon_command(&mut reader, &mut write_half).await;
        let (session_id, args) = match command {
            DaemonCommand::Spawn {
                session_id, args, ..
            } => (session_id, args),
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
        args
    });

    let app = super::router(Arc::new(super::AppState::new(fixture.config.clone())));
    let (status, resp_body) = create_transferred_task(
        &fixture,
        "abcd0001",
        "transfer-history",
        &expected_head,
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp_body}");

    let args = daemon.await.unwrap();
    let command = args.last().expect("PTY command").clone();
    assert!(
        command.contains("Review branch task-abcd0001 for task quality and test coverage against"),
        "$BRANCH must resolve to this task's own branch, not the imported fork ref: {command}"
    );
    assert!(
        command.contains("Original task prompt"),
        "$TASK_PROMPT must carry the original transferred task: {command}"
    );
    assert!(
        command.contains(&"s".repeat(300)),
        "$PREV_RESULT must carry the full inherited result: {command}"
    );
    assert!(
        command.contains(&"m".repeat(300)),
        "$PREV_MAIN_RESULT was truncated: {command}"
    );
    assert!(
        command.contains("first review directive") && command.contains("second review directive"),
        "multiline revision feedback was not visible to the agent: {command}"
    );
    assert!(
        command.contains("## Revision Feedback\n\nfirst review directive\nsecond review directive"),
        "revision feedback must be independently labelled without workflow token opt-in: {command}"
    );

    // The full ordered history persists with each record's original
    // provenance intact — never rewritten to credit this destination.
    let db = Db::open(&fixture.config.db_path).unwrap();
    let history = db.transferred_task_history("abcd0001").unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(history[0].sequence, 0);
    assert_eq!(history[0].origin_peer_id, "peer-hop0");
    assert_eq!(history[0].origin_task_id, "task-hop0-original");
    assert_eq!(history[0].origin_run_id, "run-hop0-implement");
    assert_eq!(history[0].kind, "main");
    assert!(history[0]
        .result
        .as_deref()
        .unwrap()
        .contains(&"h".repeat(300)));
    assert_eq!(history[1].sequence, 1);
    assert_eq!(history[1].origin_run_id, "run-hop0-commit");
    assert_eq!(history[1].kind, "post");

    let context = db
        .transferred_task_context("abcd0001")
        .unwrap()
        .expect("scalar context also persisted");
    assert!(context.2.as_deref().unwrap().contains(&"s".repeat(300)));
    assert!(context.3.as_deref().unwrap().contains(&"m".repeat(300)));
    assert_eq!(
        context.4.as_deref(),
        Some("first review directive\nsecond review directive")
    );

    // The actual consumer: a reviewer, a manager, or a later hop reads the
    // full ordered history — not only the three latest scalars the prompt
    // carried — through GET /v1/tasks/{id}/transfer-history, with each
    // record's original provenance intact.
    let (status, resp_body) = get_task_path(&app, "/v1/tasks/abcd0001/transfer-history").await;
    assert_eq!(status, StatusCode::OK, "{resp_body}");
    let fetched: serde_json::Value =
        serde_json::from_str(&resp_body).expect("transfer history response");
    assert_eq!(fetched["taskId"], "abcd0001");
    let records = fetched["history"].as_array().expect("history array");
    assert_eq!(records.len(), 2, "{resp_body}");
    assert_eq!(records[0]["sequence"], 0);
    assert_eq!(records[0]["originPeerId"], "peer-hop0");
    assert_eq!(records[0]["originTaskId"], "task-hop0-original");
    assert_eq!(records[0]["originRunId"], "run-hop0-implement");
    assert_eq!(records[0]["stage"], "in progress");
    assert_eq!(records[0]["kind"], "main");
    assert_eq!(records[0]["agent"], "implement");
    assert_eq!(records[1]["sequence"], 1);
    assert_eq!(records[1]["originRunId"], "run-hop0-commit");
    assert_eq!(records[1]["kind"], "post");

    fixture.cleanup();
}

/// A payload with no `history` at all — an older sender, or a genuine first
/// hop with nothing to inherit — must still create the task normally: the
/// scalar snapshots alone are enough, exactly as before this checkpoint.
#[tokio::test]
async fn a_transfer_with_no_history_field_is_unaffected() {
    use kanna_daemon::protocol::{Command as DaemonCommand, Event as DaemonEvent};
    use tokio::io::{AsyncWriteExt, BufReader};

    let fixture = build_gate_fixture("history-checkpoint-compat");
    let expected_head = repo_head_oid(&fixture.repo_root);

    let db = Db::open(&fixture.config.db_path).unwrap();
    db.upsert_transferred_task_manifest(
        "transfer-compat",
        "repo-1",
        Some("abcd0002"),
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

    let (status, resp_body) = create_transferred_task(
        &fixture,
        "abcd0002",
        "transfer-compat",
        &expected_head,
        transfer_import_body("transfer-compat", &expected_head),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp_body}");
    daemon.await.unwrap();

    let db = Db::open(&fixture.config.db_path).unwrap();
    assert!(db.transferred_task_history("abcd0002").unwrap().is_empty());
    assert!(db.get_pipeline_item("abcd0002").unwrap().is_some());

    fixture.cleanup();
}
