use super::*;
use std::os::unix::fs::PermissionsExt;

const RECOVERY_SESSION_ID: &str = "7f7d2f7a-1b2e-4c3d-9a8b-123456789abc";
/// The task's explicit model override. It lives only on its stage runs, so a
/// restart that re-derives the model from the stage definition silently
/// demotes the task — which is exactly what the recovered sidecar task lost.
const RECOVERY_MODEL: &str = "claude-opus-4-5";
/// Requested changes carried by a resumed revision run. A fresh conversation
/// only knows what its prompt says, so losing these would quietly turn a
/// revision into a re-run of the stage.
const REVIEW_FEEDBACK: &str = "Add end-to-end coverage for the rejected resume.";

fn init_recovery_fixture(label: &str) -> (std::path::PathBuf, Config, Db) {
    let repo_root = init_git_repo(label);
    run_git_fixture(&repo_root, &["branch", "task-recovery"]);
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    run_git_fixture(
        &repo_root,
        &[
            "worktree",
            "add",
            worktree.to_string_lossy().as_ref(),
            "task-recovery",
        ],
    );
    let config = test_config(label);
    let db = Db::open_for_tests(&config.db_path).unwrap();
    db.insert_test_repo_with_path("repo-1", &repo_root.to_string_lossy(), "Repo One")
        .unwrap();
    db.insert_test_pipeline_item(
        "recovery-task",
        "repo-1",
        "Remember the earlier decision and finish the file.",
        Some("Recovery"),
        "in progress",
        "2026-07-30 07:00:00",
    )
    .unwrap();
    db.update_test_pipeline_item_stage_context(
        "recovery-task",
        "task-recovery",
        "no-review",
        None,
        "claude",
    )
    .unwrap();
    db.upsert_worktree(
        "wt-recovery-task",
        "recovery-task",
        &worktree.to_string_lossy(),
        "task-recovery",
    )
    .unwrap();
    db.upsert_terminal_session(
        "agent-recovery-task",
        "repo-1",
        Some("recovery-task"),
        Some("agent"),
        Some(&worktree.to_string_lossy()),
        Some("recovery-task"),
    )
    .unwrap();
    db.insert_stage_run(NewStageRun {
        id: "run-killed-mid-turn",
        task_id: "recovery-task",
        stage: "in progress",
        kind: "main",
        agent: None,
        agent_provider: Some("claude"),
        model: Some(RECOVERY_MODEL),
        effort: None,
        status: "running",
        result: None,
        feedback: None,
        session_id: Some("recovery-task"),
        provider_session_id: Some(RECOVERY_SESSION_ID),
        cwd: Some(worktree.to_string_lossy().as_ref()),
        resumed_from_run_id: None,
    })
    .unwrap();
    (repo_root, config, db)
}

/// Put the task where a resume launch has just been made: the interrupted run
/// closed, and a `--resume` run of it recorded as running.
fn record_resume_attempt(db: &Db, worktree: &std::path::Path, feedback: Option<&str>) {
    db.finish_stage_run(
        "run-killed-mid-turn",
        "failed",
        Some("agent session exited before recording a stage verdict (exit code 143)"),
        None,
    )
    .unwrap();
    db.insert_stage_run(NewStageRun {
        id: "run-resume-attempt",
        task_id: "recovery-task",
        stage: "in progress",
        kind: "main",
        agent: None,
        agent_provider: Some("claude"),
        model: Some(RECOVERY_MODEL),
        effort: None,
        status: "running",
        result: None,
        feedback,
        session_id: Some("recovery-task"),
        provider_session_id: Some(RECOVERY_SESSION_ID),
        cwd: Some(worktree.to_string_lossy().as_ref()),
        resumed_from_run_id: Some("run-killed-mid-turn"),
    })
    .unwrap();
}

/// What `claude --resume <id>` prints when its own store has no such
/// conversation — the transcript existed at preflight, or never did, but the
/// CLI is the one that decides.
fn rejected_resume_screen() -> String {
    format!(
        "\u{1b}[2m$ claude --resume {RECOVERY_SESSION_ID}\u{1b}[0m\n\
         No conversation found with session ID: {RECOVERY_SESSION_ID}\n"
    )
}

/// Fake daemon for the rejected-resume recovery: the classifier reads the
/// exited session's terminal on one connection, then the replacement spawn
/// runs on the next. `accept_spawn` is false for the case where the fresh
/// relaunch itself cannot start.
async fn spawn_rejected_resume_fake_daemon(
    daemon_dir: String,
    snapshot_vt: String,
    accept_spawn: bool,
) -> tokio::task::JoinHandle<Vec<kanna_daemon::protocol::Command>> {
    let socket_path = test_daemon_socket_path(&daemon_dir);
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    tokio::spawn(async move {
        let mut commands = Vec::new();

        let (snapshot_stream, _) = listener.accept().await.unwrap();
        let (snapshot_read, mut snapshot_write) = snapshot_stream.into_split();
        let mut snapshot_reader = BufReader::new(snapshot_read);
        let command = read_fake_daemon_command(&mut snapshot_reader, &mut snapshot_write).await;
        assert!(
            matches!(command, kanna_daemon::protocol::Command::Snapshot { .. }),
            "expected the recovery to read the session terminal, got {command:?}"
        );
        commands.push(command);
        let event = kanna_daemon::protocol::Event::Snapshot {
            session_id: "recovery-task".to_string(),
            snapshot: kanna_daemon::protocol::TerminalSnapshot {
                version: 1,
                rows: 24,
                cols: 80,
                cursor_row: 0,
                cursor_col: 0,
                cursor_visible: true,
                saved_at: 0,
                sequence: 0,
                vt: snapshot_vt,
            },
            agent_provider: None,
        };
        snapshot_write
            .write_all(format!("{}\n", serde_json::to_string(&event).unwrap()).as_bytes())
            .await
            .unwrap();
        drop(snapshot_write);

        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        loop {
            let command = read_fake_daemon_command(&mut reader, &mut write_half).await;
            if super::answer_terminal_carryover_probe(&command, &mut write_half).await {
                continue;
            }
            let response = match &command {
                kanna_daemon::protocol::Command::Kill { .. } => {
                    kanna_daemon::protocol::Event::Error {
                        code: Some(kanna_daemon::protocol::ErrorCode::SessionNotFound),
                        message: "session not found".to_string(),
                    }
                }
                kanna_daemon::protocol::Command::Spawn {
                    session_id,
                    args,
                    cwd,
                    env,
                    ..
                } if accept_spawn => {
                    let command_line = args.last().expect("provider command");
                    let status = tokio::process::Command::new("/bin/sh")
                        .args(["-c", command_line])
                        .current_dir(cwd)
                        .envs(env)
                        .status()
                        .await
                        .unwrap();
                    assert!(status.success(), "fake fresh provider failed: {status}");
                    kanna_daemon::protocol::Event::SessionCreated {
                        session_id: session_id.clone(),
                    }
                }
                kanna_daemon::protocol::Command::Spawn { .. } => {
                    kanna_daemon::protocol::Event::Error {
                        code: None,
                        message: "no pty available".to_string(),
                    }
                }
                other => panic!("unexpected daemon command: {other:?}"),
            };
            let spawned = matches!(command, kanna_daemon::protocol::Command::Spawn { .. });
            commands.push(command);
            write_half
                .write_all(format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes())
                .await
                .unwrap();
            if !spawned {
                continue;
            }
            return commands;
        }
    })
}

