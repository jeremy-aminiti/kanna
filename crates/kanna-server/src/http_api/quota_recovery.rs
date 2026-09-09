//! What the server does when a provider positively refuses a task's turn.
//!
//! The incident this exists for: `plan-build-review` declares
//! `"agent_provider": ["claude-fable", "codex-gpt-6-astra"]`, documented as an
//! outage-fallback chain. The account's Fable allowance ran out, a task
//! advanced to `review`, the stage spawned on the leading candidate, and the
//! session died on `You've reached your Fable limit`. Nothing classified it,
//! so it looked like an ordinary dead session; `kanna_rerun_stage` re-spawned
//! it on Fable, because a rerun feeds the recorded provider back in as an
//! explicit override. The ordered list never fell back once.
//!
//! Provider *availability* was already a fallback chain — but availability is
//! resolved by "is the executable on PATH", and an exhausted allowance is a
//! perfectly installed CLI. The missing half is this: a refusal is a runtime
//! event, so recovering from it is a runtime contract.
//!
//! The contract, in the order it is applied:
//!
//! - **The refusal is recorded before anything is decided.** One row per
//!   (run, provider, stated scope), so a replayed or re-adopted announcement
//!   lands on the row that already exists and can never become a second
//!   attempt.
//! - **An explicit single-provider override is binding.** A caller who named
//!   the provider for this stage owns that choice; quota recovery does not
//!   overrule one, it reports it.
//! - **Each remaining candidate is tried at most once, and only when the
//!   attempt is positively known not to have changed the workspace.** A dirty
//!   worktree parks the task instead: the workspace is preserved and a person
//!   decides, because a blind replacement of an attempt that may already have
//!   done something is exactly what "no blind replay" rules out.
//! - **Nothing here ever produces a success.** The refused run is closed as
//!   failed with the provider's own sentence as its result, before the
//!   replacement is spawned — never left running for the spawn path to mark
//!   succeeded on its way past. No stage advances, and no completion is
//!   fabricated.
//! - **When there is nothing left to try, the task parks in one state and
//!   stops.** One `task.provider_quota_parked` event, one task-detail field,
//!   no retry loop.

use super::state::AppState;
use crate::db::{
    Db, NewProviderRejection, ProviderRejection, QuotaRecovery, QuotaRejectionSource, TaskEventKind,
};
use crate::task_creator::ProviderCandidate;

/// One announcement from the daemon, whichever surface stated it.
pub(crate) struct QuotaRejectionNotice {
    pub session_id: String,
    pub provider: String,
    pub scope: Option<String>,
    pub rule_id: String,
    pub text: String,
    pub cli_version: Option<String>,
    pub source: QuotaRejectionSource,
}

/// Record a provider's refusal and, where the contract allows it, start the
/// stage's next ordered candidate exactly once.
///
/// Never returns an error to its caller: this rides the terminal-state
/// watcher, and a task whose recovery could not be decided must not take the
/// watcher's event loop down with it.
pub(crate) async fn handle_quota_rejection(state: &AppState, notice: QuotaRejectionNotice) {
    match apply_quota_rejection(state, &notice).await {
        Ok(Some(outcome)) => {
            log::warn!(
                "[quota] {} refused task {} at stage {} (scope {:?}); recovery: {}",
                notice.provider,
                outcome.task_id,
                outcome.stage,
                notice.scope,
                outcome.recovery.as_str(),
            );
            state.publish_task_state_changed(&outcome.task_id);
        }
        Ok(None) => {}
        Err(error) => log::warn!(
            "[quota] could not act on the rejection from {} for session {}: {error}",
            notice.provider,
            notice.session_id,
        ),
    }
}

struct RejectionOutcome {
    task_id: String,
    stage: String,
    recovery: QuotaRecovery,
}

/// The still-running attempt a refusal belongs to.
struct RefusedAttempt {
    task_id: String,
    stage: String,
    run_id: String,
    run_kind: String,
    model: Option<String>,
    effort: Option<String>,
    feedback: Option<String>,
    worktree_path: Option<String>,
    override_binding: bool,
}

