//! The 6b4a48af regression, end to end through the real server wiring.
//!
//! `plan-build-review` declares `["claude-fable", "codex-gpt-6-astra"]` on its
//! `review` stage, documented as an outage-fallback chain. On 2026-09-08 the
//! account's Fable allowance ran out, a task advanced to `review`, the stage
//! spawned on the leading candidate, and the session parked on
//! `You've reached your Fable limit`. Nothing classified it, so it read as an
//! ordinary dead session, and `kanna_rerun_stage` re-spawned it on Fable
//! because a rerun feeds the recorded provider back in as an explicit
//! override. The list never fell back once.
//!
//! These tests drive the real terminal-state watcher, the real recovery, the
//! real stage-run preparation and the real spawn against one fake daemon,
//! because the defect only ever existed in the wiring between them. The
//! providers are the repository's scripted stand-ins under
//! `.kanna/test-provider-bin`; no real CLI is driven and no quota is spent.

use super::*;

mod revision_recovery;

mod real_daemon;

const TASK_ID: &str = "quota-task";
const REVIEW_WORKTREE_BRANCH: &str = "task-quota-review";

/// The measured Claude refusal, as the daemon would announce it after
/// matching it. The chrome itself is pinned in
/// `tests/cli-contract/fixtures/provider-quota-rejection.json` and exercised
/// against the real classifier by the daemon's own suite; what this fixture
/// needs is the announcement that reaches the server.
fn fable_rejection(session_id: &str) -> kanna_daemon::protocol::Event {
    kanna_daemon::protocol::Event::ProviderNotice {
        session_id: session_id.to_string(),
        kind: kanna_daemon::protocol::ProviderNoticeKind::QuotaRejection,
        session_kind: kanna_daemon::protocol::SessionKind::Pty,
        agent_provider: Some(kanna_daemon::protocol::AgentProvider::Claude),
        rule_id: "claude/notice/quota-rejection".to_string(),
        scope: Some("Fable".to_string()),
        text: "⎿ You've reached your Fable limit. Run /usage-credits to continue or switch models \
               with /model."
            .to_string(),
        cli_version: Some("2.1.266".to_string()),
    }
}

fn astra_rejection(session_id: &str) -> kanna_daemon::protocol::Event {
    kanna_daemon::protocol::Event::ProviderNotice {
        session_id: session_id.to_string(),
        kind: kanna_daemon::protocol::ProviderNoticeKind::QuotaRejection,
        session_kind: kanna_daemon::protocol::SessionKind::Pty,
        agent_provider: Some(kanna_daemon::protocol::AgentProvider::Codex),
        rule_id: "codex/notice/quota-rejection".to_string(),
        scope: None,
        text: "■ You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to \
               purchase more credits."
            .to_string(),
        cli_version: Some("0.153.4".to_string()),
    }
}

/// A repo whose `review` stage names the same ordered candidate list the
/// incident's workflow does, and a task parked at that stage with a *running*
/// run on the leading candidate — exactly the state the refusal arrives in.
fn init_quota_fixture(label: &str, config: &Config) -> (std::path::PathBuf, Db) {
    init_quota_fixture_with_candidates(label, config, true)
}

/// The same fixture with the stage naming **no** `agent_provider` — the shape
/// every built-in workflow but `plan-build-review` actually has
/// (`single-reviewer`, `no-review`, `specialized-reviewers`, …). This is the
/// common case, and the one a refusal used to strand permanently.
fn init_quota_fixture_without_candidates(label: &str, config: &Config) -> (std::path::PathBuf, Db) {
    init_quota_fixture_with_candidates(label, config, false)
}