/// A fresh Claude launch: proves it was started without `--resume` and that
/// the task's model override survived.
fn write_fresh_claude_probe(worktree: &std::path::Path) {
    let fake_claude = worktree.join(".kanna/test-provider-bin/claude");
    std::fs::write(
        &fake_claude,
        r#"#!/bin/sh
model=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --resume) echo "resumed after rejection" > fresh-proof.txt; exit 7 ;;
    --model) model="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s' "$model" > fresh-proof.txt
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_recovery_transcript(config_dir: &std::path::Path, worktree: &std::path::Path) {
    let canonical_worktree = std::fs::canonicalize(worktree).unwrap();
    for cwd in [worktree, canonical_worktree.as_path()] {
        let slug: String = cwd
            .to_string_lossy()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let project_dir = config_dir.join("projects").join(slug);
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{RECOVERY_SESSION_ID}.jsonl")),
            "{\"remembered\":\"prior-context-retained\"}\n",
        )
        .unwrap();
    }
}

async fn spawn_recovery_fake_daemon(
    daemon_dir: String,
) -> tokio::task::JoinHandle<Vec<kanna_daemon::protocol::Command>> {
    let socket_path = test_daemon_socket_path(&daemon_dir);
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    tokio::spawn(async move {
        let (presence_stream, _) = listener.accept().await.unwrap();
        let (presence_read, mut presence_write) = presence_stream.into_split();
        let mut presence_reader = BufReader::new(presence_read);
        let mut presence_line = String::new();
        presence_reader.read_line(&mut presence_line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<kanna_daemon::protocol::Command>(presence_line.trim()).unwrap(),
            kanna_daemon::protocol::Command::List
        ));
        presence_write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&kanna_daemon::protocol::Event::SessionList {
                        sessions: Vec::new(),
                    })
                    .unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(presence_write);

        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut commands = Vec::new();
        loop {
            let command = read_fake_daemon_command(&mut reader, &mut write_half).await;
            if super::answer_terminal_carryover_probe(&command, &mut write_half).await {
                continue;
            }
            let response = match &command {
                kanna_daemon::protocol::Command::Kill { .. } => {
                    kanna_daemon::protocol::Event::Error {
                        code: Some(kanna_daemon::protocol::ErrorCode::SessionNotFound),
                        message: "session not found".to_string(),
                    }
                }
                kanna_daemon::protocol::Command::Spawn {
                    session_id,
                    args,
                    cwd,
                    env,
                    ..
                } => {
                    let command_line = args.last().expect("provider command");
                    let status = tokio::process::Command::new("/bin/sh")
                        .args(["-c", command_line])
                        .current_dir(cwd)
                        .envs(env)
                        .status()
                        .await
                        .unwrap();
                    assert!(status.success(), "fake resumed provider failed: {status}");
                    kanna_daemon::protocol::Event::SessionCreated {
                        session_id: session_id.clone(),
                    }
                }
                other => panic!("unexpected daemon command: {other:?}"),
            };
            let spawned = matches!(command, kanna_daemon::protocol::Command::Spawn { .. });
            commands.push(command);
            write_half
                .write_all(format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes())
                .await
                .unwrap();
            if spawned {
                return commands;
            }
        }
    })
}

async fn spawn_listing_fake_daemon(
    daemon_dir: String,
) -> tokio::task::JoinHandle<kanna_daemon::protocol::Command> {
    let socket_path = test_daemon_socket_path(&daemon_dir);
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let command = read_fake_daemon_command(&mut reader, &mut write_half).await;
        assert!(matches!(command, kanna_daemon::protocol::Command::List));
        let event = kanna_daemon::protocol::Event::SessionList {
            sessions: vec![kanna_daemon::protocol::SessionInfo {
                session_id: "recovery-task".to_string(),
                pid: 42,
                cwd: "/tmp".to_string(),
                state: kanna_daemon::protocol::SessionState::Active,
                idle_seconds: 0,
                status: kanna_daemon::protocol::SessionStatus::Busy,
                status_observed: true,
                kind: Default::default(),
                composer_text: None,
                composer_attestation: Default::default(),
            }],
        };
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&event).unwrap()).as_bytes())
            .await
            .unwrap();
        command
    })
}