async fn apply_quota_rejection(
    state: &AppState,
    notice: &QuotaRejectionNotice,
) -> Result<Option<RejectionOutcome>, String> {
    let config = state.config();
    // The connection is deliberately opened and dropped inside each
    // synchronous step rather than held across the spawn: `Db` is a
    // single-threaded rusqlite handle, and a future holding a reference to one
    // over an await cannot be sent to the runtime that drives this watcher.
    let Some((attempt, mutation, plan, rejection)) = ({
        let db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
        let Some(attempt) = refused_attempt(&db, notice)? else {
            return Ok(None);
        };
        // Everything below replaces a session or closes a run, so it takes the
        // same single-flight guard a close, a rerun or a stage change takes. A
        // task already being mutated has an owner for its next spawn; the
        // refusal is still recorded, without recovery, so the fact does not
        // vanish.
        let mutation = state.try_begin_requested_task_mutation(&attempt.task_id);
        let plan = if mutation.is_some() {
            decide_recovery(&db, &attempt, notice)?
        } else {
            RecoveryPlan {
                recovery: QuotaRecovery::ParkedConcurrentMutation,
                candidate: None,
            }
        };
        // Recorded before the recovery runs, and keyed by the refused run, so
        // a re-announcement from a re-adopted session finds this row rather
        // than starting a second fallback.
        db.record_provider_rejection(NewProviderRejection {
            task_id: &attempt.task_id,
            stage_run_id: &attempt.run_id,
            stage: &attempt.stage,
            provider: &notice.provider,
            model: attempt.model.as_deref(),
            effort: attempt.effort.as_deref(),
            source: notice.source,
            rule_id: &notice.rule_id,
            matched_text: &notice.text,
            scope: notice.scope.as_deref(),
            cli_version: notice.cli_version.as_deref(),
            recovery: plan.recovery,
            replacement_run_id: None,
        })
        .map_err(|error| format!("db error: {error}"))?
        // Already recorded. One refusal, one observation, one attempt.
        .map(|rejection| (attempt, mutation, plan, rejection))
    }) else {
        return Ok(None);
    };

    let recovery = match plan.candidate.clone() {
        Some(candidate) => start_fallback(state, &attempt, notice, &rejection, candidate).await?,
        None => plan.recovery,
    };

    {
        let db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
        announce(&db, &attempt, notice, &rejection, recovery)?;
    }
    drop(mutation);
    Ok(Some(RejectionOutcome {
        task_id: attempt.task_id,
        stage: attempt.stage,
        recovery,
    }))
}

#[derive(Clone)]
struct RecoveryPlan {
    recovery: QuotaRecovery,
    candidate: Option<ProviderCandidate>,
}

/// Which run a refusal belongs to, and every structural precondition for
/// acting on it.
///
/// A refusal against anything but an open task's own still-running attempt is
/// recorded nowhere and acted on not at all: there is no attempt to replace.
fn refused_attempt(
    db: &Db,
    notice: &QuotaRejectionNotice,
) -> Result<Option<RefusedAttempt>, String> {
    let Some(task_id) = db
        .resolve_pipeline_item_id(&notice.session_id)
        .map_err(|error| format!("db error: {error}"))?
    else {
        return Ok(None);
    };
    let Some(item) = db
        .get_pipeline_item(&task_id)
        .map_err(|error| format!("db error: {error}"))?
    else {
        return Ok(None);
    };
    if item.closed_at.is_some() {
        return Ok(None);
    }
    let Some(run) = db
        .latest_stage_run(&task_id)
        .map_err(|error| format!("db error: {error}"))?
    else {
        return Ok(None);
    };
    if run.status != "running" || run.session_id.as_deref() != Some(notice.session_id.as_str()) {
        return Ok(None);
    }
    // The provider that refused must be the one this run is actually using.
    // A stale announcement arriving after a replacement has already swapped
    // the session onto another CLI describes a run that no longer exists.
    if run.agent_provider.as_deref() != Some(notice.provider.as_str()) {
        return Ok(None);
    }
    Ok(Some(RefusedAttempt {
        task_id,
        stage: run.stage.clone(),
        run_id: run.id.clone(),
        run_kind: run.kind.clone(),
        model: run.model.clone(),
        effort: run.effort.clone(),
        feedback: run.feedback.clone(),
        worktree_path: run.cwd.clone(),
        override_binding: run.provider_override.is_some(),
    }))
}

