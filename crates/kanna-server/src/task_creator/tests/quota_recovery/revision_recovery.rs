//! A new revision remains unfinished work when its provider is replaced.
//!
//! The incident reused a successful implementation's conversation. Only the
//! real revision route can prove which ancestry that producer writes; seeding
//! a synthetic revision row would miss the boundary that actually failed.

use super::*;

const IMPLEMENTATION_BRANCH: &str = "task-quota-implementation";
const ORIGINAL_RUN: &str = "run-previous-implementation";
const REVIEW_RUN: &str = "run-new-review";
const ORIGINAL_RESULT: &str =
    r#"{"status":"success","summary":"The previous implementation passed before this review."}"#;
const NEW_FINDING: &str = "wifi-configuration.spec.ts:145: success dismisses SetupWifiModal; reopen the modal before asserting the current-network row.";

/// Bounds both socket acceptance and command reads, including when a defect
/// parks the refusal instead of producing the Spawn this fixture expects.
/// Aborting on drop also closes the fake daemon if an earlier assertion fails.
struct RevisionDaemon(tokio::task::JoinHandle<Vec<kanna_daemon::protocol::Command>>);

impl RevisionDaemon {
    async fn commands(mut self) -> Vec<kanna_daemon::protocol::Command> {
        tokio::time::timeout(std::time::Duration::from_secs(60), &mut self.0)
            .await
            .expect("revision/provider replacement did not reach the fake daemon")
            .expect("revision fake daemon failed")
    }
}

impl Drop for RevisionDaemon {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Restore the process-global provider store even if the regression assertion
/// fails. The caller holds CLAUDE_CONFIG_DIR_LOCK for this guard's lifetime.
struct RevisionClaudeStore(Option<std::ffi::OsString>);

impl RevisionClaudeStore {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", path);
        Self(previous)
    }
}

impl Drop for RevisionClaudeStore {
    fn drop(&mut self) {
        match &self.0 {
            Some(previous) => std::env::set_var("CLAUDE_CONFIG_DIR", previous),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }
}

fn init_revision_recovery_fixture(label: &str, config: &Config) -> (std::path::PathBuf, Db) {
    let (repo_root, db) = init_quota_fixture(label, config);
    let workflow_path = repo_root.join(".kanna/workflows/candidates.json");
    let mut workflow: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&workflow_path).unwrap()).unwrap();
    workflow["stages"][0]["agent_provider"] =
        serde_json::json!(["claude-opus-med", "codex-gpt-6-astra-lo"]);
    workflow["stages"][0]["prompt"] =
        serde_json::json!("Implement the requested revision. $TASK_PROMPT");
    std::fs::write(&workflow_path, serde_json::to_string(&workflow).unwrap()).unwrap();
    publish_origin_main(&repo_root, "publish implementation fallback candidates");

    // Both workspaces have the identical committed tip, so only the presence
    // of a transcript decides whether the base revision producer resumes.
    let review_worktree = repo_root.join(format!(".kanna-worktrees/{REVIEW_WORKTREE_BRANCH}"));
    run_git_fixture(&review_worktree, &["merge", "--ff-only", "main"]);
    let implementation = repo_root.join(format!(".kanna-worktrees/{IMPLEMENTATION_BRANCH}"));
    run_git_fixture(
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            IMPLEMENTATION_BRANCH,
            implementation.to_str().unwrap(),
            "main",
        ],
    );
    db.insert_stage_run(NewStageRun {
        id: ORIGINAL_RUN,
        task_id: TASK_ID,
        stage: "in progress",
        kind: "main",
        agent: None,
        agent_provider: Some("claude"),
        model: Some("opus"),
        effort: Some("medium"),
        status: "running",
        result: None,
        feedback: None,
        session_id: Some(TASK_ID),
        provider_session_id: Some(super::super::revision::RESUME_SESSION_UUID),
        cwd: Some(implementation.to_str().unwrap()),
        resumed_from_run_id: None,
    })
    .unwrap();
    db.finish_stage_run(ORIGINAL_RUN, "succeeded", Some(ORIGINAL_RESULT), None)
        .unwrap();
    insert_running_review_run(&db, &repo_root, REVIEW_RUN, "codex", None, None);
    (repo_root, db)
}