/// Recovery respawns the interrupted run, so it must respawn with what that
/// run was actually using. The model and effort used to be re-resolved from
/// the stage and agent definition, which silently moved a recovered task onto
/// a different binding than the conversation it was continuing — the same
/// shape as the rerun bug, one level down.
#[tokio::test]
async fn resume_respawns_with_the_interrupted_runs_model_and_effort() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-binding");
    // What the definition would resolve to if the recorded run were ignored.
    std::fs::write(
        repo_root.join(".kanna/config.json"),
        serde_json::json!({
            "workspace": { "path": { "prepend": [".kanna/test-provider-bin"] } },
            "agentProviders": {
                "*": { "provider": ["claude"], "model": "repo-default-model", "effort": "low" }
            }
        })
        .to_string(),
    )
    .unwrap();
    publish_origin_main(&repo_root, "publish recovery provider preference");
    // What the interrupted run was actually holding its conversation with.
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE stage_run SET model = 'recorded-run-model', effort = 'high' WHERE id = ?",
            ["run-killed-mid-turn"],
        )
        .unwrap();

    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        137,
    )
    .await
    .unwrap();

    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    let config_dir = repo_root.join("claude-config");
    write_recovery_transcript(&config_dir, &worktree);
    let prepared = {
        let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
        let prepared = prepare_resume_task_for_api(&db, &config, "recovery-task");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        prepared.unwrap()
    };

    assert!(
        prepared.resumed_workspace().is_some(),
        "the transcript is present, so this must be a resumed run"
    );
    assert_eq!(prepared.agent_provider, "claude");
    assert_eq!(prepared.model.as_deref(), Some("recorded-run-model"));
    assert_eq!(prepared.effort.as_deref(), Some("high"));

    let _ = std::fs::remove_dir_all(&repo_root);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn restart_recovery_restores_the_claude_transcript_context() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-context");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    let config_dir = repo_root.join("claude-config");
    write_recovery_transcript(&config_dir, &worktree);
    assert_eq!(
        db.latest_stage_run("recovery-task")
            .unwrap()
            .unwrap()
            .status,
        "running",
        "the restart path begins before an Exit can finalize the lost session"
    );
    let fake_claude = worktree.join(".kanna/test-provider-bin/claude");
    std::fs::write(
        &fake_claude,
        r#"#!/bin/sh
session_id=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--resume" ]; then
    session_id="$2"
    shift 2
  else
    shift
  fi
done
slug=$(printf '%s' "$PWD" | sed 's/[^[:alnum:]]/-/g')
transcript="$CLAUDE_CONFIG_DIR/projects/$slug/$session_id.jsonl"
grep -q prior-context-retained "$transcript" || exit 42
printf 'retained' > resume-proof.txt
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let fake_daemon = spawn_recovery_fake_daemon(config.daemon_dir.clone()).await;
    let (response, commands) = {
        let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
        let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
            config.clone(),
        )));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        let commands = fake_daemon.await.unwrap();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        (response, commands)
    };
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(command_line.contains(&format!("--resume '{RECOVERY_SESSION_ID}'")));
    assert_eq!(
        std::fs::read_to_string(worktree.join("resume-proof.txt")).unwrap(),
        "retained"
    );
    let interrupted = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    let interrupted_result: serde_json::Value =
        serde_json::from_str(interrupted.result.as_deref().unwrap()).unwrap();
    assert_eq!(
        interrupted_result["summary"],
        "task session was missing when the resume action began automatic provider-context recovery"
    );
    let run = loop {
        let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
        if run.id != "run-killed-mid-turn" {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        run.resumed_from_run_id.as_deref(),
        Some("run-killed-mid-turn")
    );
    assert_eq!(run.resume_fallback_reason, None);
    assert_eq!(run.model.as_deref(), Some(RECOVERY_MODEL));

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// A recorded verdict does not keep a PTY alive. A manual stage's agent
/// finishes its turn, records success, and parks at its composer; a reboot
/// then takes the session with a `succeeded` run behind it. That is the state
/// every singleton and every manual stage is in after a restart, and refusing
/// it is how restart recovery stopped working for them: the desktop's
/// attach-failure path calls this route, so a task the route would not resume
/// was a task whose terminal never came back.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn restart_recovery_resumes_a_session_lost_after_a_success_verdict() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-succeeded");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    let config_dir = repo_root.join("claude-config");
    write_recovery_transcript(&config_dir, &worktree);
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"stage complete"}"#),
        None,
    )
    .unwrap();

    let fake_claude = worktree.join(".kanna/test-provider-bin/claude");
    std::fs::write(
        &fake_claude,
        r#"#!/bin/sh
session_id=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--resume" ]; then
    session_id="$2"
    shift 2
  else
    shift
  fi
done
slug=$(printf '%s' "$PWD" | sed 's/[^[:alnum:]]/-/g')
transcript="$CLAUDE_CONFIG_DIR/projects/$slug/$session_id.jsonl"
grep -q prior-context-retained "$transcript" || exit 42
printf 'retained' > resume-proof.txt
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let fake_daemon = spawn_recovery_fake_daemon(config.daemon_dir.clone()).await;
    let (response, commands) = {
        let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
        let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
            config.clone(),
        )));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        let commands = fake_daemon.await.unwrap();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        (response, commands)
    };
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(
        command_line.contains(&format!("--resume '{RECOVERY_SESSION_ID}'")),
        "a lost session must reopen its own conversation, not replay the prompt: {command_line}"
    );
    assert!(
        command_line.contains("already recorded its stage verdict"),
        "a recovered success must not be told to finish interrupted work: {command_line}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("resume-proof.txt")).unwrap(),
        "retained"
    );

    // The succeeded run keeps its own verdict. Recovery adds a run beside it;
    // it never rewrites a real success as an interruption.
    let finished = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    assert_eq!(finished.status, "succeeded");
    let finished_result: serde_json::Value =
        serde_json::from_str(finished.result.as_deref().unwrap()).unwrap();
    assert_eq!(finished_result["summary"], "stage complete");

    let run = loop {
        let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
        if run.id != "run-killed-mid-turn" {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        run.resumed_from_run_id.as_deref(),
        Some("run-killed-mid-turn")
    );
    assert_eq!(run.resume_fallback_reason, None);

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The transcript preflight fails, so there is no conversation to reopen — but
/// the stage's verdict is already recorded. Before the succeeded-run fix this
/// case was unreachable (the route refused it); making it reachable must not
/// mean handing a finished agent the ordinary stage instructions and letting it
/// do the work a second time.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn succeeded_recovery_without_a_transcript_does_not_replay_the_finished_stage() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-succeeded-no-transcript");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"merged the queue and recorded the verdict"}"#),
        None,
    )
    .unwrap();
    // Deliberately no transcript: an empty config dir is what a preflight
    // failure looks like from `claude_transcript_exists`.
    let config_dir = repo_root.join("empty-claude-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    write_fresh_claude_probe(&worktree);

    let fake_daemon = spawn_recovery_fake_daemon(config.daemon_dir.clone()).await;
    let (response, commands) = {
        let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
        let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
            config.clone(),
        )));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        let commands = fake_daemon.await.unwrap();
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        (response, commands)
    };
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(
        !command_line.contains("--resume"),
        "there is no transcript to reopen: {command_line}"
    );
    assert!(
        command_line.contains("ALREADY completed and recorded its verdict"),
        "a fresh conversation after a recorded success must be told not to redo it: {command_line}"
    );
    assert!(
        command_line.contains("merged the queue and recorded the verdict"),
        "the fresh conversation has no memory, so the recorded result must be restated: {command_line}"
    );
    assert!(
        !command_line.contains("Implement the requested task in this worktree"),
        "ordinary stage execution instructions must not be issued as success recovery: {command_line}"
    );

    // The original success survives untouched.
    let finished = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    assert_eq!(finished.status, "succeeded");
    assert!(finished
        .result
        .as_deref()
        .unwrap()
        .contains("merged the queue and recorded the verdict"));

    let run = loop {
        let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
        if run.id != "run-killed-mid-turn" {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        run.resume_fallback_reason.as_deref(),
        Some("no claude CLI transcript for the previous session")
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The other exit from the same change: the transcript passes preflight, the
/// resume launches, and Claude rejects the session. That relaunch is forced
/// fresh, so it lands on the same fallback and must keep the same no-redo
/// semantics — the ancestor run, not the rejected recovery run, holds the
/// verdict.
#[tokio::test]
async fn rejected_resume_after_a_success_verdict_keeps_the_no_redo_instruction() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-rejected-succeeded");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"shipped the release and recorded it"}"#),
        None,
    )
    .unwrap();
    // The recovery run this fix creates: running, resuming the succeeded run.
    db.insert_stage_run(NewStageRun {
        id: "run-resume-attempt",
        task_id: "recovery-task",
        stage: "in progress",
        kind: "main",
        agent: None,
        agent_provider: Some("claude"),
        model: Some(RECOVERY_MODEL),
        effort: None,
        status: "running",
        result: None,
        feedback: None,
        session_id: Some("recovery-task"),
        provider_session_id: Some(RECOVERY_SESSION_ID),
        cwd: Some(worktree.to_string_lossy().as_ref()),
        resumed_from_run_id: Some("run-killed-mid-turn"),
    })
    .unwrap();
    write_fresh_claude_probe(&worktree);

    let fake_daemon = spawn_rejected_resume_fake_daemon(
        config.daemon_dir.clone(),
        rejected_resume_screen(),
        true,
    )
    .await;
    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        1,
    )
    .await
    .unwrap();
    let commands = fake_daemon.await.unwrap();

    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(
        !command_line.contains("--resume"),
        "the rejected session must not be asked for again: {command_line}"
    );
    assert!(
        command_line.contains("ALREADY completed and recorded its verdict"),
        "a rejected resume of a succeeded stage must still forbid the redo: {command_line}"
    );
    assert!(
        command_line.contains("shipped the release and recorded it"),
        "the ancestor's recorded result must be restated: {command_line}"
    );
    assert!(
        !command_line.contains("Implement the requested task in this worktree"),
        "ordinary stage execution instructions must not be issued as success recovery: {command_line}"
    );

    let finished = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    assert_eq!(
        finished.status, "succeeded",
        "the ancestor's verdict is not the rejected attempt's to rewrite"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// A succeeded run whose PTY is still alive has nothing to recover. The route
/// must refuse it without spawning anything and without disturbing the
/// recorded success. The daemon wait is bounded: on this path there is no
/// spawn to wait for, so an unbounded await would hang instead of failing.
#[tokio::test]
async fn a_live_succeeded_session_is_refused_without_spawning() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-succeeded-live");
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"still talking on a live session"}"#),
        None,
    )
    .unwrap();

    let daemon = spawn_listing_fake_daemon(config.daemon_dir.clone()).await;
    let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
        config.clone(),
    )));
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("task session is still alive"),
        "the live-session exclusion must survive admitting succeeded runs"
    );

    let observed = tokio::time::timeout(std::time::Duration::from_secs(10), daemon)
        .await
        .expect("the refusal must settle the daemon wait, not hang on a spawn that never comes")
        .unwrap();
    assert!(
        matches!(observed, kanna_daemon::protocol::Command::List),
        "only the presence probe may reach the daemon: {observed:?}"
    );

    let finished = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    assert_eq!(finished.status, "succeeded");
    assert!(finished
        .result
        .as_deref()
        .unwrap()
        .contains("still talking on a live session"));
    assert_eq!(
        db.latest_stage_run("recovery-task").unwrap().unwrap().id,
        "run-killed-mid-turn",
        "a refusal must not create a replacement run"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// Drive one `actions/resume` through the real route against a fake daemon
/// that reports no live session, and return the status plus what the daemon
/// was asked to spawn.
async fn resume_round(
    config: &crate::config::Config,
) -> (axum::http::StatusCode, Vec<kanna_daemon::protocol::Command>) {
    let fake_daemon = spawn_recovery_fake_daemon(config.daemon_dir.clone()).await;
    let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
        config.clone(),
    )));
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    let commands = fake_daemon.await.unwrap();
    (response.status(), commands)
}