fn decide_recovery(
    db: &Db,
    attempt: &RefusedAttempt,
    notice: &QuotaRejectionNotice,
) -> Result<RecoveryPlan, String> {
    let parked = |recovery| {
        Ok(RecoveryPlan {
            recovery,
            candidate: None,
        })
    };
    if attempt.override_binding {
        return parked(QuotaRecovery::ParkedOverrideBinding);
    }
    let Some(candidates) = crate::task_creator::stage_provider_candidates(db, &attempt.task_id)?
    else {
        return parked(QuotaRecovery::ParkedNoCandidateList);
    };
    // The list belongs to the stage the task occupies. A refusal recorded
    // against some other stage's run is not this list's business.
    if candidates.stage != attempt.stage || candidates.run_kind != attempt.run_kind {
        return parked(QuotaRecovery::ParkedNoCandidateList);
    }
    let mut rejected = db
        .providers_rejected_at_stage(&attempt.task_id, &attempt.stage)
        .map_err(|error| format!("db error: {error}"))?;
    rejected.push(notice.provider.clone());
    let Some(candidate) = candidates
        .candidates
        .iter()
        .find(|candidate| !rejected.contains(&candidate.provider))
        .cloned()
    else {
        return parked(QuotaRecovery::ParkedNoCandidates);
    };
    // The positive no-work test. A worktree with no change of any kind —
    // tracked, staged or untracked — is the evidence that this attempt has not
    // acted; anything else, including a change made by an earlier run in the
    // same workspace, parks the task and leaves the workspace exactly as it
    // is. The replacement never resets, forks or recreates anything, so
    // committed work is preserved either way; what this guards is asking a
    // second agent to redo something a first one may have half-finished.
    //
    // Its limit, stated so nobody has to rediscover it: an attempt that
    // committed everything and left a clean tree reads as untouched here. The
    // next candidate then starts on the same prompt in a workspace already
    // holding that commit — the same position an ordinary rerun puts a task
    // in, with the commit visible rather than lost. Tightening this needs a
    // per-run workspace baseline recorded at spawn.
    match attempt.worktree_path.as_deref() {
        Some(worktree) if workspace_is_untouched(worktree)? => {}
        Some(_) => return parked(QuotaRecovery::ParkedWorkObserved),
        // No recorded workspace is no evidence of an untouched one.
        None => return parked(QuotaRecovery::ParkedWorkObserved),
    }
    Ok(RecoveryPlan {
        recovery: QuotaRecovery::FallbackStarted,
        candidate: Some(candidate),
    })
}

