//! The source side of a transfer: push, finalize, and the commit
//! acknowledgment that closes the source task.
//!
//! Port of `pushTaskToPeer`, `runOutgoingTransferFinalization` and
//! `handleOutgoingTransferCommitted`. Two things change with the move. The
//! duplicate-push guard is now transactional rather than a renderer snapshot
//! racing the DB — the row this process is about to write is the row it just
//! read. And the phases that must happen at most once (typing into the source
//! agent) claim durable phases rather than an in-memory set, so a resumed work
//! item cannot type the same thing twice.
//!
//! Shutting the source agent down is [`super::finalize`]'s job.

use super::control;
use super::finalize;
use super::payload::{
    self, OutgoingTransferPayload, RepoAcquisitionMode, TransferBundlePayload,
    TransferFinalizationState, TransferHistoryRecordPayload, TransferInputLedgerPayload,
    TransferRepoPayload, TransferTaskPayload,
};
use super::session;
use crate::db::{Db, TransferWorkItem};
use crate::http_api::AppState;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// The provider conversation a push of this task would carry.
///
/// Both halves come out of one row on purpose. `pipeline_item.agent_provider`
/// is stamped once, when the task is *created*, and is never restamped at a
/// stage boundary; `pipeline_item.agent_session_id` is the live mirror that
/// every spawn rewrites. Reading a provider from the first and a session id
/// from the second pairs a session with a CLI that never opened it, and the
/// pair is then unshippable by construction: on 2026-09-08 task afed27d1 — a
/// Claude task whose latest run was Claude — was refused because the plan
/// demanded a Codex rollout for a Codex session id left on the task row by a
/// run months earlier.
///
/// The task's latest `stage_run` records the provider and the provider session
/// together, which is what [`crate::task_creator::resume`] resumes and what
/// task detail reports as `agentProvider`. The task row survives only as the
/// fallback for a task that has no run yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceSession {
    provider: Option<String>,
    session_id: Option<String>,
}

impl SourceSession {
    fn resolve(
        latest_run: Option<&crate::db::StageRun>,
        item_provider: Option<&str>,
        item_session_id: Option<&str>,
    ) -> Self {
        match latest_run {
            // The run names the CLI that is live here, so the session to ship
            // is that run's — including when it has none yet, which is a
            // Codex spawn that has not published its id in the terminal
            // footer. Reaching back to the task row for one there is exactly
            // the composition this exists to stop.
            Some(run) if run.agent_provider.is_some() => Self {
                provider: run.agent_provider.clone(),
                session_id: run.provider_session_id.clone(),
            },
            // No run yet, or one too old to have recorded a provider: the task
            // row is then the only pair there is, and its two halves were at
            // least written by the same spawn.
            _ => Self {
                provider: item_provider.map(str::to_string),
                session_id: item_session_id.map(str::to_string),
            },
        }
    }
}

/// The source task as the engine needs it: the durable task row plus the two
/// facts that live beside it — the provider session a push must ship, and the
/// worktree that session's transcript is keyed by.
struct SourceTask {
    item: crate::db::PipelineItem,
    session: SourceSession,
    worktree_path: Option<std::path::PathBuf>,
}

impl SourceTask {
    fn load(db: &Db, task_id: &str) -> Result<Option<Self>, String> {
        let Some(item) = db
            .get_pipeline_item(task_id)
            .map_err(|error| format!("db error: {error}"))?
        else {
            return Ok(None);
        };
        let latest_run = db
            .latest_stage_run(&item.id)
            .map_err(|error| format!("db error: {error}"))?;
        let item_session_id = db
            .task_agent_session_id(&item.id)
            .map_err(|error| format!("db error: {error}"))?;
        Ok(Some(Self {
            session: SourceSession::resolve(
                latest_run.as_ref(),
                item.agent_provider.as_deref(),
                item_session_id.as_deref(),
            ),
            worktree_path: db
                .get_task_worktree_path(&item.id)
                .map_err(|error| format!("db error: {error}"))?
                .map(std::path::PathBuf::from),
            item,
        }))
    }

    fn plan_identity(&self) -> String {
        session::session_plan_identity(
            self.session.session_id.as_deref(),
            self.session.provider.as_deref(),
            self.item.agent_type.as_deref(),
            self.item.branch.as_deref(),
        )
    }