fn spawned_command_line(commands: &[kanna_daemon::protocol::Command]) -> String {
    commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last().cloned(),
            _ => None,
        })
        .expect("replacement spawn command")
}

async fn wait_for_new_latest_run(db: &Db, previous_run_id: &str) -> crate::db::StageRun {
    loop {
        let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
        if run.id != previous_run_id {
            return run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The owner's scenario applied twice. A succeeded run recovers once; that
/// recovery run then dies too, and the second recovery must still know the
/// stage's verdict is recorded. Walking one hop found only the first
/// recovery's session-interruption bookkeeping and concluded the stage had
/// never succeeded — a full replay of finished work.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn a_second_recovery_still_knows_the_stage_already_succeeded() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-double");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    let config_dir = repo_root.join("claude-config");
    write_recovery_transcript(&config_dir, &worktree);
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"first pass finished and recorded"}"#),
        None,
    )
    .unwrap();
    let fake_claude = worktree.join(".kanna/test-provider-bin/claude");
    std::fs::write(&fake_claude, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);

    // First recovery: resumes A's conversation, told not to redo it.
    let (status, commands) = resume_round(&config).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let first = spawned_command_line(&commands);
    assert!(
        first.contains(&format!("--resume '{RECOVERY_SESSION_ID}'")),
        "{first}"
    );
    assert!(
        first.contains("already recorded its stage verdict"),
        "{first}"
    );
    let run_b = wait_for_new_latest_run(&db, "run-killed-mid-turn").await;
    assert_eq!(
        run_b.replaces_run_id.as_deref(),
        Some("run-killed-mid-turn"),
        "every replacement records what it replaced"
    );
    assert_eq!(
        run_b.resumed_from_run_id.as_deref(),
        Some("run-killed-mid-turn"),
        "this one really did carry --resume, so the narrower field is set too"
    );

    // Second recovery: B's session is gone too.
    let (status, commands) = resume_round(&config).await;
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    assert_eq!(status, axum::http::StatusCode::OK);
    let second = spawned_command_line(&commands);
    assert!(
        second.contains("already recorded its stage verdict"),
        "the second recovery must still find A's verdict: {second}"
    );
    assert!(
        !second.contains("finish the interrupted work"),
        "a recorded success is not interrupted work: {second}"
    );

    // B closed as bookkeeping, A untouched.
    let interrupted = db.stage_run(&run_b.id).unwrap().unwrap();
    assert_eq!(interrupted.status, "failed");
    assert_eq!(
        interrupted.feedback.as_deref(),
        Some(crate::http_api::SESSION_INTERRUPTION_FEEDBACK),
        "the bookkeeping marker is what makes this run transparent to the walk"
    );
    let original = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    assert_eq!(original.status, "succeeded");
    assert!(original
        .result
        .as_deref()
        .unwrap()
        .contains("first pass finished and recorded"));

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The same double recovery across a *fresh fallback*. B resumes nothing, so
/// `resumed_from_run_id` is null on it and cannot carry the chain; only the
/// replacement pointer reaches A. This is the case the one-hop ancestor
/// lookup could never have covered.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn a_second_recovery_after_a_fresh_fallback_keeps_the_success_verdict() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-double-fresh");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"queue merged, verdict recorded"}"#),
        None,
    )
    .unwrap();
    // No transcript anywhere: every resume here becomes a fresh fallback.
    let config_dir = repo_root.join("empty-claude-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    write_fresh_claude_probe(&worktree);

    let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);

    let (status, commands) = resume_round(&config).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let first = spawned_command_line(&commands);
    assert!(
        !first.contains("--resume"),
        "no transcript to reopen: {first}"
    );
    assert!(
        first.contains("ALREADY completed and recorded its verdict"),
        "{first}"
    );
    let run_b = wait_for_new_latest_run(&db, "run-killed-mid-turn").await;
    assert_eq!(
        run_b.resumed_from_run_id, None,
        "a fresh fallback resumed nothing; labelling it resumed would misfire the \
         rejected-resume observer and mislead API consumers"
    );
    assert_eq!(
        run_b.replaces_run_id.as_deref(),
        Some("run-killed-mid-turn"),
        "but it still replaced A, and that is what carries the verdict forward"
    );

    let (status, commands) = resume_round(&config).await;
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    assert_eq!(status, axum::http::StatusCode::OK);
    let second = spawned_command_line(&commands);
    assert!(
        second.contains("ALREADY completed and recorded its verdict"),
        "the chain through a fresh fallback must still reach A: {second}"
    );
    assert!(
        second.contains("queue merged, verdict recorded"),
        "A's recorded result must still be restated: {second}"
    );
    assert!(
        !second.contains("Implement the requested task in this worktree"),
        "ordinary stage instructions must never be issued as success recovery: {second}"
    );

    let original = db.stage_run("run-killed-mid-turn").unwrap().unwrap();
    assert_eq!(original.status, "succeeded");

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The walk must stop at a genuine verdict. A real agent failure after a
/// success means the stage is being worked again on purpose, and a recovery
/// of *that* is ordinary recovery — not a completed stage.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn a_genuine_failure_in_the_lineage_stops_the_completed_stage_walk() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-genuine-failure");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"an earlier pass succeeded"}"#),
        None,
    )
    .unwrap();
    // A later run at the same stage that really failed — its own verdict, not
    // session bookkeeping.
    db.insert_stage_run(NewStageRun {
        id: "run-real-failure",
        task_id: "recovery-task",
        stage: "in progress",
        kind: "main",
        agent: None,
        agent_provider: Some("claude"),
        model: Some(RECOVERY_MODEL),
        effort: None,
        status: "running",
        result: None,
        feedback: None,
        session_id: Some("recovery-task"),
        provider_session_id: Some(RECOVERY_SESSION_ID),
        cwd: Some(worktree.to_string_lossy().as_ref()),
        resumed_from_run_id: None,
    })
    .unwrap();
    db.finish_stage_run(
        "run-real-failure",
        "failed",
        Some(r#"{"status":"failure","summary":"the agent could not build the crate"}"#),
        Some("build failed"),
    )
    .unwrap();
    let config_dir = repo_root.join("empty-claude-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    write_fresh_claude_probe(&worktree);

    let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
    let (status, commands) = resume_round(&config).await;
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    assert_eq!(status, axum::http::StatusCode::OK);
    let command_line = spawned_command_line(&commands);
    assert!(
        !command_line.contains("ALREADY completed and recorded its verdict"),
        "a genuine failure is not a completed stage: {command_line}"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// A run with no lineage — what `rerun_stage` deliberately produces — never
/// inherits no-redo semantics, however many successes precede it. Redo intent
/// has to survive.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn a_lineage_free_run_after_a_success_recovers_with_ordinary_semantics() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-lineage-free");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    db.finish_stage_run(
        "run-killed-mid-turn",
        "succeeded",
        Some(r#"{"status":"success","summary":"the stage had already succeeded once"}"#),
        None,
    )
    .unwrap();
    // The shape rerun_stage leaves behind: running, no replacement lineage.
    db.insert_stage_run(NewStageRun {
        id: "run-explicit-rerun",
        task_id: "recovery-task",
        stage: "in progress",
        kind: "main",
        agent: None,
        agent_provider: Some("claude"),
        model: Some(RECOVERY_MODEL),
        effort: None,
        status: "running",
        result: None,
        feedback: None,
        session_id: Some("recovery-task"),
        provider_session_id: Some(RECOVERY_SESSION_ID),
        cwd: Some(worktree.to_string_lossy().as_ref()),
        resumed_from_run_id: None,
    })
    .unwrap();
    assert_eq!(
        db.stage_run("run-explicit-rerun")
            .unwrap()
            .unwrap()
            .replaces_run_id,
        None,
        "a rerun is a deliberate redo and must stay lineage-free"
    );
    let config_dir = repo_root.join("empty-claude-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    write_fresh_claude_probe(&worktree);

    let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
    let (status, commands) = resume_round(&config).await;
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    assert_eq!(status, axum::http::StatusCode::OK);
    let command_line = spawned_command_line(&commands);
    assert!(
        !command_line.contains("ALREADY completed and recorded its verdict"),
        "an explicit redo must not be told the stage is finished: {command_line}"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn restart_recovery_discovers_and_resumes_codex_by_worktree_cwd() {
    const CODEX_SESSION_ID: &str = "019d99a5-aa94-7c73-b786-644cc095c037";

    let (repo_root, config, db) = init_recovery_fixture("task-recovery-codex-context");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE stage_run
             SET agent_provider = 'codex', provider_session_id = NULL, model = NULL
             WHERE id = 'run-killed-mid-turn'",
            [],
        )
        .unwrap();
    let codex_home = repo_root.join("codex-home");
    let sessions_dir = codex_home.join("sessions/2026/09/03");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::write(
        sessions_dir.join(format!(
            "rollout-2026-09-03T07-00-00-{CODEX_SESSION_ID}.jsonl"
        )),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": CODEX_SESSION_ID,
                    "cwd": worktree.to_string_lossy(),
                },
            })
        ),
    )
    .unwrap();

    let fake_daemon = spawn_recovery_fake_daemon(config.daemon_dir.clone()).await;
    let (response, commands) = {
        let _env_guard = super::CODEX_HOME_LOCK.lock().unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);
        let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
            config.clone(),
        )));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        let commands = fake_daemon.await.unwrap();
        std::env::remove_var("CODEX_HOME");
        (response, commands)
    };
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(
        command_line.contains(&format!("resume '{CODEX_SESSION_ID}'")),
        "the hard-death path must discover the Codex rollout by cwd: {command_line}"
    );
    let run = loop {
        let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
        if run.id != "run-killed-mid-turn" {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        run.resumed_from_run_id.as_deref(),
        Some("run-killed-mid-turn")
    );
    assert_eq!(run.provider_session_id.as_deref(), Some(CODEX_SESSION_ID));
    assert_eq!(run.resume_fallback_reason, None);
    let events = db
        .list_task_events(
            &crate::db::TaskEventScope::Tasks(vec!["recovery-task".to_string()]),
            0,
            i64::MAX,
            100,
        )
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.event_type != "task.awaiting_advance"),
        "automatic restart recovery must not transiently advertise a manual advance: {events:?}"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Process-global provider store must stay fixed through prepare.