/// Whether the worktree holds no uncommitted change at all.
///
/// `--untracked-files=all` on purpose: a new file an agent wrote and has not
/// staged is work, and the default summary would hide it inside a directory
/// entry. A git failure answers "not untouched" — the absence of evidence is
/// never evidence of absence here.
fn workspace_is_untouched(worktree_path: &str) -> Result<bool, String> {
    if !std::path::Path::new(worktree_path).is_dir() {
        return Ok(false);
    }
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(worktree_path)
        .output()
        .map_err(|error| format!("git status failed in {worktree_path}: {error}"))?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Close the refused attempt and start the next candidate once.
///
/// The refused run is finished as `failed` *before* the spawn, because the
/// spawn path finishes whatever is still running as `succeeded` on its way
/// past. A provider that refused the turn did not succeed at it.
async fn start_fallback(
    state: &AppState,
    attempt: &RefusedAttempt,
    notice: &QuotaRejectionNotice,
    rejection: &ProviderRejection,
    candidate: ProviderCandidate,
) -> Result<QuotaRecovery, String> {
    let config = state.config();
    let reason = format!(
        "{} refused this turn for spent quota ({}): {}",
        notice.provider,
        notice
            .scope
            .as_deref()
            .unwrap_or("no scope stated by the provider"),
        notice.text,
    );
    // Prepare before writing anything: a preparation failure must leave the
    // refused run exactly as it was, so the task parks with its own attempt
    // intact rather than with a closed run and no replacement.
    let db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
    let prepared = match crate::task_creator::prepare_provider_fallback_for_api(
        &db,
        config,
        &attempt.task_id,
        &attempt.run_id,
        candidate.clone(),
        &reason,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            log::warn!(
                "[quota] could not prepare the {} fallback for task {}: {error}",
                candidate.provider,
                attempt.task_id,
            );
            return finish_with(&db, rejection, QuotaRecovery::ParkedFallbackFailed, None);
        }
    };
    let replacement_provider = prepared.agent_provider.clone();
    if replacement_provider == notice.provider {
        // Resolution landed back on the refused provider — the stage's own
        // definition, the repo config or an agent's frontmatter outranked the
        // walk. Spawning it would repeat the refusal, so the task parks.
        log::warn!(
            "[quota] the {} fallback for task {} resolved back to {}; parking instead of \
             repeating the refusal",
            candidate.provider,
            attempt.task_id,
            replacement_provider,
        );
        return finish_with(&db, rejection, QuotaRecovery::ParkedFallbackFailed, None);
    }

    let refused_result = format!(
        "{} refused this stage for spent quota and recorded no work: {}. Kanna started the \
         stage's next authorized candidate ({}) in the same workspace.",
        notice.provider, notice.text, replacement_provider,
    );
    db.finish_stage_run(
        &attempt.run_id,
        "failed",
        Some(&refused_result),
        // A resumed revision's requested changes are part of its record and
        // survive the replacement.
        attempt.feedback.as_deref(),
    )
    .map_err(|error| format!("db error: {error}"))?;
    drop(db);

    let mut daemon = match crate::daemon_client::DaemonClient::connect(&config.daemon_dir).await {
        Ok(daemon) => daemon,
        Err(error) => {
            log::warn!("[quota] fallback could not reach the daemon: {error}");
            let db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
            return finish_with(&db, rejection, QuotaRecovery::ParkedFallbackFailed, None);
        }
    };
    let spawned = crate::task_creator::spawn_prepared_stage_run_for_api(
        &config.db_path,
        &mut daemon,
        &state.session_replacements,
        prepared,
    )
    .await;
    let db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
    match spawned {
        Ok(_) => {
            let replacement_run_id = db
                .latest_stage_run(&attempt.task_id)
                .map_err(|error| format!("db error: {error}"))?
                .map(|run| run.id);
            finish_with(
                &db,
                rejection,
                QuotaRecovery::FallbackStarted,
                replacement_run_id.as_deref(),
            )
        }
        Err(error) => {
            log::warn!(
                "[quota] the {replacement_provider} fallback for task {} failed to spawn: {error}",
                attempt.task_id,
            );
            finish_with(&db, rejection, QuotaRecovery::ParkedFallbackFailed, None)
        }
    }
}

fn finish_with(
    db: &Db,
    rejection: &ProviderRejection,
    recovery: QuotaRecovery,
    replacement_run_id: Option<&str>,
) -> Result<QuotaRecovery, String> {
    db.finish_provider_rejection_recovery(rejection.id, recovery, replacement_run_id)
        .map_err(|error| format!("db error: {error}"))?;
    Ok(recovery)
}