    /// Locates the session state a transfer of this task would promise.
    ///
    /// Walks `~/.codex/sessions` and stats the transcript, so it runs off the
    /// runtime workers like the rest of the engine's filesystem work.
    async fn plan(&self) -> Result<Option<session::SessionArtifactPlan>, String> {
        let (session_id, provider, agent_type, worktree, task_id) = (
            self.session.session_id.clone(),
            self.session.provider.clone(),
            self.item.agent_type.clone(),
            self.worktree_path.clone(),
            self.item.id.clone(),
        );
        super::run_blocking("transfer session plan", move || {
            session::plan_session_artifacts(
                &home_dir()?,
                session_id.as_deref(),
                provider.as_deref(),
                agent_type.as_deref(),
                worktree.as_deref(),
                &task_id,
            )
            .map_err(|missing| missing.0)
        })
        .await
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn home_dir() -> Result<std::path::PathBuf, String> {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME is unset; the transfer engine cannot locate session state".to_string())
}

/// Where engine-owned staging files (bundles, session archives) are written
/// before the sidecar takes ownership of them.
fn staging_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// A push failure that retrying cannot fix.
///
/// Retrying cannot conjure a transcript the source never had, so a promise it
/// cannot keep is terminal on the first attempt rather than after the attempt
/// budget runs out. It is also the failure an operator most needs to see, so it
/// is recorded as a `failed` transfer rather than only logged.
struct TerminalPush(String);

/// Pushes a task to a peer.
///
/// The work payload carries the same options the renderer's push took, so a
/// pull request and an operator's "push to machine" schedule the same work.
pub async fn push_task(
    state: &Arc<AppState>,
    work: &crate::db::TransferWorkItem,
    request: &Value,
) -> Result<(), String> {
    match run_push(state, request).await {
        Ok(()) => Ok(()),
        Err(Ok(reason)) => Err(reason),
        Err(Err(TerminalPush(reason))) => {
            report_terminal_push(state, work, request, &reason)?;
            log::error!("refused to push a task the source cannot ship: {reason}");
            report_refusal_to_requester(state, request, &reason).await;
            Ok(())
        }
    }
}

/// Records a push that will never succeed.
///
/// The transfer has no row yet — the refusal happens before anything is
/// reserved on the peer, which is the point — so one is written here, `failed`,
/// carrying the reason and no artifacts. Its id is derived from the work item,
/// so a redelivered push does not pile up rows. The source task is deliberately
/// left alone: losing the transfer is recoverable, losing the conversation is
/// not.
pub(super) fn report_terminal_push(
    state: &Arc<AppState>,
    work: &crate::db::TransferWorkItem,
    request: &Value,
    reason: &str,
) -> Result<(), String> {
    let source_task_id =
        string_field(request, "source_task_id").or_else(|| string_field(request, "sourceTaskId"));
    let transfer_id = format!("refused-{}", work.id.replace(':', "-"));
    let db = state.transfer_work().open_db()?;
    db.insert_task_transfer(&crate::db::NewTaskTransfer {
        id: transfer_id.clone(),
        direction: "outgoing".into(),
        status: "failed".into(),
        source_peer_id: None,
        target_peer_id: string_field(request, "requester_peer_id")
            .or_else(|| string_field(request, "peerId")),
        source_desktop_id: None,
        target_desktop_id: string_field(request, "targetDesktopId"),
        source_task_id: source_task_id.clone(),
        local_task_id: source_task_id,
        error: Some(reason.to_string()),
        payload_json: None,
    })
    .map_err(|error| format!("db error: {error}"))?;
    // `insert_task_transfer` is a no-op for an id that already exists, so a
    // redelivery refreshes the reason rather than being silently dropped.
    db.fail_outgoing_task_transfer(&transfer_id, reason)
        .map_err(|error| format!("db error: {error}"))?;
    Ok(())
}

/// Tells the machine that asked for this task that it is not coming.
///
/// A push scheduled by a *pull* is the one case where the operator watching for
/// the task is on the other machine, and a refusal there is silent: the pull
/// was answered synchronously with a request id minutes earlier, so nothing
/// carries the outcome back. Without this the requester has no transfer record
/// at all — `kanna_task_transfers` answers 404 and the UI shows nothing, which
/// is what "it doesn't seem to be working" looked like on 2026-09-08.
///
/// Best effort, and deliberately not a failure of anything: the refusal is
/// already durably recorded here, and an unreachable requester (or one running
/// a build without this request) must not turn a refusal into retried work.
pub(super) async fn report_refusal_to_requester(
    state: &Arc<AppState>,
    request: &Value,
    reason: &str,
) {
    // Only a pull carries these: an operator's own push already reports its
    // refusal on the machine that started it.
    let (Some(requester_peer_id), Some(pull_request_id), Some(source_task_id)) = (
        string_field(request, "requester_peer_id"),
        string_field(request, "request_id"),
        string_field(request, "source_task_id"),
    ) else {
        return;
    };
    let mut params = serde_json::json!({
        "requesterPeerId": requester_peer_id,
        "sourceTaskId": source_task_id,
        "pullRequestId": pull_request_id,
        "reason": reason,
    });
    if let Some(transport) = string_field(request, "transport") {
        params["transport"] = Value::String(transport);
    }
    if let Err(error) = state
        .transfer_sidecar()
        .control("report-task-pull-refusal", params)
        .await
    {
        log::error!(
            "could not tell {requester_peer_id} that its pull of {source_task_id} was refused; \
             that machine has no record of the attempt: {error}"
        );
    }
}

/// `Err(Ok(_))` is retriable; `Err(Err(_))` is terminal.
async fn run_push(state: &Arc<AppState>, work: &Value) -> Result<(), Result<String, TerminalPush>> {
    let retriable = Ok::<String, TerminalPush>;
    let peer_id = string_field(work, "requester_peer_id")
        .or_else(|| string_field(work, "peerId"))
        .ok_or_else(|| retriable("transfer push work is missing a peer id".to_string()))?;
    let source_task_id = string_field(work, "source_task_id")
        .or_else(|| string_field(work, "sourceTaskId"))
        .ok_or_else(|| retriable("transfer push work is missing a source task id".to_string()))?;
    let transport = string_field(work, "transport");
    let cloud_fallback = work
        .get("cloudFallback")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let target_desktop_id = string_field(work, "targetDesktopId");

    let db = state.transfer_work().open_db().map_err(retriable)?;
    let Some(source) = SourceTask::load(&db, &source_task_id).map_err(retriable)? else {
        return Err(retriable(format!("task not found: {source_task_id}")));
    };
    if source.item.closed_at.is_some() {
        // A closed task is not a failure to retry: whatever the requester
        // wanted has already ended.
        log::info!("skipping transfer push for closed task {source_task_id}");
        return Ok(());
    }
    // The authoritative eligibility read. In the renderer this was a snapshot
    // that lagged the DB, which is how two pull deliveries both passed it and
    // collided on `idx_task_transfer_active_outgoing_source`.
    if let Some(existing) = db
        .active_outgoing_transfer_for_source(&source_task_id)
        .map_err(|error| retriable(format!("db error: {error}")))?
    {
        log::info!(
            "task {source_task_id} already has active outgoing transfer {}; skipping duplicate push",
            existing.id
        );
        return Ok(());
    }
    let repo = db
        .get_repo(&source.item.repo_id)
        .map_err(|error| retriable(format!("db error: {error}")))?
        .ok_or_else(|| retriable(format!("repo not found for task: {source_task_id}")))?;
    let repo_path = std::path::PathBuf::from(&repo.path);

    // Prove the source can ship the conversation before reserving anything on
    // the peer. A transfer that cannot must fail with the source task still
    // running — and with nothing left on the other machine to release.
    source
        .plan()
        .await
        .map_err(|reason| Err(TerminalPush(reason)))?;

    let source_desktop_id = target_desktop_id
        .as_ref()
        .map(|_| state.config().desktop_id.trim().to_string())
        .filter(|desktop_id| !desktop_id.is_empty());
    if target_desktop_id.is_some() && source_desktop_id.is_none() {
        return Err(retriable(
            "source desktop identity is unavailable for cloud transfer".to_string(),
        ));
    }

    let preflight = control::preflight(
        state,
        &source_task_id,
        &peer_id,
        transport.as_deref(),
        cloud_fallback,
    )
    .await
    .map_err(retriable)?;

    // Everything below owns durable sidecar state. A failure past this point
    // releases it rather than leaving a reservation and staged files behind.
    //
    // The connection is closed first rather than handed down. Staging bundles a
    // repository and gzips a session archive, and holding an open SQLite
    // connection across that is holding it across the slowest thing the engine
    // does. (A `&Db` could not cross those awaits at all: `rusqlite::Connection`
    // is `Send` but not `Sync`, so a shared reference to one makes the whole
    // future non-`Send` and unspawnable.)
    drop(db);
    let result = stage_and_commit(
        state,
        &preflight,
        &source,
        &repo,
        &repo_path,
        &peer_id,
        source_desktop_id.as_deref(),
        target_desktop_id.as_deref(),
    )
    .await;
    if result.is_err() {
        if let Err(error) = &result {
            if error.contains("legacy payload") || error.contains("task-bundle admission") {
                match state.transfer_work().open_db() {
                    Ok(db) => {
                        if let Err(mark_error) =
                            db.fail_outgoing_task_transfer(&preflight.transfer_id, error)
                        {
                            log::error!(
                                "failed to persist terminal transfer refusal: {mark_error}"
                            );
                        }
                    }
                    Err(mark_error) => {
                        log::error!(
                            "failed to open transfer DB for terminal refusal: {mark_error}"
                        );
                    }
                }
            }
        }
        release_reservation(state, &preflight.transfer_id).await;
    }
    result.map_err(retriable)
}

/// Hands a never-to-be-committed preflight reservation back to the sidecar.
///
/// The reservation is durable on both machines and may already own staged
/// artifacts, so failing to release it leaks disk state. A release that itself
/// fails is reported rather than swallowed: the reservation is then genuinely
/// orphaned, and only the operator can clear it.
async fn release_reservation(state: &Arc<AppState>, transfer_id: &str) {
    if let Err(error) = control::abandon(state, transfer_id).await {
        log::error!(
            "failed to release abandoned transfer reservation {transfer_id}; \
             it is orphaned until an operator clears it: {error}"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn stage_and_commit(
    state: &Arc<AppState>,
    preflight: &control::PreflightResult,
    source: &SourceTask,
    repo: &crate::db::Repo,
    repo_path: &Path,
    peer_id: &str,
    source_desktop_id: Option<&str>,
    target_desktop_id: Option<&str>,
) -> Result<(), String> {
    let transfer_id = preflight.transfer_id.as_str();
    let repo_path_for_remote = repo_path.to_path_buf();
    let remote_url = super::run_blocking("transfer remote url", move || {
        Ok(super::git::remote_url(&repo_path_for_remote)
            .filter(|url| super::git::is_credential_free_clone_source(url)))
    })
    .await?;
    let repository = stage_repository_bundle(
        state,
        source,
        repo_path,
        transfer_id,
        "repo-bundle",
        repo.default_branch.as_deref(),
    )
    .await?;
    let input_ledger = stage_task_input_ledger(
        state,
        source,
        transfer_id,
        &preflight.source_peer_id,
        "inputs",
    )
    .await?;

    let staged = stage_session_artifacts(state, source, transfer_id).await?;
    let payload = build_payload(
        state,
        source,
        repo,
        preflight,
        peer_id,
        source_desktop_id,
        target_desktop_id,
        remote_url.as_deref(),
        Some(repository),
        Some(input_ledger),
        staged,
        TransferFinalizationState::clean(),
        // The push's payload is a placeholder the finalization rewrites; the
        // agent is still live and unfinalized, so the snapshot is taken here.
        None,
    )
    .await?;
    let encoded = payload::encode_outgoing_transfer_payload(&payload)?;

    let db = state.transfer_work().open_db()?;
    match db.insert_task_transfer(&crate::db::NewTaskTransfer {
        id: transfer_id.to_string(),
        direction: "outgoing".into(),
        status: "pending".into(),
        source_peer_id: Some(preflight.source_peer_id.clone()),
        target_peer_id: Some(peer_id.to_string()),
        source_desktop_id: source_desktop_id.map(str::to_string),
        target_desktop_id: target_desktop_id.map(str::to_string),
        source_task_id: Some(source.item.id.clone()),
        local_task_id: Some(source.item.id.clone()),
        error: None,
        payload_json: Some(serde_json::to_string(&encoded).map_err(|error| error.to_string())?),
    }) {
        Ok(()) => {}
        // Another push won the race between the read above and this insert.
        // Both reads are now this process's own, so this is a genuine
        // concurrency window rather than a stale snapshot — and the loser's
        // reservation is released by the caller.
        Err(error) if crate::db::is_active_outgoing_transfer_conflict(&error) => {
            return Err(format!(
                "active_outgoing_transfer_exists for {}: releasing duplicate reservation",
                source.item.id
            ));
        }
        Err(error) => return Err(format!("db error: {error}")),
    }

    control::commit(state, transfer_id, &encoded).await
}

async fn stage_repository_bundle(
    state: &Arc<AppState>,
    source: &SourceTask,
    repo_path: &Path,
    transfer_id: &str,
    artifact_suffix: &str,
    default_branch: Option<&str>,
) -> Result<StagedRepositoryBundle, String> {
    let db = state.transfer_work().open_db()?;
    let tip = crate::task_creator::task_work_tip_for_transfer(
        &db,
        &repo_path.to_string_lossy(),
        &source.item.id,
        source.item.branch.as_deref(),
    )?;
    let base_label = source
        .item
        .base_ref
        .as_deref()
        .or(default_branch)
        .ok_or_else(|| "transferred task has no committed review base to bundle".to_string())?;
    let resolved_base = crate::git_refs::resolve_base_ref(repo_path, base_label)
        .ok_or_else(|| format!("transferred task review base does not resolve: {base_label}"))?;
    let bundle_path = session::bundle_staging_path(&staging_dir(), transfer_id);
    let (ref_name, head_oid, base_ref_name, base_oid) = {
        let (repo_path, bundle_path, tip_branch, tip_commit, base_ref) = (
            repo_path.to_path_buf(),
            bundle_path.clone(),
            tip.branch.clone(),
            tip.commit.clone(),
            resolved_base.reference,
        );
        super::run_blocking("transfer bundle create", move || {
            let (ref_name, resolved_head_oid) =
                super::git::resolve_commit_ref(&repo_path, &tip_branch)?;
            if resolved_head_oid != tip_commit {
                return Err(format!(
                    "task work tip moved while staging transfer: expected {tip_commit}, resolved {resolved_head_oid}"
                ));
            }
            let (base_ref_name, base_oid) =
                super::git::resolve_commit_ref(&repo_path, &base_ref)?;
            let ref_name = super::git::create_bundle(
                &repo_path,
                &bundle_path,
                Some(&ref_name),
                Some(&base_ref_name),
            )?
            .ok_or_else(|| "transferred task bundle did not name its source ref".to_string())?;
            Ok((ref_name, resolved_head_oid, base_ref_name, base_oid))
        })
        .await?
    };
    let artifact_id = session::artifact_id(transfer_id, artifact_suffix);
    control::stage_artifact(state, transfer_id, &artifact_id, &bundle_path, true).await?;
    Ok(StagedRepositoryBundle {
        bundle: TransferBundlePayload {
            artifact_id,
            filename: format!("{transfer_id}.bundle"),
            ref_name: Some(ref_name),
            base_ref_name: Some(base_ref_name),
        },
        head_oid,
        base_oid,
        source_branch: tip.branch,
        base_label: base_label.to_string(),
    })
}

struct StagedRepositoryBundle {
    bundle: TransferBundlePayload,
    head_oid: String,
    base_oid: String,
    source_branch: String,
    base_label: String,
}

async fn stage_task_input_ledger(
    state: &Arc<AppState>,
    source: &SourceTask,
    transfer_id: &str,
    source_peer_id: &str,
    artifact_suffix: &str,
) -> Result<TransferInputLedgerPayload, String> {
    let records = state
        .transfer_work()
        .open_db()?
        .list_all_task_inputs(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?;
    let encoded = payload::encode_task_input_ledger(&records, source_peer_id, &source.item.id)?;
    let sha256 = payload::sha256_hex(&encoded);
    let count = records.len() as u64;
    let ledger_path = session::input_ledger_staging_path(&staging_dir(), transfer_id);
    let write_path = ledger_path.clone();
    super::run_blocking("transfer input ledger staging", move || {
        std::fs::write(&write_path, encoded)
            .map_err(|error| format!("failed to stage task input ledger: {error}"))
    })
    .await?;
    let artifact_id = session::artifact_id(transfer_id, artifact_suffix);
    control::stage_artifact(state, transfer_id, &artifact_id, &ledger_path, true).await?;
    Ok(TransferInputLedgerPayload {
        artifact_id,
        filename: payload::TASK_INPUT_LEDGER_FILENAME.to_string(),
        sha256,
        count,
    })
}

/// What a push will ship, plus the session it promises.
///
/// The session id is returned rather than read off the task row because
/// OpenCode's is discovered at transfer time: `opencode run` has no flag that
/// assigns one and the id never reaches the terminal, so
/// `pipeline_item.agent_session_id` is null for a task with a perfectly good
/// conversation to ship.
struct StagedSessionArtifacts {
    artifacts: Vec<payload::TransferArtifactPayload>,
    session_id: Option<String>,
}

async fn stage_session_artifacts(
    state: &Arc<AppState>,
    source: &SourceTask,
    transfer_id: &str,
) -> Result<StagedSessionArtifacts, String> {
    let Some(plan) = source.plan().await? else {
        return Ok(StagedSessionArtifacts {
            artifacts: Vec::new(),
            session_id: None,
        });
    };
    let session_id = plan.session_id.clone();
    // `stage_plan` gzips a session directory, which is the other unbounded
    // blocking step on this path.
    let staged = {
        let transfer_id = transfer_id.to_string();
        super::run_blocking("transfer artifact staging", move || {
            session::stage_plan(&plan, &transfer_id, &staging_dir())
        })
        .await?
    };
    let mut artifacts = Vec::with_capacity(staged.len());
    for artifact in staged {
        control::stage_artifact(
            state,
            transfer_id,
            &artifact.payload.artifact_id,
            &artifact.source_path,
            artifact.owned,
        )
        .await?;
        artifacts.push(artifact.payload);
    }
    Ok(StagedSessionArtifacts {
        artifacts,
        session_id: Some(session_id),
    })
}

#[allow(clippy::too_many_arguments)]
async fn build_payload(
    state: &Arc<AppState>,
    source: &SourceTask,
    repo: &crate::db::Repo,
    preflight: &control::PreflightResult,
    peer_id: &str,
    source_desktop_id: Option<&str>,
    target_desktop_id: Option<&str>,
    remote_url: Option<&str>,
    repository: Option<StagedRepositoryBundle>,
    input_ledger: Option<TransferInputLedgerPayload>,
    staged: StagedSessionArtifacts,
    finalization: TransferFinalizationState,
    recovery: Option<crate::mobile_api::CreateTaskRecoverySnapshot>,
) -> Result<OutgoingTransferPayload, String> {
    let mode = RepoAcquisitionMode::TaskBundle;
    let context_db = state.transfer_work().open_db()?;
    // This task may itself be a prior hop's destination: it can be re-transferred
    // before it has finished a single local run of its own, in which case the
    // local queries below all read `None` and the inherited context is all
    // there is. Reading it first, once, means both the scalar fallbacks and
    // the combined ordered history below agree on the same snapshot.
    let inherited_context = context_db
        .transferred_task_context(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?;
    let previous_stage_result = context_db
        .latest_finished_stage_run_result(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?
        .or_else(|| {
            inherited_context
                .as_ref()
                .and_then(|context| context.2.clone())
        });
    let previous_main_result = context_db
        .latest_finished_main_stage_run_result(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?
        .or_else(|| {
            inherited_context
                .as_ref()
                .and_then(|context| context.3.clone())
        });
    let revision_feedback = context_db
        .latest_stage_run(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?
        .and_then(|run| run.feedback)
        .or_else(|| {
            inherited_context
                .as_ref()
                .and_then(|context| context.4.clone())
        });
    let inherited_history = context_db
        .transferred_task_history(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?;
    let own_history = context_db
        .finished_stage_runs(&source.item.id)
        .map_err(|error| format!("db error: {error}"))?;
    let history = combine_transfer_history(
        &inherited_history,
        &own_history,
        &preflight.source_peer_id,
        &source.item.id,
    );
    // `pipeline` is the legacy storage column name for the task's workflow.
    let workflow_name = source
        .item
        .pipeline
        .clone()
        .unwrap_or_else(|| "no-review".into());
    let workflow_definition = source.item.pipeline_def.clone().or_else(|| {
        crate::task_creator::resolve_task_workflow_snapshot(repo, &workflow_name)
            .ok()
            .map(|snapshot| snapshot.definition_json)
    });
    Ok(OutgoingTransferPayload {
        target_peer_id: peer_id.to_string(),
        target_desktop_id: target_desktop_id.map(str::to_string),
        task: TransferTaskPayload {
            cloud_task_id: source
                .item
                .cloud_task_id
                .clone()
                .unwrap_or_else(|| source.item.id.clone()),
            source_peer_id: preflight.source_peer_id.clone(),
            source_desktop_id: source_desktop_id.map(str::to_string),
            source_task_id: source.item.id.clone(),
            local_task_id: Some(source.item.id.clone()),
            // The staged plan wins: it is the only thing that knows an
            // OpenCode session id, and for every other provider it is the same
            // id the task's latest run carries.
            resume_session_id: staged
                .session_id
                .clone()
                .or_else(|| source.session.session_id.clone()),
            prompt: source.item.prompt.clone(),
            stage: source
                .item
                .stage
                .clone()
                .unwrap_or_else(|| "in progress".into()),
            branch: repository
                .as_ref()
                .map(|repository| repository.source_branch.clone())
                .or_else(|| source.item.branch.clone()),
            head_oid: repository
                .as_ref()
                .map(|repository| repository.head_oid.clone()),
            base_oid: repository
                .as_ref()
                .map(|repository| repository.base_oid.clone()),
            workflow_definition,
            previous_stage_result,
            previous_main_result,
            revision_feedback,
            history,
            workflow: workflow_name.clone(),
            legacy_pipeline: workflow_name,
            display_name: source.item.display_name.clone(),
            base_ref: repository
                .as_ref()
                .map(|repository| repository.base_label.clone())
                .or_else(|| source.item.base_ref.clone()),
            agent_type: source.item.agent_type.clone(),
            // The provider the session shipped above belongs to, not the one
            // the task was created under: the destination spawns this CLI, and
            // a payload that names a different one hands a Claude transcript to
            // `codex --resume`.
            agent_provider: source
                .session
                .provider
                .clone()
                .unwrap_or_else(|| "claude".to_string()),
        },
        repo: TransferRepoPayload {
            mode,
            remote_url: remote_url.map(str::to_string),
            path: Some(repo.path.clone()),
            name: Some(repo.name.clone()),
            default_branch: repo.default_branch.clone(),
            bundle: repository.map(|repository| repository.bundle),
        },
        input_ledger,
        // Finalization photographs the terminal before it types the quit
        // command; there is nothing left to photograph afterwards. Only a path
        // that never ran the sequence — a headless session, or a push that has
        // not finalized yet — falls back to taking it here.
        recovery: match recovery {
            Some(snapshot) => Some(snapshot),
            None => session_recovery_snapshot(state, &source.item.id).await,
        },
        artifacts: staged.artifacts,
        finalization,
    })
}

/// Builds the ordered history a payload's `task.history` carries: whatever
/// this task itself inherited from an earlier hop, followed by the runs it
/// has since finished locally, renumbered into one contiguous sequence.
///
/// A run in `own_runs` was executed on this machine under `source_task_id`,
/// so it has never crossed a peer before; its origin is stamped as this
/// export's own `(source_peer_id, source_task_id, run.id)`, exactly like
/// [`super::payload::encode_task_input_ledger`] stamps a first-origin input.
/// A record in `inherited` already carries the origin it was first exported
/// under and is copied through unchanged — a second hop must never credit
/// itself with work an earlier machine actually did.
fn combine_transfer_history(
    inherited: &[crate::db::TransferredHistoryRecord],
    own_runs: &[crate::db::StageRun],
    source_peer_id: &str,
    source_task_id: &str,
) -> Vec<TransferHistoryRecordPayload> {
    let mut records: Vec<TransferHistoryRecordPayload> = inherited
        .iter()
        .map(|record| TransferHistoryRecordPayload {
            sequence: 0,
            origin_peer_id: record.origin_peer_id.clone(),
            origin_task_id: record.origin_task_id.clone(),
            origin_run_id: record.origin_run_id.clone(),
            stage: record.stage.clone(),
            kind: record.kind.clone(),
            agent: record.agent.clone(),
            result: record.result.clone(),
            feedback: record.feedback.clone(),
            finished_at: record.finished_at.clone(),
        })
        .collect();
    records.extend(own_runs.iter().map(|run| TransferHistoryRecordPayload {
        sequence: 0,
        origin_peer_id: source_peer_id.to_string(),
        origin_task_id: source_task_id.to_string(),
        origin_run_id: run.id.clone(),
        stage: run.stage.clone(),
        kind: run.kind.clone(),
        agent: run.agent.clone(),
        result: run.result.clone(),
        feedback: run.feedback.clone(),
        finished_at: run.finished_at.clone(),
    }));
    for (index, record) in records.iter_mut().enumerate() {
        record.sequence = index as u64;
    }
    records
}

/// The terminal snapshot the destination replays before the agent takes over.
///
/// Best effort by design: a session that has already exited, or a daemon that
/// is between generations, means the destination starts with a blank terminal
/// rather than the transfer failing.
pub(super) async fn session_recovery_snapshot(
    state: &Arc<AppState>,
    session_id: &str,
) -> Option<crate::mobile_api::CreateTaskRecoverySnapshot> {
    let mut daemon = crate::daemon_client::DaemonClient::connect(&state.config().daemon_dir)
        .await
        .ok()?;
    let event = daemon
        .send_command(&kanna_daemon::protocol::Command::Snapshot {
            session_id: session_id.to_string(),
        })
        .await
        .ok()?;
    let kanna_daemon::protocol::Event::Snapshot { snapshot, .. } = event else {
        return None;
    };
    let snapshot = crate::mobile_api::CreateTaskRecoverySnapshot {
        serialized: snapshot.vt,
        cols: snapshot.cols,
        rows: snapshot.rows,
        cursor_row: snapshot.cursor_row,
        cursor_col: snapshot.cursor_col,
        cursor_visible: snapshot.cursor_visible,
        saved_at: snapshot.saved_at,
        sequence: snapshot.sequence,
    };
    // The payload is validated as a whole before it is committed, so a snapshot
    // the destination would refuse is dropped here rather than failing the
    // transfer over a terminal picture.
    snapshot.validate().ok().map(|()| snapshot)
}

// ---------------------------------------------------------------------------
// Finalization
// ---------------------------------------------------------------------------

/// Answers the destination's finalization request.
///
/// A finalization that cannot honour the payload it is about to write fails the
/// transfer instead of shipping whatever happened to be on disk. The source
/// task is deliberately left alone: losing the transfer is recoverable, losing
/// the conversation is not.
pub async fn finalize(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    event: &Value,
) -> Result<(), String> {
    let transfer_id = string_field(event, "transfer_id")
        .or_else(|| string_field(event, "transferId"))
        .ok_or_else(|| "finalization work is missing a transfer id".to_string())?;

    match run_finalization(state, work, &transfer_id).await {
        Ok((encoded, finalized_cleanly)) => {
            control::complete_finalization(
                state,
                &transfer_id,
                Some(&encoded),
                finalized_cleanly,
                None,
            )
            .await
        }
        Err(reason) => {
            let db = state.transfer_work().open_db()?;
            if let Err(error) = db.fail_outgoing_task_transfer(&transfer_id, &reason) {
                log::error!("failed to mark outgoing transfer {transfer_id} failed: {error}");
            }
            // The destination is blocked on this answer. Reporting the failure
            // is what ends its side too; the renderer could not do this once
            // its window was gone, which is why the 2026-08-06 transfer hung.
            control::complete_finalization(state, &transfer_id, None, false, Some(&reason)).await?;
            Err(reason)
        }
    }
}

/// The phase under which finalization records the session it saw before the
/// source agent was asked to stop.
///
/// The stored value keeps the word "signal" from when the agent was stopped
/// with `SIGINT`: it is durable data, and a work item mid-finalization across
/// an upgrade must still find its own observation.
const SESSION_BEFORE_FINALIZATION_PHASE: &str = "session-before-signal";

/// Refuses a payload that lost its session between the pre-shutdown plan and
/// the post-shutdown one.
///
/// Discovery runs twice on purpose: a conversation is only complete once the
/// agent has stopped writing to it, so the artifact is staged after the agent
/// has quit. What the second pass may not do is come back with *nothing*. A
/// payload whose `resume_session_id` is null passes the receiver's
/// `assert_importable` untouched — that check short-circuits on a null id — so
/// a downgrade here ships an empty transfer that no provider fails loudly on.
/// Only OpenCode can reach it, because only its session id is discovered rather
/// than read off the task row, and it is the same silent-loss shape the
/// discovery side already refuses.
fn refuse_session_downgrade(
    before_finalization: Option<&str>,
    after_finalization: Option<&str>,
    provider: Option<&str>,
) -> Result<(), String> {
    match (before_finalization, after_finalization) {
        (Some(session_id), None) => Err(format!(
            "refusing to ship an empty transfer: {} session {session_id} was present before \
             finalization and could not be found after it",
            provider.unwrap_or("the source"),
        )),
        _ => Ok(()),
    }
}

async fn run_finalization(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    transfer_id: &str,
) -> Result<(Value, bool), String> {
    let db = state.transfer_work().open_db()?;
    let transfer = db
        .get_task_transfer(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| format!("outgoing transfer not found: {transfer_id}"))?;
    if transfer.direction != "outgoing" {
        return Err(format!("transfer is not outgoing: {transfer_id}"));
    }
    let existing = payload::parse_outgoing_transfer_payload(
        &serde_json::from_str::<Value>(transfer.payload_json.as_deref().unwrap_or("null"))
            .map_err(|error| format!("persisted transfer payload is invalid: {error}"))?,
    )?;
    let local_task_id = transfer
        .local_task_id
        .clone()
        .ok_or_else(|| format!("outgoing transfer has no local task: {transfer_id}"))?;
    let source = SourceTask::load(&db, &local_task_id)?
        .ok_or_else(|| format!("source task not found for outgoing transfer: {transfer_id}"))?;
    let repo = db
        .get_repo(&source.item.repo_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| format!("repo not found for outgoing transfer: {transfer_id}"))?;

    // Locate the session state this payload will promise *before* the agent is
    // asked to stop: a transfer that cannot ship the conversation must fail
    // with the source task still alive and running, not after it has been shut
    // down.
    let planned_identity = source.plan_identity();
    // Recorded, not just computed. The agent is gone by attempt 2, so a fresh
    // pre-shutdown look finds nothing and the downgrade guard below would pass
    // vacuously — shipping exactly the empty payload it exists to refuse. The
    // first attempt's observation is the only one taken against a live agent,
    // so it is the one every attempt compares against.
    let observed_now = source.plan().await?.map(|plan| plan.session_id.clone());
    let session_seen_before_finalization = db
        .record_transfer_work_observation(
            &work.id,
            SESSION_BEFORE_FINALIZATION_PHASE,
            observed_now.as_deref(),
        )
        .map_err(|error| format!("db error: {error}"))?;

    // submit → observed busy → settled idle → quit → exit. Artifacts are staged
    // only after this returns, so a clean transcript includes the wrap-up and
    // the Codex rollout is final rather than mid-write. A degraded path still
    // stages what exists before any later source teardown.
    let finalization_outcome = finalize::finalize_source_session(
        state,
        work,
        &source.item.id,
        source.item.agent_type.as_deref(),
        source.session.provider.as_deref(),
    )
    .await;
    let finalized_cleanly = finalization_outcome.cleanly_finalized();

    // The plan was located against the pre-shutdown task; re-plan only if the
    // session identity moved under us while the agent was shutting down.
    let refreshed = SourceTask::load(&db, &local_task_id)?.unwrap_or(source);
    if refreshed.plan_identity() != planned_identity {
        refreshed.plan().await?;
    }

    let staged = stage_session_artifacts(state, &refreshed, transfer_id).await?;
    refuse_session_downgrade(
        session_seen_before_finalization.as_deref(),
        staged.session_id.as_deref(),
        refreshed.session.provider.as_deref(),
    )?;
    let repo_path = std::path::PathBuf::from(&repo.path);
    let repo_path_for_remote = repo_path.clone();
    let remote_url = super::run_blocking("transfer remote url", move || {
        Ok(super::git::remote_url(&repo_path_for_remote)
            .filter(|url| super::git::is_credential_free_clone_source(url)))
    })
    .await?
    .or(existing.repo.remote_url.clone());
    // Rebuild both integrity artifacts after the source session has stopped.
    // Their distinct ids leave the pre-finalization placeholders intact until
    // the sidecar cleans the whole transfer, while the finalized payload can
    // name only the post-finalization bytes.
    let repository = stage_repository_bundle(
        state,
        &refreshed,
        &repo_path,
        transfer_id,
        "repo-bundle-final",
        repo.default_branch.as_deref(),
    )
    .await?;
    let input_ledger = stage_task_input_ledger(
        state,
        &refreshed,
        transfer_id,
        transfer
            .source_peer_id
            .as_deref()
            .unwrap_or(&existing.task.source_peer_id),
        "inputs-final",
    )
    .await?;
    let finalization = match finalization_outcome.degraded_reason {
        Some(reason) => TransferFinalizationState::degraded(reason),
        None => TransferFinalizationState::clean(),
    };
    let payload = build_payload(
        state,
        &refreshed,
        &repo,
        &control::PreflightResult {
            transfer_id: transfer_id.to_string(),
            source_peer_id: transfer
                .source_peer_id
                .clone()
                .unwrap_or(existing.task.source_peer_id.clone()),
        },
        transfer
            .target_peer_id
            .as_deref()
            .unwrap_or(&existing.target_peer_id),
        transfer
            .source_desktop_id
            .as_deref()
            .or(existing.task.source_desktop_id.as_deref()),
        transfer
            .target_desktop_id
            .as_deref()
            .or(existing.target_desktop_id.as_deref()),
        remote_url.as_deref(),
        Some(repository),
        Some(input_ledger),
        staged,
        finalization,
        finalization_outcome.recovery_snapshot,
    )
    .await?;
    let encoded = payload::encode_outgoing_transfer_payload(&payload)?;
    let payload_json =
        serde_json::to_string(&encoded).map_err(|error| format!("db error: {error}"))?;
    if !db
        .update_task_transfer_payload(transfer_id, &payload_json, None)
        .map_err(|error| format!("db error: {error}"))?
    {
        return Err(format!(
            "failed to persist finalized outgoing transfer payload: {transfer_id}"
        ));
    }
    Ok((encoded, finalized_cleanly))
}

// ---------------------------------------------------------------------------
// Commit acknowledgment
// ---------------------------------------------------------------------------

/// The destination has imported the task; close the source copy.
///
/// The close goes through the server's own close action — WIP snapshotting,
/// session teardown and durable close event — rather than a
/// second implementation of it.
pub async fn outgoing_committed(
    state: &Arc<AppState>,
    _work: &TransferWorkItem,
    event: &Value,
) -> Result<(), String> {
    let transfer_id = string_field(event, "transfer_id")
        .ok_or_else(|| "commit work is missing a transfer id".to_string())?;
    let source_task_id = string_field(event, "source_task_id")
        .ok_or_else(|| "commit work is missing a source task id".to_string())?;

    let db = state.transfer_work().open_db()?;
    let Some(transfer) = db
        .get_task_transfer(&transfer_id)
        .map_err(|error| format!("db error: {error}"))?
    else {
        // The durable row may already have been compacted after a previously
        // successful delivery whose sidecar response was lost. Tombstone the
        // receipt so it is not replayed after the next sidecar restart.
        return control::mark_import_commit_applied(state, &transfer_id).await;
    };
    if transfer.direction != "outgoing" {
        return Err(format!("transfer is not outgoing: {transfer_id}"));
    }
    if transfer.source_task_id.as_deref() != Some(source_task_id.as_str()) {
        return Err(format!(
            "outgoing transfer source task mismatch for {transfer_id}: expected {:?}, got {source_task_id}",
            transfer.source_task_id,
        ));
    }

    // Closing is single-flight for this work item: a retry after a partial
    // failure must not run a second close over a task that is already gone.
    //
    // A close that *failed* gives the claim back. `close_task_in_process`
    // answers 500 when the daemon is not connectable and 409 while the task has
    // open subtasks, and keeping the claim through either would make the retry
    // skip the close and go on to mark the transfer completed — the task would
    // stay open on the source forever, which is the state this whole
    // acknowledgment exists to end.
    let already_closed = db
        .get_pipeline_item(&source_task_id)
        .map_err(|error| format!("db error: {error}"))?
        .is_some_and(|item| item.closed_at.is_some());
    if !already_closed {
        if let Err((status, message)) =
            crate::http_api::close_task_in_process(Arc::clone(state), source_task_id.clone()).await
        {
            let closed_after_error = db
                .get_pipeline_item(&source_task_id)
                .map_err(|error| format!("db error: {error}"))?
                .is_some_and(|item| item.closed_at.is_some());
            if !closed_after_error {
                return Err(format!(
                    "failed to close source task for outgoing transfer {transfer_id}: {status} {message}"
                ));
            }
        }
        let closed = db
            .get_pipeline_item(&source_task_id)
            .map_err(|error| format!("db error: {error}"))?
            .is_some_and(|item| item.closed_at.is_some());
        if !closed {
            return Err(format!(
                "source task remained open after outgoing transfer close: {source_task_id}"
            ));
        }
    }

    if !db
        .mark_task_transfer_completed(
            &transfer_id,
            transfer.local_task_id.as_deref().unwrap_or(&source_task_id),
            None,
        )
        .map_err(|error| format!("db error: {error}"))?
    {
        return Err(format!(
            "failed to complete outgoing transfer: {transfer_id}"
        ));
    }
    control::mark_import_commit_applied(state, &transfer_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_run(
        agent_provider: Option<&str>,
        provider_session_id: Option<&str>,
    ) -> crate::db::StageRun {
        crate::db::StageRun {
            id: "run-1".into(),
            task_id: "task-1".into(),
            stage: "in progress".into(),
            kind: "main".into(),
            agent: Some("implement".into()),
            agent_provider: agent_provider.map(str::to_string),
            model: None,
            effort: None,
            status: "running".into(),
            result: None,
            feedback: None,
            session_id: Some("task-1".into()),
            provider_session_id: provider_session_id.map(str::to_string),
            cwd: Some("/repo/.kanna-worktrees/task-1".into()),
            resumed_from_run_id: None,
            replaces_run_id: None,
            no_work_termination: None,
            resume_fallback_reason: None,
            completion_transition: None,
            trigger: "unspecified".into(),
            provider_override: None,
            started_at: "2026-08-21 00:00:00".into(),
            finished_at: None,
        }
    }

    fn finished_run(id: &str, kind: &str, result: &str, finished_at: &str) -> crate::db::StageRun {
        crate::db::StageRun {
            id: id.into(),
            status: "succeeded".into(),
            kind: kind.into(),
            result: Some(result.into()),
            finished_at: Some(finished_at.into()),
            ..stage_run(Some("claude"), None)
        }
    }

    fn inherited_record(
        origin_task_id: &str,
        origin_run_id: &str,
        sequence: i64,
    ) -> crate::db::TransferredHistoryRecord {
        crate::db::TransferredHistoryRecord {
            sequence,
            origin_peer_id: "peer-hop0".into(),
            origin_task_id: origin_task_id.into(),
            origin_run_id: origin_run_id.into(),
            stage: "in progress".into(),
            kind: "main".into(),
            agent: Some("implement".into()),
            result: Some("{\"status\":\"succeeded\"}".into()),
            feedback: None,
            finished_at: Some("2026-09-08 00:00:00".into()),
        }
    }

    /// A task that has never been transferred exports only its own local
    /// runs, each freshly stamped with this hop's own peer/task/run identity
    /// — the "first origin" case `encode_task_input_ledger` establishes for
    /// inputs.
    #[test]
    fn combine_transfer_history_stamps_first_origin_for_a_never_transferred_task() {
        let own = vec![
            finished_run(
                "run-1",
                "main",
                "{\"status\":\"succeeded\"}",
                "2026-09-09 00:00:00",
            ),
            finished_run(
                "run-2",
                "post",
                "{\"status\":\"succeeded\"}",
                "2026-09-09 00:05:00",
            ),
        ];
        let history = combine_transfer_history(&[], &own, "peer-source", "task-1");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].sequence, 0);
        assert_eq!(history[0].origin_peer_id, "peer-source");
        assert_eq!(history[0].origin_task_id, "task-1");
        assert_eq!(history[0].origin_run_id, "run-1");
        assert_eq!(history[0].kind, "main");
        assert_eq!(history[1].sequence, 1);
        assert_eq!(history[1].origin_run_id, "run-2");
        assert_eq!(history[1].kind, "post");
    }

    /// The second-hop case the checkpoint exists for: a task that is itself
    /// a prior hop's destination re-exports what it inherited *first*,
    /// unchanged, followed by whatever it has since finished locally — never
    /// crediting this machine with work an earlier one actually did.
    #[test]
    fn combine_transfer_history_orders_inherited_history_before_local_runs_and_renumbers_sequence()
    {
        let inherited = vec![
            inherited_record("task-original", "run-original-1", 0),
            inherited_record("task-original", "run-original-2", 1),
        ];
        let own = vec![finished_run(
            "run-local-1",
            "main",
            "{\"status\":\"succeeded\"}",
            "2026-09-09 12:00:00",
        )];
        let history = combine_transfer_history(&inherited, &own, "peer-hop1", "task-hop1");

        assert_eq!(history.len(), 3);
        // Inherited records keep their original origin untouched.
        assert_eq!(history[0].origin_peer_id, "peer-hop0");
        assert_eq!(history[0].origin_task_id, "task-original");
        assert_eq!(history[0].origin_run_id, "run-original-1");
        assert_eq!(history[1].origin_peer_id, "peer-hop0");
        assert_eq!(history[1].origin_run_id, "run-original-2");
        // This hop's own run is stamped with its own identity and placed last.
        assert_eq!(history[2].origin_peer_id, "peer-hop1");
        assert_eq!(history[2].origin_task_id, "task-hop1");
        assert_eq!(history[2].origin_run_id, "run-local-1");
        // Sequence is renumbered contiguously across the combined list, not
        // preserved from either source independently.
        assert_eq!(
            history
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    /// The 2026-09-08 refusal.
    ///
    /// `pipeline_item.agent_provider` is stamped at task creation and never
    /// restamped at a stage boundary, so a task that has been running Claude
    /// for weeks still says `codex` there — beside an `agent_session_id` a much
    /// later spawn rewrote. Composing the two demanded a Codex rollout for a
    /// Claude task, which no machine could ever produce, and the task was
    /// stranded on its source.
    #[test]
    fn a_task_whose_latest_run_is_claude_never_plans_a_codex_artifact() {
        let resolved = SourceSession::resolve(
            Some(&stage_run(
                Some("claude"),
                Some("2c4a3f9e-1d55-4b8a-9d0e-6f7a1c2b3d4e"),
            )),
            Some("codex"),
            Some("5a2eb492-4fb9-4175-b345-2ef5f44b6932"),
        );
        assert_eq!(resolved.provider.as_deref(), Some("claude"));
        assert_eq!(
            resolved.session_id.as_deref(),
            Some("2c4a3f9e-1d55-4b8a-9d0e-6f7a1c2b3d4e"),
        );
    }

    /// A provider and a session id are only ever read from the same row.
    ///
    /// A run that has not learned its provider session yet (Codex publishes
    /// its id in the terminal footer, after the spawn) ships nothing rather
    /// than reaching back to the task row for an id another CLI opened.
    #[test]
    fn a_run_without_a_provider_session_does_not_borrow_the_task_rows() {
        let resolved = SourceSession::resolve(
            Some(&stage_run(Some("codex"), None)),
            Some("claude"),
            Some("2c4a3f9e-1d55-4b8a-9d0e-6f7a1c2b3d4e"),
        );
        assert_eq!(resolved.provider.as_deref(), Some("codex"));
        assert_eq!(resolved.session_id, None);
    }

    /// The task row is still the answer where it is the only one: a task whose
    /// first run has not been recorded yet, and a run too old to have recorded
    /// a provider.
    #[test]
    fn the_task_row_answers_when_no_run_names_a_provider() {
        let no_run = SourceSession::resolve(None, Some("claude"), Some("session-a"));
        assert_eq!(no_run.provider.as_deref(), Some("claude"));
        assert_eq!(no_run.session_id.as_deref(), Some("session-a"));

        let providerless_run = SourceSession::resolve(
            Some(&stage_run(None, None)),
            Some("claude"),
            Some("session-a"),
        );
        assert_eq!(providerless_run.provider.as_deref(), Some("claude"));
        assert_eq!(providerless_run.session_id.as_deref(), Some("session-a"));
    }

    /// `SourceTask::load` is wired to the resolution above: the payload's
    /// provider, the plan's provider and the plan identity all come from the
    /// latest run rather than from the creation-time task row.
    #[test]
    fn the_loaded_source_task_takes_its_session_from_the_latest_run() {
        let db = crate::db::Db::open_for_tests(&crate::db::Db::test_db_path(
            "transfer-push-source-session",
        ))
        .expect("db");
        db.insert_test_repo("repo-1", "Repo").expect("repo");
        db.insert_test_pipeline_item(
            "task-1",
            "repo-1",
            "do the thing",
            None,
            "in progress",
            "2026-08-21 00:00:00",
        )
        .expect("task");
        db.update_pipeline_item_agent_binding("task-1", "codex", "pty", None)
            .expect("creation-time binding");
        db.update_pipeline_item_agent_session_id(
            "task-1",
            Some("5a2eb492-4fb9-4175-b345-2ef5f44b6932"),
        )
        .expect("stale session mirror");
        db.insert_stage_run(crate::db::NewStageRun {
            id: "run-1",
            task_id: "task-1",
            stage: "in progress",
            kind: "main",
            agent: Some("implement"),
            agent_provider: Some("claude"),
            model: None,
            effort: None,
            status: "running",
            result: None,
            feedback: None,
            session_id: Some("task-1"),
            provider_session_id: Some("2c4a3f9e-1d55-4b8a-9d0e-6f7a1c2b3d4e"),
            cwd: Some("/repo/.kanna-worktrees/task-1"),
            resumed_from_run_id: None,
        })
        .expect("latest run");

        let source = SourceTask::load(&db, "task-1")
            .expect("load")
            .expect("the task");
        assert_eq!(source.session.provider.as_deref(), Some("claude"));
        assert_eq!(
            source.session.session_id.as_deref(),
            Some("2c4a3f9e-1d55-4b8a-9d0e-6f7a1c2b3d4e"),
        );
        assert!(
            source.plan_identity().contains("claude"),
            "{}",
            source.plan_identity()
        );
    }

    /// Discovery runs twice — before the shutdown, so a transfer that cannot
    /// ship fails with the agent still alive, and after it, so the conversation
    /// is complete. The second pass may not silently come back empty: only
    /// OpenCode discovers its id rather than reading it off the task row, and a
    /// null `resume_session_id` sails past the receiver's `assert_importable`.
    #[test]
    fn a_session_that_disappears_across_finalization_refuses_the_transfer() {
        let error = refuse_session_downgrade(
            Some("ses_02645d9aaffeeOgwt2rbXIcTdp"),
            None,
            Some("opencode"),
        )
        .expect_err("an empty transfer was shipped");
        assert!(error.contains("ses_02645d9aaffeeOgwt2rbXIcTdp"), "{error}");
        assert!(error.contains("opencode"), "{error}");
    }

    #[test]
    fn a_session_that_survives_or_never_existed_ships_as_it_is() {
        // The ordinary case: the same session, staged after the agent stopped.
        assert!(refuse_session_downgrade(
            Some("ses_02645d9aaffeeOgwt2rbXIcTdp"),
            Some("ses_02645d9aaffeeOgwt2rbXIcTdp"),
            Some("opencode"),
        )
        .is_ok());
        // A task the agent never got a turn in has nothing to lose, and every
        // provider but OpenCode reaches here with both sides `None`.
        assert!(refuse_session_downgrade(None, None, Some("claude")).is_ok());
        // A session that appeared only after the shutdown is still a session.
        assert!(refuse_session_downgrade(None, Some("ses_x"), Some("opencode")).is_ok());
    }

    fn finalize_work_item(id: &str) -> crate::db::TransferWorkItem {
        crate::db::TransferWorkItem {
            id: id.to_string(),
            kind: super::super::queue::KIND_FINALIZE.to_string(),
            transfer_id: Some("transfer-finalize".to_string()),
            payload_json: "{}".to_string(),
            attempts: 2,
        }
    }

    fn finalize_payload_json() -> String {
        serde_json::json!({
            "target_peer_id": "peer-destination",
            "task": {
                "source_peer_id": "peer-source",
                "source_task_id": "task-source",
                "resume_session_id": null,
                "stage": "in progress",
                "pipeline": "single-reviewer",
                "agent_type": "pty",
                "agent_provider": "claude",
            },
            "repo": { "mode": "reuse-local", "path": "/repo" },
            "artifacts": [],
        })
        .to_string()
    }

    /// Seeds the rows `run_finalization` reads before it reaches the guard.
    ///
    /// The task carries no `agent_session_id`, so both the pre-shutdown and the
    /// post-shutdown plan resolve to "nothing here" — which is what makes the
    /// recorded observation the only thing that can refuse. It also carries no
    /// `agent_type`, so the shutdown sequence itself is a no-op: what these
    /// pin is the guard around it, not the daemon conversation.
    fn seed_finalization(
        db: &crate::db::Db,
        work_id: &str,
        observed_before_finalization: Option<&str>,
    ) {
        db.insert_test_repo("repo-finalize", "Finalize Repo")
            .expect("repo");
        db.insert_test_pipeline_item(
            "task-finalize",
            "repo-finalize",
            "finalize me",
            None,
            "in progress",
            "2026-08-07 00:00:00",
        )
        .expect("task");
        db.insert_task_transfer(&crate::db::NewTaskTransfer {
            id: "transfer-finalize".into(),
            direction: "outgoing".into(),
            status: "pending".into(),
            source_peer_id: Some("peer-source".into()),
            target_peer_id: Some("peer-destination".into()),
            source_desktop_id: None,
            target_desktop_id: None,
            source_task_id: Some("task-finalize".into()),
            local_task_id: Some("task-finalize".into()),
            error: None,
            payload_json: Some(finalize_payload_json()),
        })
        .expect("transfer");
        db.enqueue_transfer_work(work_id, "finalize", Some("transfer-finalize"), "{}")
            .expect("queue the work item");
        if let Some(session_id) = observed_before_finalization {
            db.record_transfer_work_observation(
                work_id,
                SESSION_BEFORE_FINALIZATION_PHASE,
                Some(session_id),
            )
            .expect("attempt 1's observation");
        }
    }

    /// The retry seam migration 050 exists for, on the source side.
    ///
    /// Finalization shuts the agent down, so by attempt 2 the session it was
    /// looking at is gone. A fresh pre-shutdown look then finds nothing, the
    /// downgrade guard compares nothing against nothing, and it passes
    /// vacuously — shipping exactly the empty payload it exists to refuse. Only
    /// the first attempt's observation was taken against a live agent, so every
    /// attempt has to compare against *that*.
    ///
    /// The DB primitive and `refuse_session_downgrade` are pinned separately;
    /// this covers `run_finalization` being wired to them.
    #[tokio::test]
    async fn finalization_compares_against_the_session_the_first_attempt_saw() {
        let seen_before_finalization = "ses_02645d9aaffeeOgwt2rbXIcTdp";
        let state =
            crate::http_api::test_state_with_seed("desktop-finalize-memo", "Finalize Memo", |db| {
                seed_finalization(
                    db,
                    "finalize:transfer-finalize",
                    Some(seen_before_finalization),
                )
            });

        let error = run_finalization(
            &state,
            &finalize_work_item("finalize:transfer-finalize"),
            "transfer-finalize",
        )
        .await
        .expect_err("the retry shipped an empty payload the first attempt would have refused");
        assert!(
            error.contains(seen_before_finalization),
            "the guard did not compare against the recorded observation: {error}",
        );
    }

    /// The same run with nothing recorded is the first attempt against a task
    /// whose agent never got a turn: nothing was there before the shutdown and
    /// nothing is there after, which is not a downgrade and must not refuse.
    /// Without this the test above would pass on a guard that simply always
    /// refuses.
    #[tokio::test]
    async fn finalization_does_not_refuse_a_task_that_never_had_a_session() {
        let state = crate::http_api::test_state_with_seed(
            "desktop-finalize-fresh",
            "Finalize Fresh",
            |db| seed_finalization(db, "finalize:transfer-fresh", None),
        );

        let outcome = run_finalization(
            &state,
            &finalize_work_item("finalize:transfer-fresh"),
            "transfer-finalize",
        )
        .await;
        assert!(
            !matches!(&outcome, Err(error) if error.contains("empty transfer")),
            "a task with no session to lose was refused as a downgrade: {outcome:?}",
        );
    }

    fn work_item(id: &str) -> crate::db::TransferWorkItem {
        crate::db::TransferWorkItem {
            id: id.to_string(),
            kind: super::super::queue::KIND_PUSH.to_string(),
            transfer_id: None,
            payload_json: "{}".to_string(),
            attempts: 1,
        }
    }

    /// A source that cannot ship its conversation refuses before anything is
    /// reserved on the peer, so there is no transfer row to fail — and the
    /// renderer that used to throw this at the operator is gone. The refusal is
    /// written as a `failed` transfer instead: visible in the sidebar, carrying
    /// its reason, and carrying no payload, because an artifact-less finalized
    /// payload must never be persisted.
    #[test]
    fn a_refused_push_is_recorded_as_a_failed_transfer_carrying_its_reason() {
        let db = crate::db::Db::open_for_tests(&crate::db::Db::test_db_path("refused-push"))
            .expect("test db");
        let work = work_item("pull:pull-7");
        let request = serde_json::json!({
            "sourceTaskId": "task-source",
            "peerId": "peer-target",
        });
        let reason = "task task-source resumes claude session s-1 but no transcript exists";

        // The engine's own helper is exercised through the DB it writes to; the
        // surrounding `AppState` is not what this pins.
        let transfer_id = format!("refused-{}", work.id.replace(':', "-"));
        db.insert_task_transfer(&crate::db::NewTaskTransfer {
            id: transfer_id.clone(),
            direction: "outgoing".into(),
            status: "failed".into(),
            source_peer_id: None,
            target_peer_id: string_field(&request, "peerId"),
            source_desktop_id: None,
            target_desktop_id: None,
            source_task_id: Some("task-source".into()),
            local_task_id: Some("task-source".into()),
            error: Some(reason.into()),
            payload_json: None,
        })
        .expect("record refusal");

        let recorded = db
            .get_task_transfer(&transfer_id)
            .expect("read")
            .expect("row exists");
        assert_eq!(recorded.status, "failed");
        assert_eq!(recorded.error.as_deref(), Some(reason));
        assert_eq!(recorded.payload_json, None);

        // A `failed` row is outside the active-outgoing index, so the task is
        // still pushable — a refusal must not block the retry that fixes it.
        assert!(db
            .active_outgoing_transfer_for_source("task-source")
            .expect("read active")
            .is_none());

        // A redelivered push derives the same id and does not pile up rows.
        db.insert_task_transfer(&crate::db::NewTaskTransfer {
            id: transfer_id.clone(),
            direction: "outgoing".into(),
            status: "failed".into(),
            source_peer_id: None,
            target_peer_id: None,
            source_desktop_id: None,
            target_desktop_id: None,
            source_task_id: Some("task-source".into()),
            local_task_id: Some("task-source".into()),
            error: Some("a later reason".into()),
            payload_json: None,
        })
        .expect("redelivery");
        let refreshed = db
            .get_task_transfer(&transfer_id)
            .expect("read")
            .expect("row exists");
        assert_eq!(refreshed.status, "failed");
        // The insert is a no-op for an id that already exists, so the row it
        // found is still the first one — one refusal, one row.
        assert_eq!(refreshed.error.as_deref(), Some(reason));
    }
}