async fn restart_recovery_records_the_exact_codex_fallback_reason() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-codex-fallback");
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE stage_run
             SET agent_provider = 'codex', provider_session_id = NULL, model = NULL
             WHERE id = 'run-killed-mid-turn'",
            [],
        )
        .unwrap();
    let empty_codex_home = repo_root.join("empty-codex-home");
    std::fs::create_dir_all(empty_codex_home.join("sessions")).unwrap();

    let fake_daemon = spawn_recovery_fake_daemon(config.daemon_dir.clone()).await;
    let response = {
        let _env_guard = super::CODEX_HOME_LOCK.lock().unwrap();
        std::env::set_var("CODEX_HOME", &empty_codex_home);
        let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
            config.clone(),
        )));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        fake_daemon.await.unwrap();
        std::env::remove_var("CODEX_HOME");
        response
    };
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let run = loop {
        let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
        if run.id != "run-killed-mid-turn" {
            break run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(run.resumed_from_run_id, None);
    assert_eq!(
        run.resume_fallback_reason.as_deref(),
        Some("no Codex CLI transcript for the recorded session or previous run cwd")
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

#[tokio::test]
async fn resume_fallback_records_why_the_provider_context_was_not_restored() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-fallback");
    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        137,
    )
    .await
    .unwrap();
    let empty_config_dir = repo_root.join("empty-claude-config");
    std::fs::create_dir_all(empty_config_dir.join("projects")).unwrap();
    let prepared = {
        let _env_guard = super::CLAUDE_CONFIG_DIR_LOCK.lock().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &empty_config_dir);
        let prepared = prepare_resume_task_for_api(&db, &config, "recovery-task");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        prepared.unwrap()
    };
    assert!(prepared.resumed_workspace().is_none());
    assert_eq!(
        prepared.resume_fallback_reason.as_deref(),
        Some("no claude CLI transcript for the previous session")
    );
    // Losing the provider transcript must not also lose the task's model.
    assert_eq!(prepared.model.as_deref(), Some(RECOVERY_MODEL));

    let fake_daemon = spawn_fake_daemon_fork_transition(config.daemon_dir.clone(), 1).await;
    let mut daemon = DaemonClient::connect(&config.daemon_dir).await.unwrap();
    spawn_prepared_stage_run_for_api(
        &config.db_path,
        &mut daemon,
        &crate::session_replacements::SessionReplacements::default(),
        prepared,
    )
    .await
    .unwrap();
    fake_daemon.await.unwrap();

    let run = db.latest_stage_run("recovery-task").unwrap().unwrap();
    assert_eq!(run.resumed_from_run_id, None);
    assert_eq!(
        run.resume_fallback_reason.as_deref(),
        Some("no claude CLI transcript for the previous session")
    );
    assert_eq!(run.model.as_deref(), Some(RECOVERY_MODEL));

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The second half of the incident: the transcript passes preflight (or the
/// preflight cannot see the CLI's own verdict), the resume launches, and the
/// CLI rejects the session. The task must not be left dead with a transcript
/// that will never come back.
#[tokio::test]
async fn rejected_claude_resume_relaunches_the_stage_with_a_fresh_conversation() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-rejected");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    record_resume_attempt(&db, &worktree, Some(REVIEW_FEEDBACK));
    // A notification target proves the dead attempt reports nothing: this
    // stage has not failed, it has not run.
    db.update_test_pipeline_item_notify_task("recovery-task", "task-parent")
        .unwrap();
    write_fresh_claude_probe(&worktree);

    let fake_daemon = spawn_rejected_resume_fake_daemon(
        config.daemon_dir.clone(),
        rejected_resume_screen(),
        true,
    )
    .await;
    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        1,
    )
    .await
    .unwrap();
    let commands = fake_daemon.await.unwrap();

    let command_line = commands
        .iter()
        .find_map(|command| match command {
            kanna_daemon::protocol::Command::Spawn { args, .. } => args.last(),
            _ => None,
        })
        .expect("replacement spawn command");
    assert!(
        !command_line.contains("--resume"),
        "the relaunch must not ask for the rejected session again: {command_line}"
    );
    assert!(command_line.contains(&format!("--model '{RECOVERY_MODEL}'")));
    assert!(
        command_line.contains(REVIEW_FEEDBACK),
        "the fresh conversation must still be told what was asked for: {command_line}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("fresh-proof.txt")).unwrap(),
        RECOVERY_MODEL
    );

    let rejected = db.stage_run("run-resume-attempt").unwrap().unwrap();
    assert_eq!(rejected.status, "failed");
    assert!(
        rejected
            .result
            .as_deref()
            .is_some_and(|result| result.contains("rejected the recorded provider session")),
        "the rejected attempt should say why it never ran: {:?}",
        rejected.result
    );

    let replacement = db.latest_stage_run("recovery-task").unwrap().unwrap();
    assert_eq!(replacement.status, "running");
    assert_eq!(replacement.resumed_from_run_id, None);
    assert_eq!(replacement.model.as_deref(), Some(RECOVERY_MODEL));
    assert_eq!(replacement.feedback.as_deref(), Some(REVIEW_FEEDBACK));
    assert_eq!(replacement.stage, "in progress");
    assert!(
        replacement
            .resume_fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("No conversation found with session ID")),
        "the replacement must record that it runs on fresh context: {:?}",
        replacement.resume_fallback_reason
    );

    let item = db.get_pipeline_item("recovery-task").unwrap().unwrap();
    assert_eq!(item.activity.as_deref(), Some("working"));
    assert!(
        item.notified_at.is_none(),
        "a rejected resume must not claim a legacy notification target"
    );

    // Exactly once: the replacement is a fresh run, so an exit that prints the
    // same rejection is reported as the failure it is instead of starting yet
    // another session.
    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        1,
    )
    .await
    .unwrap();
    assert!(db.list_task_inputs("task-parent", 10).unwrap().is_empty());
    assert_eq!(
        db.latest_stage_run("recovery-task").unwrap().unwrap().id,
        replacement.id,
        "no third run may be started for the same rejection"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// The recovery is best-effort: when the fresh relaunch cannot start, the exit
/// must still be reported as the failure it was, not silently swallowed.
#[tokio::test]
async fn failed_fresh_relaunch_reports_the_original_agent_failure() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-rejected-fail");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    record_resume_attempt(&db, &worktree, None);
    db.update_test_pipeline_item_notify_task("recovery-task", "task-parent")
        .unwrap();
    write_fresh_claude_probe(&worktree);

    let fake_daemon = spawn_rejected_resume_fake_daemon(
        config.daemon_dir.clone(),
        rejected_resume_screen(),
        false,
    )
    .await;
    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        1,
    )
    .await
    .unwrap();
    let commands = fake_daemon.await.unwrap();

    let inputs = commands
        .iter()
        .filter_map(|command| match command {
            kanna_daemon::protocol::Command::SubmitInput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        inputs.is_empty(),
        "completion must not be submitted as task input"
    );
    assert!(
        !worktree.join("fresh-proof.txt").exists(),
        "a rejected spawn must not have started a provider"
    );

    let latest = db.latest_stage_run("recovery-task").unwrap().unwrap();
    assert_eq!(latest.status, "failed");
    assert!(
        latest
            .result
            .as_deref()
            .is_some_and(|result| result.contains("failed to start stage run")),
        "the failed relaunch should record why it never started: {:?}",
        latest.result
    );
    let item = db.get_pipeline_item("recovery-task").unwrap().unwrap();
    assert_eq!(item.activity.as_deref(), Some("unread"));

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// An ordinary agent failure is not a resume failure: nothing may be retried
/// with a fresh conversation just because the session died.
#[tokio::test]
async fn ordinary_agent_failure_in_a_resumed_run_is_not_retried_fresh() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-not-rejected");
    let worktree = repo_root.join(".kanna-worktrees/task-recovery");
    record_resume_attempt(&db, &worktree, None);

    let fake_daemon = spawn_rejected_resume_fake_daemon(
        config.daemon_dir.clone(),
        "error: the test suite failed\n".to_string(),
        false,
    )
    .await;
    crate::http_api::handle_task_terminal_state(
        &crate::http_api::AppState::new(config.clone()),
        "recovery-task",
        1,
    )
    .await
    .unwrap();

    let latest = db.latest_stage_run("recovery-task").unwrap().unwrap();
    assert_eq!(latest.id, "run-resume-attempt");
    assert_eq!(latest.status, "failed");
    assert!(
        latest
            .result
            .as_deref()
            .is_some_and(|result| result.contains("use kanna_resume_task")),
        "an ordinary failure keeps the interrupted-session reporting: {:?}",
        latest.result
    );
    fake_daemon.abort();

    let _ = std::fs::remove_dir_all(&repo_root);
}