/// Append the durable events, and mark a parked task unread.
///
/// Two kinds, not one: every refusal is a `task.provider_quota_rejected`
/// record, and only the ones that leave the task waiting for a person also
/// emit `task.provider_quota_parked`. A manager watching for work to do
/// filters on the second and never has to interpret the first.
fn announce(
    db: &Db,
    attempt: &RefusedAttempt,
    notice: &QuotaRejectionNotice,
    rejection: &ProviderRejection,
    recovery: QuotaRecovery,
) -> Result<(), String> {
    let replacement_run_id = db
        .provider_rejections_for_task(&attempt.task_id)
        .map_err(|error| format!("db error: {error}"))?
        .into_iter()
        .find(|row| row.id == rejection.id)
        .and_then(|row| row.replacement_run_id);
    db.append_task_event(
        &attempt.task_id,
        TaskEventKind::ProviderQuotaRejected,
        serde_json::json!({
            "provider": notice.provider,
            "model": attempt.model,
            "effort": attempt.effort,
            "scope": notice.scope,
            "stage": attempt.stage,
            "stageRunId": attempt.run_id,
            "source": notice.source.as_str(),
            "ruleId": notice.rule_id,
            "matchedText": notice.text,
            "cliVersion": notice.cli_version,
            "recovery": recovery.as_str(),
            "replacementRunId": replacement_run_id,
        }),
    )
    .map_err(|error| format!("db error: {error}"))?;

    if !recovery.is_parked() {
        return Ok(());
    }
    let rejected_providers = db
        .providers_rejected_at_stage(&attempt.task_id, &attempt.stage)
        .map_err(|error| format!("db error: {error}"))?;
    db.append_task_event(
        &attempt.task_id,
        TaskEventKind::ProviderQuotaParked,
        serde_json::json!({
            "provider": notice.provider,
            "scope": notice.scope,
            "stage": attempt.stage,
            "stageRunId": attempt.run_id,
            "reason": recovery.as_str(),
            "rejectedProviders": rejected_providers,
            "action": parked_action(recovery),
        }),
    )
    .map_err(|error| format!("db error: {error}"))?;
    // The task is not failed and not finished — it is waiting. Unread is the
    // display state that says exactly that, and it is the one a human already
    // reads; nothing new is invented for quota.
    db.update_pipeline_item_activity(&attempt.task_id, "unread")
        .map_err(|error| format!("db error: {error}"))?;
    Ok(())
}

/// The parked-action sentence, for the test that pins them to operations that
/// exist. Operator instructions are a contract like any other surface.
#[cfg(test)]
pub(crate) fn parked_action_for_tests(recovery: QuotaRecovery) -> &'static str {
    parked_action(recovery)
}

/// What a person can actually do, in words, for each way of parking.
fn parked_action(recovery: QuotaRecovery) -> &'static str {
    match recovery {
        QuotaRecovery::FallbackStarted => "",
        QuotaRecovery::ParkedNoCandidates => {
            "Every provider this stage names has refused a turn here. Wait for an allowance to \
             reset, then rerun the stage with kanna_rerun_stage: it prefers a candidate that has \
             not been refused, and runs the recorded provider when none is left. To move the task \
             sooner, re-point the stage with kanna_replace_task_workflow and rerun it."
        }
        QuotaRecovery::ParkedNoCandidateList => {
            "This stage names no ordered provider candidates, so there was nothing to fall back \
             to. Wait for the allowance to reset, then rerun the stage with kanna_rerun_stage: it \
             runs on the recorded provider. To move the task sooner, give the stage a candidate \
             list with kanna_replace_task_workflow and rerun it."
        }
        QuotaRecovery::ParkedWorkObserved => {
            "The refused attempt had already changed its workspace, so Kanna did not replace it. \
             The workspace is untouched: review what is there, then rerun or resume the stage \
             deliberately."
        }
        QuotaRecovery::ParkedOverrideBinding => {
            "This run was started from an explicit provider override, which automatic recovery \
             does not overrule. Wait for the allowance to reset, then rerun the stage with \
             kanna_rerun_stage: it reproduces that override. To run this stage on another \
             provider, re-point it with kanna_replace_task_workflow, which supersedes the \
             override, and rerun it."
        }
        QuotaRecovery::ParkedFallbackFailed => {
            "The next candidate could not be started. The refused attempt's workspace is intact; \
             rerun the stage once the cause is cleared."
        }
        QuotaRecovery::ParkedConcurrentMutation => {
            "The task was already being closed, rerun or advanced when the refusal arrived, so \
             the refusal was recorded without recovery. Rerun the stage if it is still parked."
        }
    }
}