fn init_quota_fixture_with_candidates(
    label: &str,
    config: &Config,
    candidates: bool,
) -> (std::path::PathBuf, Db) {
    let repo_root = init_git_repo(label);
    std::fs::create_dir_all(repo_root.join(".kanna/workflows")).unwrap();
    std::fs::create_dir_all(repo_root.join(".kanna/agents/review")).unwrap();
    std::fs::write(
        repo_root.join(".kanna/workflows/candidates.json"),
        r#"{
  "stages": [
    { "name": "in progress", "policy": { "transition": "manual" }, "prompt": "$TASK_PROMPT" },
    {
      "name": "review",
      "policy": { "transition": "manual" },
      "agent": "review",
      "prompt": "Review $BRANCH"PROVIDERS
    }
  ]
}"#
        .replace(
            "PROVIDERS",
            if candidates {
                ",\n      \"agent_provider\": [\"claude-fable-hi\", \"codex-gpt-6-astra-lo\"]"
            } else {
                ""
            },
        ),
    )
    .unwrap();
    std::fs::write(
        repo_root.join(".kanna/agents/review/AGENT.md"),
        "---\nname: review\ndescription: Test review agent\n---\nReview it.",
    )
    .unwrap();
    publish_origin_main(&repo_root, "publish quota candidate definitions");
    run_git_fixture(&repo_root, &["branch", REVIEW_WORKTREE_BRANCH]);
    let review_worktree = repo_root.join(format!(".kanna-worktrees/{REVIEW_WORKTREE_BRANCH}"));
    run_git_fixture(
        &repo_root,
        &[
            "worktree",
            "add",
            review_worktree.to_string_lossy().as_ref(),
            REVIEW_WORKTREE_BRANCH,
        ],
    );

    let db = Db::open_for_tests(&config.db_path).unwrap();
    db.insert_test_repo_with_path("repo-1", &repo_root.to_string_lossy(), "Repo One")
        .unwrap();
    db.insert_test_pipeline_item(
        TASK_ID,
        "repo-1",
        "Fix the redraw.",
        Some("Quota task"),
        "review",
        "2026-09-08 07:00:00",
    )
    .unwrap();
    db.update_test_pipeline_item_stage_context(
        TASK_ID,
        REVIEW_WORKTREE_BRANCH,
        "candidates",
        None,
        "claude",
    )
    .unwrap();
    (repo_root, db)
}

/// The still-running review attempt on the leading candidate.
fn insert_running_review_run(
    db: &Db,
    repo_root: &std::path::Path,
    id: &str,
    provider: &str,
    model: Option<&str>,
    effort: Option<&str>,
) {
    let cwd = repo_root
        .join(format!(".kanna-worktrees/{REVIEW_WORKTREE_BRANCH}"))
        .to_string_lossy()
        .to_string();
    db.insert_stage_run(NewStageRun {
        id,
        task_id: TASK_ID,
        stage: "review",
        kind: "main",
        agent: Some("review"),
        agent_provider: Some(provider),
        model,
        effort,
        status: "running",
        result: None,
        feedback: None,
        session_id: Some(TASK_ID),
        provider_session_id: None,
        cwd: Some(cwd.as_str()),
        resumed_from_run_id: None,
    })
    .unwrap();
}

/// Fake daemon for the whole recovery: answers `Subscribe` and the watcher's
/// control `List`, broadcasts the refusal, then serves the recovery's own
/// connection (kill + spawn) before shutting the watcher down.
///
/// The shutdown is written after the refusal on the same subscriber stream, so
/// the watcher — which processes each event to completion before reading the
/// next — has finished the recovery by the time it sees it.
async fn spawn_fake_daemon_for_rejection(
    daemon_dir: String,
    rejections: Vec<kanna_daemon::protocol::Event>,
    expected_spawns: usize,
) -> tokio::task::JoinHandle<Vec<kanna_daemon::protocol::Command>> {
    let socket_path = test_daemon_socket_path(&daemon_dir);
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    tokio::spawn(async move {
        let (subscribe_stream, _) = listener.accept().await.unwrap();
        let (subscribe_read, mut subscribe_write) = subscribe_stream.into_split();
        let mut subscribe_reader = BufReader::new(subscribe_read);
        assert!(matches!(
            read_fake_daemon_command(&mut subscribe_reader, &mut subscribe_write).await,
            kanna_daemon::protocol::Command::Subscribe
        ));
        write_fake_daemon_event(&mut subscribe_write, &kanna_daemon::protocol::Event::Ok).await;

        let (list_stream, _) = listener.accept().await.unwrap();
        let (list_read, mut list_write) = list_stream.into_split();
        let mut list_reader = BufReader::new(list_read);
        assert!(matches!(
            read_fake_daemon_command(&mut list_reader, &mut list_write).await,
            kanna_daemon::protocol::Command::List
        ));
        write_fake_daemon_event(
            &mut list_write,
            &kanna_daemon::protocol::Event::SessionList {
                sessions: Vec::new(),
            },
        )
        .await;

        for rejection in &rejections {
            write_fake_daemon_event(&mut subscribe_write, rejection).await;
        }

        let mut commands = Vec::new();
        let mut spawns = 0;
        while spawns < expected_spawns {
            let (recovery_stream, _) = listener.accept().await.unwrap();
            let (recovery_read, mut recovery_write) = recovery_stream.into_split();
            let mut recovery_reader = BufReader::new(recovery_read);
            loop {
                let Some(command) =
                    read_fake_daemon_command_optional(&mut recovery_reader, &mut recovery_write)
                        .await
                else {
                    break;
                };
                if answer_terminal_carryover_probe(&command, &mut recovery_write).await {
                    continue;
                }
                let response = match &command {
                    kanna_daemon::protocol::Command::Kill { .. } => {
                        kanna_daemon::protocol::Event::Ok
                    }
                    kanna_daemon::protocol::Command::Spawn { session_id, .. }
                    | kanna_daemon::protocol::Command::SpawnAgent { session_id, .. } => {
                        spawns += 1;
                        kanna_daemon::protocol::Event::SessionCreated {
                            session_id: session_id.clone(),
                        }
                    }
                    other => panic!("unexpected daemon command: {other:?}"),
                };
                commands.push(command);
                write_fake_daemon_event(&mut recovery_write, &response).await;
                if spawns >= expected_spawns {
                    break;
                }
            }
        }

        write_fake_daemon_event(
            &mut subscribe_write,
            &kanna_daemon::protocol::Event::ShuttingDown,
        )
        .await;
        commands
    })
}