/// A terminal-loss path records `runtime_status = 'exited'` alongside the
/// interrupted run. When the recovery it tells the operator to run finds the
/// session still alive and restores the run instead of spawning, that verdict
/// describes a process that is demonstrably still there — and it does not
/// self-heal, because the daemon writes a runtime status only when a session's
/// classification changes.
///
/// Left behind it is worse than a stale display value: `runtimeState:
/// "exited"` is one of the three terminations `kanna_wait_task`'s
/// `until: "finished"` resolves on, so every waiter on a live task would be
/// told it had finished, and the task-manager watcher would count the task as
/// parked.
#[tokio::test]
async fn resuming_into_a_live_session_clears_the_exited_runtime_verdict() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-live-runtime");
    // What `ErrorCode::HandoffLost` leaves behind: the run failed with the
    // interruption feedback, and the runtime dimension marked terminal.
    crate::http_api::mark_task_session_interrupted(
        &config.db_path,
        "recovery-task",
        "failed",
        "session lost during daemon handoff; use kanna_resume_task to recover",
    )
    .unwrap();
    assert_eq!(
        db.get_pipeline_item_runtime_status("recovery-task")
            .unwrap(),
        Some("exited".to_string()),
        "the interruption must record the terminal runtime verdict this test then clears"
    );

    let daemon = spawn_listing_fake_daemon(config.daemon_dir.clone()).await;
    let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
        config.clone(),
    )));
    let response = tower::ServiceExt::oneshot(
        app.clone(),
        axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(matches!(
        daemon.await.unwrap(),
        kanna_daemon::protocol::Command::List
    ));

    // Read it back the way a supervisor does, over HTTP, rather than off the
    // column: the point is what the task serves after the restore.
    let detail = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::get("/v1/tasks/recovery-task")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(detail.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(detail.into_body(), usize::MAX)
        .await
        .unwrap();
    let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(
        detail["runtimeState"],
        serde_json::json!("exited"),
        "the session is live; reporting it as exited finishes every wait on it: {detail}"
    );
    assert_eq!(
        detail["latestRun"]["status"],
        serde_json::json!("running"),
        "the restore itself must still have happened: {detail}"
    );

    let _ = std::fs::remove_dir_all(&repo_root);
}

#[tokio::test]
async fn restart_recovery_refuses_to_duplicate_a_live_running_session() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-running-live");
    let daemon = spawn_listing_fake_daemon(config.daemon_dir.clone()).await;
    let app = crate::http_api::router(std::sync::Arc::new(crate::http_api::AppState::new(
        config.clone(),
    )));
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::post("/v1/tasks/recovery-task/actions/resume")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("task session is still alive"),
        "the conflict must explain the failed hard-death precondition"
    );
    assert_eq!(
        db.latest_stage_run("recovery-task")
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(matches!(
        daemon.await.unwrap(),
        kanna_daemon::protocol::Command::List
    ));

    let _ = std::fs::remove_dir_all(&repo_root);
}

#[tokio::test]
async fn live_daemon_session_restores_a_false_interruption_instead_of_spawning() {
    let (repo_root, config, db) = init_recovery_fixture("task-recovery-live");
    db.cancel_running_stage_runs("recovery-task").unwrap();
    assert_eq!(
        db.latest_stage_run("recovery-task")
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );

    let daemon = spawn_listing_fake_daemon(config.daemon_dir.clone()).await;
    assert_eq!(
        super::super::daemon_session_presence(&config.daemon_dir, "recovery-task").await,
        super::super::DaemonSessionPresence::Present
    );
    assert!(
        crate::http_api::restore_task_run_for_live_session(&config.db_path, "recovery-task")
            .unwrap()
    );
    assert_eq!(
        db.latest_stage_run("recovery-task")
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(matches!(
        daemon.await.unwrap(),
        kanna_daemon::protocol::Command::List
    ));

    let _ = std::fs::remove_dir_all(&repo_root);
}