fn task_spawn(commands: &[kanna_daemon::protocol::Command]) -> (&str, &str) {
    let spawns = commands
        .iter()
        .filter_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn {
                session_id,
                args,
                cwd,
                ..
            } if session_id == TASK_ID => Some((args.last().unwrap().as_str(), cwd.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(spawns.len(), 1, "one replacement of the task session");
    spawns[0]
}

async fn revision_route_then_provider_fallback(label: &str, with_transcript: bool) {
    let config = test_config(label);
    let (repo_root, db) = init_revision_recovery_fixture(label, &config);
    let implementation = repo_root.join(format!(".kanna-worktrees/{IMPLEMENTATION_BRANCH}"));
    let original_tip = run_git_fixture(&implementation, &["rev-parse", "HEAD"]);
    let store = repo_root.join("claude-config");
    std::fs::create_dir_all(&store).unwrap();
    if with_transcript {
        super::super::revision::write_resume_transcript(&store, &implementation);
    }
    let _store = RevisionClaudeStore::set(&store);
    let state = std::sync::Arc::new(crate::http_api::AppState::new(config.clone()));
    let app = crate::http_api::router(state.clone());
    let revision_daemon =
        RevisionDaemon(spawn_fake_daemon_fork_transition(config.daemon_dir.clone(), 1).await);

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tower::ServiceExt::oneshot(
            app,
            axum::http::Request::post(format!("/v1/tasks/{TASK_ID}/actions/request-revision"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "runId": REVIEW_RUN,
                        "targetStage": "in progress",
                        "summary": "The new Wi-Fi success assertion inspects a dismissed modal.",
                        "prompt": NEW_FINDING
                    })
                    .to_string(),
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("revision HTTP route did not return")
    .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, axum::http::StatusCode::OK, "{body:?}");
    let revision_commands = revision_daemon.commands().await;
    crate::http_api::wait_for_task_mutation_to_finish(&state, TASK_ID).await;

    let revision = db.latest_stage_run(TASK_ID).unwrap().unwrap();
    assert_ne!(revision.id, ORIGINAL_RUN);
    assert_ne!(revision.id, REVIEW_RUN);
    assert_eq!(revision.stage, "in progress");
    assert_eq!(revision.status, "running");
    assert_eq!(revision.agent_provider.as_deref(), Some("claude"));
    assert_eq!(revision.feedback.as_deref(), Some(NEW_FINDING));
    assert_eq!(revision.replaces_run_id, None, "a revision is new work");
    assert_eq!(
        revision.resumed_from_run_id.as_deref(),
        with_transcript.then_some(ORIGINAL_RUN),
        "pin the real revision producer; never fabricate its ancestry in this test"
    );
    let (revision_prompt, revision_cwd) = task_spawn(&revision_commands);
    assert!(revision_prompt.contains(NEW_FINDING), "{revision_prompt}");
    assert!(!revision_prompt.contains("ALREADY completed"));
    assert_eq!(revision.cwd.as_deref(), Some(revision_cwd));
    assert_eq!(db.stage_run(REVIEW_RUN).unwrap().unwrap().status, "failed");
    let branch_before_fallback = db.get_pipeline_item(TASK_ID).unwrap().unwrap().branch;

    let fallback_daemon = RevisionDaemon(
        spawn_fake_daemon_for_rejection(
            config.daemon_dir.clone(),
            vec![fable_rejection(TASK_ID)],
            1,
        )
        .await,
    );
    run_watcher(&state, &state.session_replacements()).await;
    let fallback_commands = fallback_daemon.commands().await;
    let (fallback_prompt, fallback_cwd) = task_spawn(&fallback_commands);
    let refused = db.stage_run(&revision.id).unwrap().unwrap();
    let replacement = db.latest_stage_run(TASK_ID).unwrap().unwrap();
    assert_eq!(refused.status, "failed");
    assert_eq!(
        refused.no_work_termination.as_deref(),
        Some(crate::db::no_work_termination::QUOTA_REPLACEMENT)
    );
    assert_eq!(refused.feedback.as_deref(), Some(NEW_FINDING));
    assert_ne!(replacement.id, revision.id);
    assert_eq!(replacement.status, "running");
    assert_eq!(replacement.result, None);
    assert_eq!(replacement.stage, "in progress");
    assert_eq!(replacement.agent_provider.as_deref(), Some("codex"));
    assert_eq!(replacement.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(replacement.effort.as_deref(), Some("low"));
    assert_eq!(
        replacement.replaces_run_id.as_deref(),
        Some(revision.id.as_str())
    );
    assert_eq!(replacement.resumed_from_run_id, None);
    assert_eq!(replacement.feedback.as_deref(), Some(NEW_FINDING));
    assert_eq!(replacement.cwd, revision.cwd);
    assert_eq!(fallback_cwd, revision_cwd);
    let original = db.stage_run(ORIGINAL_RUN).unwrap().unwrap();
    assert_eq!(original.status, "succeeded");
    assert_eq!(original.result.as_deref(), Some(ORIGINAL_RESULT));
    let task = db.get_pipeline_item(TASK_ID).unwrap().unwrap();
    assert_eq!(task.stage.as_deref(), Some("in progress"));
    assert_eq!(task.branch, branch_before_fallback);
    assert!(task.closed_at.is_none());
    assert_eq!(db.task_revision_rounds(TASK_ID).unwrap(), 1);
    assert_eq!(db.list_stage_runs_for_task(TASK_ID).unwrap().len(), 4);
    assert_eq!(
        run_git_fixture(std::path::Path::new(fallback_cwd), &["rev-parse", "HEAD"]),
        original_tip
    );
    assert!(run_git_fixture(
        std::path::Path::new(fallback_cwd),
        &["status", "--porcelain", "--untracked-files=all"]
    )
    .is_empty());
    let rejections = events_of(&db, "task.provider_quota_rejected");
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0]["stageRunId"], revision.id);
    assert_eq!(rejections[0]["replacementRunId"], replacement.id);
    assert_eq!(rejections[0]["recovery"], "fallback-started");
    assert!(events_of(&db, "task.provider_quota_parked").is_empty());

    // Assert the captured Spawn, not just retained DB feedback: the incident
    // kept feedback in the row while telling its new agent to do no work.
    assert!(fallback_prompt.contains(NEW_FINDING), "{fallback_prompt}");
    assert!(
        fallback_prompt.contains("Implement the requested revision."),
        "{fallback_prompt}"
    );
    assert!(
        !fallback_prompt.contains("ALREADY completed"),
        "{fallback_prompt}"
    );
    assert!(!fallback_prompt.contains("do not record a stage verdict again"));

    // Lose C's session too. The walk now reaches the revision boundary at
    // an interior hop (C replaces B), rather than starting directly at B.
    // This catches restoring the conversation fallback only inside the loop.
    let recovery_daemon =
        RevisionDaemon(spawn_recording_fake_daemon(config.daemon_dir.clone(), false).await);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tower::ServiceExt::oneshot(
            crate::http_api::router(state.clone()),
            axum::http::Request::post(format!("/v1/tasks/{TASK_ID}/actions/resume"))
                .body(axum::body::Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("recovery HTTP route did not return")
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let recovery_commands = recovery_daemon.commands().await;
    crate::http_api::wait_for_task_mutation_to_finish(&state, TASK_ID).await;
    let (recovery_prompt, recovery_cwd) = task_spawn(&recovery_commands);
    assert_eq!(recovery_cwd, fallback_cwd);
    assert!(recovery_prompt.contains("Implement the requested revision."));
    assert!(
        !recovery_prompt.contains("ALREADY completed"),
        "{recovery_prompt}"
    );
    let recovered = db.latest_stage_run(TASK_ID).unwrap().unwrap();
    assert_eq!(recovered.status, "running");
    assert_eq!(
        recovered.replaces_run_id.as_deref(),
        Some(replacement.id.as_str())
    );
    let original = db.stage_run(ORIGINAL_RUN).unwrap().unwrap();
    assert_eq!(original.status, "succeeded");
    assert_eq!(original.result.as_deref(), Some(ORIGINAL_RESULT));
    std::fs::remove_dir_all(&repo_root).unwrap();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Provider preflight runs inside the HTTP route's worker.
async fn resumed_revision_keeps_new_work_across_provider_fallback() {
    let _env_guard = crate::task_creator::tests::CLAUDE_CONFIG_DIR_LOCK
        .lock()
        .unwrap();
    revision_route_then_provider_fallback("resumed-revision-quota-boundary", true).await;
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // The empty provider store is also process-global.
async fn fresh_revision_keeps_new_work_across_provider_fallback() {
    let _env_guard = crate::task_creator::tests::CLAUDE_CONFIG_DIR_LOCK
        .lock()
        .unwrap();
    revision_route_then_provider_fallback("fresh-revision-quota-control", false).await;
}