/// A fake daemon that serves the watcher and broadcasts refusals but never
/// expects a spawn — for the cases that must park instead of recovering.
async fn spawn_fake_daemon_expecting_no_recovery(
    daemon_dir: String,
    rejection: kanna_daemon::protocol::Event,
) -> tokio::task::JoinHandle<()> {
    let socket_path = test_daemon_socket_path(&daemon_dir);
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    tokio::spawn(async move {
        let (subscribe_stream, _) = listener.accept().await.unwrap();
        let (subscribe_read, mut subscribe_write) = subscribe_stream.into_split();
        let mut subscribe_reader = BufReader::new(subscribe_read);
        assert!(matches!(
            read_fake_daemon_command(&mut subscribe_reader, &mut subscribe_write).await,
            kanna_daemon::protocol::Command::Subscribe
        ));
        write_fake_daemon_event(&mut subscribe_write, &kanna_daemon::protocol::Event::Ok).await;

        let (list_stream, _) = listener.accept().await.unwrap();
        let (list_read, mut list_write) = list_stream.into_split();
        let mut list_reader = BufReader::new(list_read);
        assert!(matches!(
            read_fake_daemon_command(&mut list_reader, &mut list_write).await,
            kanna_daemon::protocol::Command::List
        ));
        write_fake_daemon_event(
            &mut list_write,
            &kanna_daemon::protocol::Event::SessionList {
                sessions: Vec::new(),
            },
        )
        .await;

        write_fake_daemon_event(&mut subscribe_write, &rejection).await;
        write_fake_daemon_event(
            &mut subscribe_write,
            &kanna_daemon::protocol::Event::ShuttingDown,
        )
        .await;
    })
}

async fn run_watcher(
    state: &crate::http_api::AppState,
    replacements: &crate::session_replacements::SessionReplacements,
) {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::terminal_watcher::terminal_state_watcher_once(state, replacements),
    )
    .await
    .expect("the watcher did not finish")
    .unwrap();
}

fn events_of(db: &Db, kind: &str) -> Vec<serde_json::Value> {
    db.list_task_events(
        &crate::db::TaskEventScope::Tasks(vec![TASK_ID.to_string()]),
        0,
        i64::MAX,
        200,
    )
    .unwrap()
    .into_iter()
    .filter(|event| event.event_type == kind)
    .map(|event| event.payload)
    .collect()
}

/// Drive one `actions/resume` for the quota fixture's task against a daemon
/// that records rather than executes, and return what it was asked to spawn.
async fn quota_resume_round(
    config: &Config,
) -> (axum::http::StatusCode, Vec<kanna_daemon::protocol::Command>) {
    let daemon = super::spawn_recording_fake_daemon(config.daemon_dir.clone(), false).await;
    let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
        config.clone(),
    )));
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::post(format!("/v1/tasks/{TASK_ID}/actions/resume"))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    let commands = daemon.await.unwrap();
    (response.status(), commands)
}

/// The quota producer, driven for real, between a recorded success and a later
/// recovery.
///
/// A succeeded, a recovery of A is running, the provider refuses that turn for
/// spent quota and the real watcher replaces it. The refused row recorded no
/// agent turn, so a later recovery must still reach A's verdict. Nothing here
/// writes the classification: it has to come from the quota producer, so
/// reverting that call to `finish_stage_run` must break this test.
#[tokio::test]
async fn a_real_quota_replacement_stays_transparent_to_a_later_recovery() {
    let config = test_config("quota-recovery-chain");
    let (repo_root, db) = init_quota_fixture("quota-recovery-chain", &config);

    // A: the stage's real verdict.
    insert_running_review_run(&db, &repo_root, "run-succeeded", "claude", None, None);
    db.finish_stage_run(
        "run-succeeded",
        "succeeded",
        Some(r#"{"status":"success","summary":"the review passed before the outage"}"#),
        None,
    )
    .unwrap();

    // B: a real recovery of A, so its lineage comes from production code.
    let (status, _) = quota_resume_round(&config).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let recovery = loop {
        let run = db.latest_stage_run(TASK_ID).unwrap().unwrap();
        if run.id != "run-succeeded" {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        recovery.replaces_run_id.as_deref(),
        Some("run-succeeded"),
        "the recovery must be linked to the success by production, not by this test"
    );
    let feedback_before = recovery.feedback.clone();

    // The provider refuses B for spent quota; the real watcher replaces it.
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();
    let fake_daemon = spawn_fake_daemon_for_rejection(
        config.daemon_dir.clone(),
        vec![fable_rejection(TASK_ID)],
        1,
    )
    .await;
    run_watcher(&state, &replacements).await;
    let _ = fake_daemon.await.unwrap();

    let refused = db.stage_run(&recovery.id).unwrap().unwrap();
    assert_eq!(refused.status, "failed");
    assert_eq!(
        refused.no_work_termination.as_deref(),
        Some(crate::db::no_work_termination::QUOTA_REPLACEMENT),
        "the real quota producer must persist its classification: {refused:?}"
    );
    assert_eq!(
        refused.feedback, feedback_before,
        "the replacement leaves the attempt's retained feedback exactly as it was"
    );

    // C, the fallback candidate, then loses its session too.
    let fallback = db
        .list_stage_runs_for_task(TASK_ID)
        .unwrap()
        .into_iter()
        .find(|run| run.status == "running")
        .expect("the quota fallback candidate");
    let (status, commands) = quota_resume_round(&config).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last().cloned(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(
        command_line.contains("ALREADY completed and recorded its verdict"),
        "a quota refusal is not a task verdict, so D must still reach A: {command_line}"
    );
    assert!(
        command_line.contains("the review passed before the outage"),
        "A's exact recorded result must reach D: {command_line}"
    );
    assert!(
        !command_line.contains("Review $BRANCH") && !command_line.contains("Review it."),
        "no ordinary stage instructions after a recorded success: {command_line}"
    );
    assert_eq!(
        fallback.no_work_termination, None,
        "the fallback itself recorded nothing yet; only the refused row is classified"
    );

    let original = db.stage_run("run-succeeded").unwrap().unwrap();
    assert_eq!(original.status, "succeeded");
    assert!(original
        .result
        .as_deref()
        .unwrap()
        .contains("the review passed before the outage"));

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The regression itself: the leading candidate is refused before it has done
/// anything, and the stage's *next* candidate starts once — same task, same
/// stage, same workspace, carrying its own model and effort from its own
/// compact selector.
#[tokio::test]
async fn a_refused_leading_candidate_falls_back_to_the_next_one_with_its_own_model() {
    let config = test_config("quota-fallback");
    let (repo_root, db) = init_quota_fixture("quota-fallback", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_for_rejection(
        config.daemon_dir.clone(),
        vec![fable_rejection(TASK_ID)],
        1,
    )
    .await;
    run_watcher(&state, &replacements).await;
    let commands = fake_daemon.await.unwrap();

    let spawns = commands
        .iter()
        .filter(|command| {
            matches!(
                command,
                kanna_daemon::protocol::Command::Spawn { .. }
                    | kanna_daemon::protocol::Command::SpawnAgent { .. }
            )
        })
        .count();
    assert_eq!(
        spawns, 1,
        "each remaining candidate is tried at most once: {commands:?}"
    );

    let runs = db.list_stage_runs_for_task(TASK_ID).unwrap();
    let refused = runs
        .iter()
        .find(|run| run.id == "run-review")
        .expect("refused run");
    // Never a success. The spawn path finishes whatever is still running as
    // `succeeded` on its way past, which is exactly what would have turned a
    // refusal into a passing review.
    assert_eq!(refused.status, "failed");
    assert!(
        refused
            .result
            .as_deref()
            .is_some_and(|result| result.contains("Fable limit")),
        "the refused run keeps the provider's own sentence: {refused:?}"
    );

    let replacement = runs
        .iter()
        .find(|run| run.id != "run-review")
        .expect("a replacement run");
    assert_eq!(replacement.stage, "review", "same stage");
    assert_eq!(replacement.kind, "main");
    assert_eq!(replacement.agent_provider.as_deref(), Some("codex"));
    // The candidate's *own* pair, never the refused candidate's: a selector
    // list gives every fallback its own coherent model and effort, and
    // `codex -m fable` would be rejected by the Codex CLI outright.
    assert_eq!(replacement.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(replacement.effort.as_deref(), Some("low"));
    assert_eq!(replacement.status, "running");
    assert_eq!(
        replacement.cwd.as_deref(),
        refused.cwd.as_deref(),
        "the fallback runs in the same workspace; nothing is forked or reset"
    );
    // The engine walked to this provider; nobody chose it. Recording an
    // explicit override would make the next rerun read an outage detour as a
    // decision.
    assert!(replacement.provider_override.is_none());

    let rejected = events_of(&db, "task.provider_quota_rejected");
    assert_eq!(rejected.len(), 1, "one refusal, one record");
    assert_eq!(rejected[0]["provider"], "claude");
    assert_eq!(rejected[0]["scope"], "Fable");
    assert_eq!(rejected[0]["source"], "pty");
    assert_eq!(rejected[0]["recovery"], "fallback-started");
    assert_eq!(rejected[0]["ruleId"], "claude/notice/quota-rejection");
    // The record links the refused attempt to the one that replaced it, which
    // is the only durable lineage a later reader has: the terminal is gone at
    // the next stage boundary.
    assert_eq!(rejected[0]["stageRunId"], "run-review");
    assert_eq!(rejected[0]["replacementRunId"], replacement.id);
    assert!(
        rejected[0]["matchedText"]
            .as_str()
            .is_some_and(|text| text.contains("Fable limit")),
        "the record keeps the sentence the claim was made from"
    );
    assert!(
        events_of(&db, "task.provider_quota_parked").is_empty(),
        "a recovered refusal is not an actionable park"
    );
    // The task did not advance and did not close.
    let item = db.get_pipeline_item(TASK_ID).unwrap().expect("task");
    assert_eq!(item.stage.as_deref(), Some("review"));
    assert!(item.closed_at.is_none());
}

/// The second refusal has nothing left to try, so the task parks in one
/// actionable state — and no further attempt is made.
#[tokio::test]
async fn the_last_candidate_refused_parks_the_task_once() {
    let config = test_config("quota-exhausted");
    let (repo_root, db) = init_quota_fixture("quota-exhausted", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    // The first refusal already happened and moved the task onto codex.
    db.record_provider_rejection(crate::db::NewProviderRejection {
        task_id: TASK_ID,
        stage_run_id: "run-review",
        stage: "review",
        provider: "claude",
        model: Some("fable"),
        effort: Some("high"),
        source: crate::db::QuotaRejectionSource::Pty,
        rule_id: "claude/notice/quota-rejection",
        matched_text: "You've reached your Fable limit.",
        scope: Some("Fable"),
        cli_version: Some("2.1.266"),
        recovery: crate::db::QuotaRecovery::FallbackStarted,
        replacement_run_id: Some("run-review-2"),
    })
    .unwrap();
    db.finish_stage_run("run-review", "failed", Some("refused"), None)
        .unwrap();
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review-2",
        "codex",
        Some("gpt-6-astra"),
        Some("low"),
    );
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_expecting_no_recovery(
        config.daemon_dir.clone(),
        astra_rejection(TASK_ID),
    )
    .await;
    run_watcher(&state, &replacements).await;
    fake_daemon.await.unwrap();

    let parked = events_of(&db, "task.provider_quota_parked");
    assert_eq!(parked.len(), 1, "one park, not a retry loop");
    assert_eq!(parked[0]["reason"], "parked-no-candidates");
    assert_eq!(
        parked[0]["rejectedProviders"],
        serde_json::json!(["claude", "codex"]),
    );
    assert!(
        parked[0]["action"]
            .as_str()
            .is_some_and(|action| !action.trim().is_empty()),
        "a parked task says what a person can do about it"
    );

    // No completion is fabricated: the refused attempt stays running, because
    // its session is a live, parked agent, and nothing here declares it dead.
    let run = db
        .list_stage_runs_for_task(TASK_ID)
        .unwrap()
        .into_iter()
        .find(|run| run.id == "run-review-2")
        .expect("the second attempt");
    assert_eq!(run.status, "running");
    assert!(run.result.is_none());

    // Task detail carries the same one state a manager reads.
    let item = db.get_pipeline_item(TASK_ID).unwrap().expect("task");
    assert_eq!(item.activity.as_deref(), Some("unread"));
    assert_eq!(item.stage.as_deref(), Some("review"));
}

/// A refusal that arrives after the attempt has changed its workspace parks
/// the task and preserves the workspace. Replacing that blind is exactly the
/// "no blind replay" case: the second agent would redo work the first one may
/// have half-finished.
#[tokio::test]
async fn a_refusal_after_the_workspace_changed_parks_and_preserves_the_work() {
    let config = test_config("quota-dirty");
    let (repo_root, db) = init_quota_fixture("quota-dirty", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    let worktree = repo_root.join(format!(".kanna-worktrees/{REVIEW_WORKTREE_BRANCH}"));
    std::fs::write(worktree.join("review-notes.md"), "half-finished findings").unwrap();
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_expecting_no_recovery(
        config.daemon_dir.clone(),
        fable_rejection(TASK_ID),
    )
    .await;
    run_watcher(&state, &replacements).await;
    fake_daemon.await.unwrap();

    let parked = events_of(&db, "task.provider_quota_parked");
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0]["reason"], "parked-work-observed");

    assert_eq!(
        std::fs::read_to_string(worktree.join("review-notes.md")).unwrap(),
        "half-finished findings",
        "the workspace is preserved exactly as the refused attempt left it"
    );
    let runs = db.list_stage_runs_for_task(TASK_ID).unwrap();
    assert_eq!(runs.len(), 1, "no replacement run was started: {runs:?}");
    assert_eq!(
        runs[0].status, "running",
        "and nothing was declared finished"
    );
}

/// An explicit single-provider override is a caller's decision about which
/// provider runs this stage. Quota recovery reports it; it does not overrule
/// it.
#[tokio::test]
async fn an_explicit_provider_override_is_never_walked_around() {
    let config = test_config("quota-override");
    let (repo_root, db) = init_quota_fixture("quota-override", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    db.set_test_stage_run_provider_override(
        "run-review",
        &crate::db::StageProviderOverride {
            source: "operator".to_string(),
            provider: "claude".to_string(),
            model: Some("fable".to_string()),
            effort: None,
        },
    )
    .unwrap();
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_expecting_no_recovery(
        config.daemon_dir.clone(),
        fable_rejection(TASK_ID),
    )
    .await;
    run_watcher(&state, &replacements).await;
    fake_daemon.await.unwrap();

    let parked = events_of(&db, "task.provider_quota_parked");
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0]["reason"], "parked-override-binding");
    assert_eq!(
        db.list_stage_runs_for_task(TASK_ID).unwrap().len(),
        1,
        "the override's provider is not replaced behind the caller's back"
    );

    // The *automatic* path leaves the override alone. A deliberate rerun still
    // runs, reproducing that override — which is what the parked action
    // promises, and the only thing `kanna_rerun_stage` can do, since it takes
    // no provider argument.
    let rerun = prepare_rerun_stage_for_api(&db, &config, TASK_ID)
        .expect("a deliberate rerun is never refused for a past refusal");
    assert_eq!(rerun.agent_provider, "claude");
    assert_eq!(
        rerun
            .provider_override
            .as_ref()
            .map(|o| o.provider.as_str()),
        Some("claude"),
        "the rerun reproduces the caller's override rather than walking around it"
    );
}

/// The half of the incident nothing caught: `kanna_rerun_stage` re-spawned the
/// refused provider because a rerun reproduces the recorded run. With the
/// stage's ordered list to walk, it re-resolves around the refusal instead.
#[tokio::test]
async fn a_rerun_after_a_refusal_does_not_respawn_the_refused_provider() {
    let config = test_config("quota-rerun");
    let (repo_root, db) = init_quota_fixture("quota-rerun", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    db.record_provider_rejection(crate::db::NewProviderRejection {
        task_id: TASK_ID,
        stage_run_id: "run-review",
        stage: "review",
        provider: "claude",
        model: Some("fable"),
        effort: Some("high"),
        source: crate::db::QuotaRejectionSource::Pty,
        rule_id: "claude/notice/quota-rejection",
        matched_text: "You've reached your Fable limit.",
        scope: Some("Fable"),
        cli_version: Some("2.1.266"),
        recovery: crate::db::QuotaRecovery::ParkedWorkObserved,
        replacement_run_id: None,
    })
    .unwrap();

    let rerun = prepare_rerun_stage_for_api(&db, &config, TASK_ID).unwrap();
    assert_eq!(
        rerun.agent_provider, "codex",
        "the rerun re-resolves the stage's candidate list around the refusal"
    );
    assert_eq!(rerun.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(rerun.effort.as_deref(), Some("low"));
    assert!(
        rerun.provider_override.is_none(),
        "walking around a refusal is not a caller's provider decision"
    );
}

/// With no un-refused candidate left, a rerun still runs — on the recorded
/// provider. Refusing here is what disabled recovery forever: nothing ages a
/// rejection row out, so the operator the parked action tells to "wait for the
/// allowance to reset and rerun" could never do it.
#[tokio::test]
async fn a_rerun_with_no_remaining_candidate_runs_the_recorded_provider() {
    let config = test_config("quota-rerun-exhausted");
    let (repo_root, db) = init_quota_fixture("quota-rerun-exhausted", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "codex",
        Some("gpt-6-astra"),
        Some("low"),
    );
    for provider in ["claude", "codex"] {
        db.record_provider_rejection(crate::db::NewProviderRejection {
            task_id: TASK_ID,
            stage_run_id: "run-review",
            stage: "review",
            provider,
            model: None,
            effort: None,
            source: crate::db::QuotaRejectionSource::Pty,
            rule_id: "test/notice/quota-rejection",
            matched_text: "refused",
            scope: None,
            cli_version: Some("2.1.266"),
            recovery: crate::db::QuotaRecovery::ParkedNoCandidates,
            replacement_run_id: None,
        })
        .unwrap();
    }

    let rerun = prepare_rerun_stage_for_api(&db, &config, TASK_ID)
        .expect("a deliberate rerun is never refused for a past refusal");
    assert_eq!(
        rerun.agent_provider, "codex",
        "with nothing un-refused to prefer, the rerun reproduces the recorded run"
    );
    assert_eq!(rerun.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(rerun.effort.as_deref(), Some("low"));
}

/// The gate is keyed to the refused run, so it stops applying once the
/// operator has acted: the rerun's own new run carries no rejection, and the
/// next rerun reproduces it without consulting the stage's history at all.
#[tokio::test]
async fn a_second_rerun_is_not_gated_by_the_first_runs_refusal() {
    let config = test_config("quota-rerun-twice");
    let (repo_root, db) = init_quota_fixture("quota-rerun-twice", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    db.record_provider_rejection(crate::db::NewProviderRejection {
        task_id: TASK_ID,
        stage_run_id: "run-review",
        stage: "review",
        provider: "claude",
        model: Some("fable"),
        effort: Some("high"),
        source: crate::db::QuotaRejectionSource::Pty,
        rule_id: "claude/notice/quota-rejection",
        matched_text: "You've reached your Fable limit.",
        scope: Some("Fable"),
        cli_version: Some("2.1.266"),
        recovery: crate::db::QuotaRecovery::ParkedWorkObserved,
        replacement_run_id: None,
    })
    .unwrap();

    // The refused run is the latest, so this rerun walks around it.
    let first = prepare_rerun_stage_for_api(&db, &config, TASK_ID).unwrap();
    assert_eq!(first.agent_provider, "codex");

    // The rerun's own run is now the latest and was never refused, so the
    // stage's rejection history no longer steers anything.
    db.finish_stage_run("run-review", "cancelled", None, None)
        .unwrap();
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review-2",
        "claude",
        Some("fable"),
        Some("high"),
    );

    let second = prepare_rerun_stage_for_api(&db, &config, TASK_ID)
        .expect("a run that was never refused reruns normally");
    assert_eq!(
        second.agent_provider, "claude",
        "a rejection against an older run must not steer this one"
    );
    assert_eq!(second.model.as_deref(), Some("fable"));
}

/// A resume reopens the recorded provider's own conversation. A past refusal
/// does not refuse it: reopening once the allowance has reset is exactly what
/// the `parked-work-observed` action tells the operator to do.
#[tokio::test]
async fn a_resume_after_a_refusal_reopens_the_recorded_conversation() {
    let config = test_config("quota-resume");
    let (repo_root, db) = init_quota_fixture("quota-resume", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    db.finish_stage_run("run-review", "failed", Some("session ended"), None)
        .unwrap();
    db.record_provider_rejection(crate::db::NewProviderRejection {
        task_id: TASK_ID,
        stage_run_id: "run-review",
        stage: "review",
        provider: "claude",
        model: Some("fable"),
        effort: Some("high"),
        source: crate::db::QuotaRejectionSource::Pty,
        rule_id: "claude/notice/quota-rejection",
        matched_text: "You've reached your Fable limit.",
        scope: Some("Fable"),
        cli_version: Some("2.1.266"),
        recovery: crate::db::QuotaRecovery::ParkedWorkObserved,
        replacement_run_id: None,
    })
    .unwrap();

    let resumed = prepare_resume_task_for_api(&db, &config, TASK_ID)
        .expect("a refusal must not disable resume for the rest of the stage");
    assert_eq!(
        resumed.agent_provider, "claude",
        "a resume continues the recorded provider's own conversation"
    );
}

/// The common case, and the regression this revision fixes: a workflow whose
/// stage names no `agent_provider` at all — `single-reviewer` and every other
/// built-in but `plan-build-review`. The refusal parks
/// `parked-no-candidate-list`, and both operations the parked action names
/// must then actually work. They used to be refused for the rest of the task's
/// life at that stage, because the gate was keyed to the stage name and
/// nothing ages a rejection row out.
#[tokio::test]
async fn a_stage_with_no_candidates_parks_and_still_reruns_and_resumes() {
    let config = test_config("quota-no-candidates");
    let (repo_root, db) = init_quota_fixture_without_candidates("quota-no-candidates", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_expecting_no_recovery(
        config.daemon_dir.clone(),
        fable_rejection(TASK_ID),
    )
    .await;
    run_watcher(&state, &replacements).await;
    fake_daemon.await.unwrap();

    let parked = events_of(&db, "task.provider_quota_parked");
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0]["reason"], "parked-no-candidate-list");
    assert_eq!(
        db.list_stage_runs_for_task(TASK_ID).unwrap().len(),
        1,
        "there was no candidate to fall back to, so nothing was spawned"
    );

    // What the parked action promises: rerun the stage, and it runs.
    let rerun = prepare_rerun_stage_for_api(&db, &config, TASK_ID)
        .expect("a parked task must have a working recovery through rerun");
    assert_eq!(
        rerun.agent_provider, "claude",
        "with no candidate list there is nothing to prefer; the rerun reproduces the run"
    );

    // And resume, once the session has ended, reopens the same conversation.
    db.finish_stage_run("run-review", "failed", Some("session ended"), None)
        .unwrap();
    let resumed = prepare_resume_task_for_api(&db, &config, TASK_ID)
        .expect("a parked task must have a working recovery through resume");
    assert_eq!(resumed.agent_provider, "claude");
}

/// The parked-action sentences are operator instructions, so they must name
/// operations that exist and read as prose. Both failed once: one told the
/// operator to "rerun the stage with a different provider override" when
/// `kanna_rerun_stage` takes only a task id, and a sibling literal shipped
/// with runs of spaces where `\` line continuations were intended.
#[test]
fn parked_actions_name_real_operations_and_are_not_garbled() {
    use crate::db::QuotaRecovery;

    for recovery in [
        QuotaRecovery::ParkedNoCandidates,
        QuotaRecovery::ParkedNoCandidateList,
        QuotaRecovery::ParkedWorkObserved,
        QuotaRecovery::ParkedOverrideBinding,
        QuotaRecovery::ParkedFallbackFailed,
        QuotaRecovery::ParkedConcurrentMutation,
    ] {
        let action = crate::http_api::parked_action_for_tests(recovery);
        assert!(
            !action.trim().is_empty(),
            "{recovery:?} parks the task, so it owes the operator an action"
        );
        assert!(
            !action.contains("  "),
            "{recovery:?} action has a collapsed line continuation: {action:?}"
        );
        assert!(
            !action.contains("provider override once")
                && !action.contains("with a different provider override"),
            "{recovery:?} action tells the operator to pass an override kanna_rerun_stage \
             does not accept: {action:?}"
        );
    }
}

/// The same refusal announced twice — a re-adopted session, a reconnecting
/// watcher — is one observation and one attempt.
#[tokio::test]
async fn a_replayed_refusal_does_not_start_a_second_attempt() {
    let config = test_config("quota-replay");
    let (repo_root, db) = init_quota_fixture("quota-replay", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_for_rejection(
        config.daemon_dir.clone(),
        vec![fable_rejection(TASK_ID), fable_rejection(TASK_ID)],
        1,
    )
    .await;
    run_watcher(&state, &replacements).await;
    let commands = fake_daemon.await.unwrap();

    let spawns = commands
        .iter()
        .filter(|command| {
            matches!(
                command,
                kanna_daemon::protocol::Command::Spawn { .. }
                    | kanna_daemon::protocol::Command::SpawnAgent { .. }
            )
        })
        .count();
    assert_eq!(
        spawns, 1,
        "the replay is de-duplicated, not retried: {commands:?}"
    );
    assert_eq!(events_of(&db, "task.provider_quota_rejected").len(), 1);
}

/// A refusal for a task nobody is running — closed, or a session that is not
/// this run's — is recorded nowhere and acted on not at all.
#[tokio::test]
async fn a_refusal_against_a_closed_task_changes_nothing() {
    let config = test_config("quota-closed");
    let (repo_root, db) = init_quota_fixture("quota-closed", &config);
    insert_running_review_run(
        &db,
        &repo_root,
        "run-review",
        "claude",
        Some("fable"),
        Some("high"),
    );
    db.close_pipeline_item(TASK_ID).unwrap();
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();

    let fake_daemon = spawn_fake_daemon_expecting_no_recovery(
        config.daemon_dir.clone(),
        fable_rejection(TASK_ID),
    )
    .await;
    run_watcher(&state, &replacements).await;
    fake_daemon.await.unwrap();

    assert!(events_of(&db, "task.provider_quota_rejected").is_empty());
    assert_eq!(db.list_stage_runs_for_task(TASK_ID).unwrap().len(), 1);
}
