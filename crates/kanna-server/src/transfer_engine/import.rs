//! The destination side of a transfer: record the request, acquire the
//! repository, materialize the conversation, create the task, acknowledge.
//!
//! Port of `recordIncomingTransfer`, `approveIncomingTransfer`,
//! `ensureIncomingTransferRepo` and `importTransferredResumeState`. The renderer
//! guarded this with two overlapping leases — a Tauri delivery lease and a DB
//! claim lease keyed on renderer-generated owner tokens — because two windows
//! could otherwise import the same transfer. One process cannot race itself:
//! the work queue's item id is the whole exclusion, and the DB claim columns
//! stay only as the record of which import owns the row.

use super::control;
use super::payload::{
    self, OutgoingTransferPayload, RepoAcquisitionMode, TransferHistoryRecordPayload,
};
use super::session;
use crate::db::TransferWorkItem;
use crate::http_api::AppState;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The claim token an engine import writes. The lease columns exist because the
/// DB statements still key on them; the exclusion itself is the work queue.
const ENGINE_CLAIM_TOKEN: &str = "kanna-server-transfer-engine";

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME is unset; the transfer engine cannot import session state".to_string())
}

/// Records an incoming transfer request and schedules its import.
///
/// Recording and importing are separate work items so the row exists — and is
/// visible in the sidebar — even if the import itself has to retry.
pub async fn record_incoming(state: &Arc<AppState>, event: &Value) -> Result<(), String> {
    let transfer_id = string_field(event, "transfer_id")
        .ok_or_else(|| "incoming transfer event is missing a transfer id".to_string())?;
    let source_peer_id = string_field(event, "source_peer_id");
    let source_task_id = string_field(event, "source_task_id");
    let raw_payload = event
        .get("payload")
        .ok_or_else(|| "incoming transfer event is missing a payload".to_string())?;
    let parsed = payload::parse_outgoing_transfer_payload(raw_payload)?;

    let queue = state.transfer_work();
    let db = queue.open_db()?;
    db.insert_task_transfer(&crate::db::NewTaskTransfer {
        id: transfer_id.clone(),
        direction: "incoming".into(),
        status: "pending".into(),
        source_peer_id,
        target_peer_id: None,
        source_desktop_id: parsed.task.source_desktop_id.clone(),
        target_desktop_id: parsed.target_desktop_id.clone(),
        source_task_id,
        local_task_id: None,
        error: None,
        payload_json: Some(
            serde_json::to_string(raw_payload).map_err(|error| format!("db error: {error}"))?,
        ),
    })
    .map_err(|error| format!("db error: {error}"))?;
    if parsed.repo.mode != RepoAcquisitionMode::TaskBundle {
        let reason = "incoming transfer uses an unsupported legacy payload; task-bundle admission is required";
        db.fail_incoming_task_transfer(&transfer_id, reason)
            .map_err(|error| format!("db error: {error}"))?;
        // Best-effort: this is a fast, zero-I/O decision the sidecar can
        // relay to the source immediately instead of making it wait out the
        // admission timeout for an outcome that is already known. An old or
        // unreachable sidecar just means the source falls back to today's
        // timeout-based unresolved outcome — the durable refusal above
        // already stands regardless.
        if let Err(error) =
            control::mark_incoming_transfer_refused(state, &transfer_id, reason).await
        {
            log::warn!(
                "could not relay definitive refusal for transfer {transfer_id} to the sidecar; \
                 the source will see the existing timeout-based unresolved outcome instead: {error}"
            );
        }
        return Err(reason.to_string());
    }
    control::mark_incoming_event_recorded(state, &transfer_id).await?;
    queue.enqueue(
        &format!("import:{transfer_id}"),
        super::queue::KIND_IMPORT,
        Some(&transfer_id),
        &serde_json::json!({ "transferId": transfer_id }),
    )?;
    Ok(())
}

/// Records a pull this machine asked for that the source will not ship.
///
/// The machine that starts a pull is the one watching for the task to arrive,
/// and until this existed it was the only party told nothing: the refusal was
/// recorded on the source, and here `GET /v1/tasks/{id}/transfers` answered
/// 404 — "no such task" — which is exactly what the operator already knew.
///
/// The row is `incoming` and `failed` with no `local_task_id`, because nothing
/// arrived and nothing ever will. It carries the source's task id, so
/// `kanna_task_transfers` (which matches either id) answers with it, and the
/// snapshot's transfer alerts turn it into a toast for the operator who
/// started the move.
pub async fn record_pull_refusal(state: &Arc<AppState>, event: &Value) -> Result<(), String> {
    let request_id = string_field(event, "request_id")
        .ok_or_else(|| "task pull refusal is missing a request id".to_string())?;
    let source_peer_id = string_field(event, "source_peer_id")
        .ok_or_else(|| "task pull refusal is missing a source peer id".to_string())?;
    let source_task_id = string_field(event, "source_task_id")
        .ok_or_else(|| "task pull refusal is missing a source task id".to_string())?;
    let reason = string_field(event, "reason")
        .unwrap_or_else(|| "the source machine reported no reason".to_string());

    let db = state.transfer_work().open_db()?;
    // Derived from the pull rather than random: a redelivered refusal has to
    // land on the row the first delivery wrote, not pile a second one up.
    let transfer_id = format!("refused-pull-{source_peer_id}-{request_id}");
    // Inserted `pending` and failed in the next statement rather than written
    // `failed` outright: only the fail route stamps `completed_at`, and a row
    // that never gets one reads as a transfer still in progress.
    db.insert_task_transfer(&crate::db::NewTaskTransfer {
        id: transfer_id.clone(),
        direction: "incoming".into(),
        status: "pending".into(),
        source_peer_id: Some(source_peer_id),
        target_peer_id: None,
        source_desktop_id: None,
        target_desktop_id: None,
        source_task_id: Some(source_task_id.clone()),
        local_task_id: None,
        error: None,
        payload_json: None,
    })
    .map_err(|error| format!("db error: {error}"))?;
    // Both statements are no-ops once the row is terminal, so a redelivered
    // refusal collapses onto the record the first one wrote instead of piling
    // a second row up or reopening this one.
    db.fail_incoming_task_transfer(&transfer_id, &reason)
        .map_err(|error| format!("db error: {error}"))?;
    log::warn!("the source machine refused to send task {source_task_id}: {reason}");
    Ok(())
}

/// Rejects an incoming transfer on the operator's behalf.
pub async fn reject_transfer(state: &Arc<AppState>, work: &Value) -> Result<(), String> {
    let transfer_id = string_field(work, "transferId")
        .ok_or_else(|| "reject work is missing a transfer id".to_string())?;
    let db = state.transfer_work().open_db()?;
    let transfer = db
        .get_task_transfer(&transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| format!("incoming transfer not found: {transfer_id}"))?;
    if transfer.direction != "incoming" {
        return Err(format!("transfer is not incoming: {transfer_id}"));
    }
    if !matches!(transfer.status.as_str(), "rejected") {
        db.mark_task_transfer_rejected(&transfer_id, "Rejected locally")
            .map_err(|error| format!("db error: {error}"))?;
    }
    release_incoming_reservation(state, &transfer_id).await
}

/// Releases the sidecar state a transfer that settled earlier still owns.
///
/// Queued by the engine's startup sweep for every terminal incoming row whose
/// cleanup never completed. The renderer used to do this at window mount, and
/// it is the only thing that frees a *committed* reservation: those are exempt
/// from TTL pruning, so each one left behind permanently consumes one of the
/// destination's bounded reservation slots.
pub async fn release_settled_reservation(
    state: &Arc<AppState>,
    work: &Value,
) -> Result<(), String> {
    let transfer_id = string_field(work, "transferId")
        .ok_or_else(|| "sidecar cleanup work is missing a transfer id".to_string())?;
    release_incoming_reservation(state, &transfer_id).await
}

/// Releases the sidecar state a settled incoming transfer still owns.
async fn release_incoming_reservation(
    state: &Arc<AppState>,
    transfer_id: &str,
) -> Result<(), String> {
    control::mark_import_ack_completed(state, transfer_id).await?;
    state
        .transfer_work()
        .open_db()?
        .mark_incoming_transfer_sidecar_cleanup_completed(transfer_id)
        .map_err(|error| format!("db error: {error}"))?;
    Ok(())
}

/// Imports an approved incoming transfer.
///
/// Runs the whole sequence the renderer did — finalize the source, acquire the
/// repository, materialize the conversation, create the task, acknowledge the
/// commit — but every step that must happen once claims a durable phase, so a
/// resumed work item continues rather than repeating.
pub async fn import_transfer(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    request: &Value,
) -> Result<(), String> {
    let transfer_id = string_field(request, "transferId")
        .ok_or_else(|| "import work is missing a transfer id".to_string())?;
    match run_import(state, work, &transfer_id).await {
        Ok(()) => Ok(()),
        Err(ImportFailure::Retry(reason)) => Err(reason),
        // Retrying cannot conjure session state the payload never carried, so
        // this transfer is terminal now rather than after N attempts — both
        // machines need a visible end state.
        Err(ImportFailure::Terminal(reason)) => {
            let db = state.transfer_work().open_db()?;
            db.fail_incoming_task_transfer(&transfer_id, &reason)
                .map_err(|error| format!("db error: {error}"))?;
            log::error!("refused incoming transfer {transfer_id}: {reason}");
            release_incoming_reservation(state, &transfer_id).await?;
            Ok(())
        }
    }
}

#[derive(Debug)]
pub(crate) enum ImportFailure {
    Retry(String),
    Terminal(String),
}

impl From<String> for ImportFailure {
    fn from(reason: String) -> Self {
        Self::Retry(reason)
    }
}

async fn run_import(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    transfer_id: &str,
) -> Result<(), ImportFailure> {
    let db = state.transfer_work().open_db()?;
    let transfer = db
        .get_task_transfer(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| format!("incoming transfer not found: {transfer_id}"))?;
    if transfer.direction != "incoming" {
        return Err(format!("transfer is not incoming: {transfer_id}").into());
    }
    if matches!(
        transfer.status.as_str(),
        "completed" | "rejected" | "failed"
    ) {
        return Ok(());
    }
    db.claim_pending_incoming_transfer(transfer_id, ENGINE_CLAIM_TOKEN, true)
        .map_err(|error| format!("db error: {error}"))?;

    let mut local_task_id = transfer.local_task_id.clone();
    let stored = payload::parse_outgoing_transfer_payload(
        &serde_json::from_str::<Value>(transfer.payload_json.as_deref().unwrap_or("null"))
            .map_err(|error| format!("persisted transfer payload is invalid: {error}"))?,
    )
    .map_err(ImportFailure::Terminal)?;

    // Every recovery path must honor the same integrity contract as the first
    // attempt. In particular, a pre-existing local_task_id is not evidence
    // that the repository, head, or durable history was ever imported.
    if stored.repo.mode != RepoAcquisitionMode::TaskBundle {
        return Err(ImportFailure::Terminal(
            "source server uses a legacy transfer payload that cannot prove the task head or durable input ledger; update it and retry the transfer".into(),
        ));
    }

    let persisted_manifest = db
        .transferred_task_manifest(transfer_id)
        .map_err(|error| format!("db error: {error}"))?;
    let has_manifest = persisted_manifest.is_some();
    let payload = if local_task_id.is_some() || has_manifest {
        stored
    } else {
        let finalized = control::finalize_from_source(state, transfer_id).await?;
        let payload = payload::parse_outgoing_transfer_payload(&finalized.payload)
            .map_err(ImportFailure::Terminal)?;
        if payload.repo.mode != RepoAcquisitionMode::TaskBundle {
            return Err(ImportFailure::Terminal(
                "finalized source payload lost the task-bundle integrity contract; source remains recoverable".into(),
            ));
        }
        assert_payload_matches_reservation(&transfer, &payload)?;
        if !finalized.finalized_cleanly {
            // The source could not shut its agent down cleanly. The
            // conversation still crosses, but this machine's operator has to
            // know the handoff was degraded — it is their task now.
            log::warn!(
                "incoming transfer {transfer_id} was not cleanly finalized: {}",
                payload
                    .finalization
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("the source reported no reason"),
            );
        }
        let payload_json = serde_json::to_string(&finalized.payload)
            .map_err(|error| format!("db error: {error}"))?;
        if !db
            .update_task_transfer_payload(transfer_id, &payload_json, Some(ENGINE_CLAIM_TOKEN))
            .map_err(|error| format!("db error: {error}"))?
        {
            return Err(format!(
                "failed to persist finalized incoming transfer payload: {transfer_id}"
            )
            .into());
        }
        payload
    };

    let destination_task_id = persisted_manifest
        .as_ref()
        .and_then(|(_, _, _, task_id, _)| task_id.clone())
        .or_else(|| local_task_id.clone())
        .unwrap_or_else(|| session::destination_task_id(transfer_id));
    if persisted_manifest
        .as_ref()
        .is_some_and(|(_, _, _, bound, _)| {
            bound
                .as_deref()
                .zip(local_task_id.as_deref())
                .is_some_and(|(bound, local)| bound != local)
        })
    {
        return Err(ImportFailure::Terminal(format!(
            "incoming transfer {transfer_id} has conflicting persisted task identities"
        )));
    }
    let needs_preparation = db
        .transferred_task_manifest_content_commitment(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .is_none();

    if needs_preparation {
        // A payload that promises a resumable session and ships no way to
        // resume it must not be imported: minting a fresh session here is what
        // silently left the conversation behind on the source machine.
        session::assert_importable(
            &transfer_id,
            payload.task.agent_type.as_deref(),
            Some(payload.task.agent_provider.as_str()),
            payload.task.resume_session_id.as_deref(),
            &payload.artifacts,
        )
        .map_err(|missing| ImportFailure::Terminal(missing.0))?;

        // The destination task id — and therefore its worktree — is
        // deterministic before creation, which is what lets the transcript be
        // re-keyed to the destination slug before the agent spawns `--resume`.
        let (repo_id, repo_path, imported_refs) =
            acquire_repo(state, transfer_id, &destination_task_id, &payload).await?;
        #[cfg(test)]
        if serde_json::from_str::<Value>(&work.payload_json)
            .ok()
            .and_then(|value| {
                value
                    .get("interruptAfterAcquisition")
                    .and_then(Value::as_bool)
            })
            == Some(true)
            && db
                .read_transfer_work_observation(&work.id, "test-interrupted-after-acquisition")
                .map_err(|error| format!("db error: {error}"))?
                .is_none()
        {
            db.record_transfer_work_observation(
                &work.id,
                "test-interrupted-after-acquisition",
                Some(&repo_id),
            )
            .map_err(|error| format!("db error: {error}"))?;
            return Err(ImportFailure::Retry(
                "test interruption after durable repository acquisition".into(),
            ));
        }
        let imported_inputs = fetch_task_input_ledger(state, transfer_id, &payload).await?;
        let destination_worktree =
            session::destination_worktree_path(&repo_path, &destination_task_id);
        db.upsert_transferred_task_manifest(
            transfer_id,
            &repo_id,
            Some(&destination_task_id),
            payload.task.head_oid.as_deref().unwrap_or_default(),
            payload.task.base_oid.as_deref().unwrap_or_default(),
        )
        .map_err(|error| {
            ImportFailure::Terminal(format!("transfer manifest admission failed: {error}"))
        })?;
        let resume_session_id =
            materialize_resume_state(state, work, transfer_id, &payload, &destination_worktree)
                .await?;

        let created = crate::http_api::create_transferred_task_in_process(
            Arc::clone(state),
            build_create_request(
                state,
                transfer_id,
                &repo_id,
                &payload,
                imported_refs.clone(),
                resume_session_id.clone(),
            )
            .await,
            destination_task_id.clone(),
            imported_inputs,
            payload.clone(),
        )
        .await
        .map_err(|(status, message)| {
            format!("failed to create the transferred task ({status}): {message}")
        })?;
        local_task_id = Some(created.task_id);
        let expected_head =
            payload.task.head_oid.as_deref().ok_or_else(|| {
                ImportFailure::Terminal("task bundle has no expected head".into())
            })?;
        db.upsert_transferred_task_manifest(
            transfer_id,
            &repo_id,
            local_task_id.as_deref(),
            expected_head,
            payload.task.base_oid.as_deref().unwrap_or_default(),
        )
        .map_err(|error| {
            ImportFailure::Terminal(format!("transfer manifest admission failed: {error}"))
        })?;
        #[cfg(test)]
        if serde_json::from_str::<Value>(&work.payload_json)
            .ok()
            .and_then(|value| value.get("interruptAfterSpawn").and_then(Value::as_bool))
            == Some(true)
            && db
                .read_transfer_work_observation(&work.id, "test-interrupted-after-spawn")
                .map_err(|error| format!("db error: {error}"))?
                .is_none()
        {
            db.record_transfer_work_observation(
                &work.id,
                "test-interrupted-after-spawn",
                local_task_id.as_deref(),
            )
            .map_err(|error| format!("db error: {error}"))?;
            return Err(ImportFailure::Retry(
                "test interruption after verified task spawn".into(),
            ));
        }
        if !db
            .mark_incoming_transfer_importing(
                transfer_id,
                local_task_id.as_deref().unwrap_or_default(),
                ENGINE_CLAIM_TOKEN,
            )
            .map_err(|error| format!("db error: {error}"))?
        {
            return Err(
                format!("failed to claim imported task for transfer: {transfer_id}").into(),
            );
        }
    }

    // A crash after the immutable proof was committed but before the daemon
    // spawn returned leaves the transfer row without local_task_id. Re-enter
    // the existing requested-id repair owner without re-fetching artifacts or
    // revalidating the now-live mutable task snapshot.
    if !needs_preparation && local_task_id.is_none() {
        let (repo_id, head_oid, _, bound_task, state_name) = db
            .transferred_task_manifest(transfer_id)
            .map_err(|error| format!("db error: {error}"))?
            .ok_or_else(|| format!("transfer manifest missing: {transfer_id}"))?;
        if bound_task.as_deref() != Some(destination_task_id.as_str()) || state_name != "prepared" {
            return Err(ImportFailure::Terminal(format!(
                "transfer manifest task binding mismatch: {transfer_id}"
            )));
        }
        let transfer_root = format!("refs/kanna/transfers/{transfer_id}/{head_oid}");
        let created = crate::http_api::create_transferred_task_in_process(
            Arc::clone(state),
            build_create_request(
                state,
                transfer_id,
                &repo_id,
                &payload,
                Some((
                    format!("{transfer_root}/head"),
                    format!("{transfer_root}/base"),
                )),
                payload.task.resume_session_id.clone(),
            )
            .await,
            destination_task_id.clone(),
            Vec::new(),
            payload.clone(),
        )
        .await
        .map_err(|(status, message)| {
            format!("failed to repair the transferred task ({status}): {message}")
        })?;
        local_task_id = Some(created.task_id);
        if !db
            .mark_incoming_transfer_importing(
                transfer_id,
                local_task_id.as_deref().unwrap_or_default(),
                ENGINE_CLAIM_TOKEN,
            )
            .map_err(|error| format!("db error: {error}"))?
        {
            return Err(
                format!("failed to claim repaired task for transfer: {transfer_id}").into(),
            );
        }
    }

    let local_task_id = local_task_id
        .ok_or_else(|| format!("incoming transfer has no local task: {transfer_id}"))?;

    // Once the destination server has durably recorded the complete proof,
    // acknowledgment recovery must not depend on re-fetching artifacts.
    let persisted_commitment = db
        .transferred_task_manifest_content_commitment(transfer_id)
        .map_err(|error| format!("db error: {error}"))?;
    if persisted_commitment.is_none() {
        verify_persisted_task_bundle(state, &payload, &local_task_id, transfer_id, None).await?;
    }
    let destination_repo_id;
    if let Some((repo_id, _, _, bound_task, state_name)) = db
        .transferred_task_manifest(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
    {
        if bound_task.as_deref() != Some(local_task_id.as_str()) {
            return Err(format!("transfer manifest task binding mismatch: {transfer_id}").into());
        }
        if state_name != "prepared" {
            return Err(format!("transfer manifest {transfer_id} is not durably prepared").into());
        }
        destination_repo_id = repo_id;
    } else {
        return Err(format!("transfer manifest missing: {transfer_id}").into());
    }
    // Half A of the acknowledgment's proof (see docs/kanna-server-boundary.md
    // item 3): the content commitment `verify_persisted_task_bundle` computed
    // and persisted just above, from its own read-back state. `None` here
    // would mean the manifest was never actually proven, which the call
    // above already made terminal — so this is an invariant check, not a
    // fallback path.
    let content_commitment = db
        .transferred_task_manifest_content_commitment(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| {
            format!("transfer manifest for {transfer_id} has no proven content commitment")
        })?;

    db.set_cloud_task_identity(&local_task_id, &payload.task.cloud_task_id)
        .map_err(|error| format!("db error: {error}"))?;
    db.insert_task_transfer_provenance(&crate::db::NewTaskTransferProvenance {
        pipeline_item_id: local_task_id.clone(),
        source_peer_id: payload.task.source_peer_id.clone(),
        source_task_id: payload.task.source_task_id.clone(),
        source_machine_task_label: payload.task.branch.clone(),
    })
    .map_err(|error| format!("db error: {error}"))?;
    let marked = db
        .mark_incoming_transfer_awaiting_acknowledgment(
            transfer_id,
            &local_task_id,
            ENGINE_CLAIM_TOKEN,
        )
        .map_err(|error| format!("db error: {error}"))?;
    let already_awaiting = db
        .get_task_transfer(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .is_some_and(|transfer| {
            transfer.local_task_id.as_deref() == Some(local_task_id.as_str())
                && transfer.status == "awaiting_acknowledgment"
        });
    if !marked && !already_awaiting {
        return Err(format!(
            "failed to mark incoming transfer awaiting acknowledgment: {transfer_id}"
        )
        .into());
    }
    state.publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);

    // The sidecar's matching receipt is idempotent. Always replay the network
    // effect: a claim taken before it could turn a crash into local success
    // while the source task remained open.
    control::acknowledge_import_committed(
        state,
        transfer_id,
        &payload.task.source_task_id,
        &local_task_id,
        &content_commitment,
        &destination_repo_id,
    )
    .await
    .map_err(ImportFailure::from)?;
    if !db
        .mark_task_transfer_completed(transfer_id, &local_task_id, Some(ENGINE_CLAIM_TOKEN))
        .map_err(|error| format!("db error: {error}"))?
    {
        return Err(
            format!("failed to complete acknowledged incoming transfer: {transfer_id}").into(),
        );
    }
    release_incoming_reservation(state, transfer_id).await?;
    Ok(())
}

pub(crate) async fn verify_persisted_task_bundle(
    state: &Arc<AppState>,
    payload: &OutgoingTransferPayload,
    local_task_id: &str,
    transfer_id: &str,
    imported_inputs: Option<&[crate::db::ImportedTaskInput]>,
) -> Result<(), ImportFailure> {
    let db = state.transfer_work().open_db()?;
    let item = db
        .get_pipeline_item(local_task_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| {
            ImportFailure::Terminal(format!(
                "transferred task {local_task_id} disappeared before integrity verification"
            ))
        })?;
    let manifest = db
        .transferred_task_manifest(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| {
            ImportFailure::Terminal(format!(
                "transferred task {local_task_id} has no acquisition manifest"
            ))
        })?;
    if manifest.0 != item.repo_id
        || manifest.3.as_deref() != Some(local_task_id)
        || !matches!(manifest.4.as_str(), "importing" | "prepared")
    {
        return Err(ImportFailure::Terminal(format!(
            "transferred task {local_task_id} does not match its acquisition manifest"
        )));
    }
    if item.stage.as_deref() != Some(payload.task.stage.as_str()) {
        let imported_stage_ran = db
            .list_stage_runs_for_task(local_task_id)
            .map_err(|error| format!("db error: {error}"))?
            .iter()
            .any(|run| run.stage == payload.task.stage);
        if !imported_stage_ran {
            return Err(ImportFailure::Terminal(format!(
                "transferred task {local_task_id} stage does not match the source context"
            )));
        }
    }
    let workflow_matches = match (
        item.pipeline_def.as_deref(),
        payload.task.workflow_definition.as_deref(),
    ) {
        (Some(stored), Some(expected)) if stored == expected => true,
        (Some(stored), Some(expected)) => serde_json::from_str::<serde_json::Value>(stored)
            .ok()
            .zip(serde_json::from_str::<serde_json::Value>(expected).ok())
            .is_some_and(|(a, b)| a == b),
        (None, None) => true,
        _ => false,
    };
    if !workflow_matches {
        return Err(ImportFailure::Terminal(format!(
            "transferred task {local_task_id} workflow definition does not match the source snapshot"
        )));
    }
    let repo = db
        .get_repo(&item.repo_id)
        .map_err(|error| format!("db error: {error}"))?
        .ok_or_else(|| {
            ImportFailure::Terminal(format!(
                "transferred task {local_task_id} repository disappeared before verification"
            ))
        })?;
    let expected_head = payload
        .task
        .head_oid
        .as_deref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no expected head".into()))?;
    let expected_base =
        payload.task.base_oid.as_deref().ok_or_else(|| {
            ImportFailure::Terminal("task bundle has no expected review base".into())
        })?;
    if manifest.1 != expected_head || manifest.2 != expected_base {
        return Err(ImportFailure::Terminal(format!(
            "transferred task {local_task_id} manifest OIDs do not match the source snapshot"
        )));
    }
    let expected_head_owned = expected_head.to_string();
    let transfer_id_owned = transfer_id.to_string();
    let (repo_path, task_id, branch, base_ref, db_path) = (
        repo.path.clone(),
        local_task_id.to_string(),
        item.branch.clone(),
        item.base_ref.clone(),
        state.config().db_path.clone(),
    );
    let (actual_head, actual_base) =
        super::run_blocking("transferred task persisted ref verification", move || {
            let verify_db =
                crate::db::Db::open(&db_path).map_err(|error| format!("db error: {error}"))?;
            let tip = crate::task_creator::task_work_tip_for_transfer(
                &verify_db,
                &repo_path,
                &task_id,
                branch.as_deref(),
            )?;
            let base = base_ref
                .as_deref()
                .ok_or_else(|| "transferred task has no persisted review base ref".to_string())?;
            let base_oid = super::git::commit_oid(std::path::Path::new(&repo_path), base)?;
            if !super::git::commit_is_ancestor(
                std::path::Path::new(&repo_path),
                &expected_head_owned,
                &tip.branch,
            )? {
                return Err(format!(
                    "destination task branch {} does not contain transferred head {}",
                    tip.branch, expected_head_owned
                ));
            }
            let private_head =
                format!("refs/kanna/transfers/{transfer_id_owned}/{expected_head_owned}/head");
            let imported_head =
                super::git::commit_oid(std::path::Path::new(&repo_path), &private_head)?;
            Ok::<_, String>((imported_head, base_oid))
        })
        .await?;
    if actual_head != expected_head || actual_base != expected_base {
        return Err(ImportFailure::Terminal(format!(
            "transferred task {local_task_id} persisted refs do not match the source manifest"
        )));
    }

    drop(db);
    let fetched_inputs;
    let imported = if let Some(imported) = imported_inputs {
        imported
    } else {
        fetched_inputs = fetch_task_input_ledger(state, transfer_id, payload).await?;
        &fetched_inputs
    };
    let verify_db = state.transfer_work().open_db()?;
    let existing = verify_db
        .list_all_task_inputs(local_task_id)
        .map_err(|error| format!("db error: {error}"))?;
    let imported_existing: Vec<_> = existing.iter().filter(|row| row.origin.is_some()).collect();
    if imported_existing.len() != imported.len()
        || imported_existing
            .iter()
            .zip(imported.iter())
            .any(|(row, expected)| {
                row.stage != expected.stage
                    || row.source != expected.source
                    || row.message != expected.message
                    || row.delivered_at != expected.delivered_at
                    || row.origin.as_ref() != Some(&expected.origin)
            })
    {
        return Err(ImportFailure::Terminal(format!(
            "transferred task {local_task_id} durable input history does not match the source ledger"
        )));
    }

    // The ordered foreign stage/main/post/revision history a second hop would
    // need to re-export honestly — proved here for the same reason the input
    // ledger is: without it, the sole-authorization acknowledgment below
    // would silently drop this guarantee out of the proved contract.
    let db_history = verify_db
        .transferred_task_history(local_task_id)
        .map_err(|error| format!("db error: {error}"))?;
    let expected_history = &payload.task.history;
    if db_history.len() != expected_history.len()
        || db_history
            .iter()
            .zip(expected_history.iter())
            .any(|(row, expected)| {
                row.origin_peer_id != expected.origin_peer_id
                    || row.origin_task_id != expected.origin_task_id
                    || row.origin_run_id != expected.origin_run_id
                    || row.stage != expected.stage
                    || row.kind != expected.kind
                    || row.result != expected.result
                    || row.feedback != expected.feedback
                    || row.finished_at != expected.finished_at
            })
    {
        return Err(ImportFailure::Terminal(format!(
            "transferred task {local_task_id} durable foreign history does not match the source snapshot"
        )));
    }

    // The proof `outgoing_committed` requires before it will ever close the
    // source task, computed ONLY from what was just read back above — never
    // from `payload`'s own copy of these values — so a destination that
    // imported nothing has nothing here it could echo. See
    // docs/kanna-server-boundary.md item 3.
    let read_back_history: Vec<TransferHistoryRecordPayload> = db_history
        .iter()
        .map(|record| TransferHistoryRecordPayload {
            sequence: u64::try_from(record.sequence).unwrap_or_default(),
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
    let content_commitment =
        payload::transfer_content_commitment(&payload::TransferContentCommitmentInput {
            transfer_id,
            cloud_task_id: &payload.task.cloud_task_id,
            head_oid: &actual_head,
            base_oid: &actual_base,
            stage: &payload.task.stage,
            workflow_definition: item.pipeline_def.as_deref(),
            // The artifact checksum already verified byte-for-byte against the
            // fetched ledger above (`decode_task_input_ledger`'s own sha256
            // check) — reusing it here needs no retained ledger bytes on the
            // source and is not an echo of anything unverified.
            input_ledger_sha256: payload
                .input_ledger
                .as_ref()
                .map(|ledger| ledger.sha256.as_str()),
            history: &read_back_history,
        })
        .map_err(ImportFailure::Terminal)?;
    let completed = verify_db
        .complete_transferred_task_manifest_preparation(transfer_id, &content_commitment)
        .map_err(|error| format!("db error: {error}"))?;
    if !completed {
        let persisted = verify_db
            .transferred_task_manifest_content_commitment(transfer_id)
            .map_err(|error| format!("db error: {error}"))?;
        if persisted.as_deref() != Some(content_commitment.as_str()) {
            return Err(ImportFailure::Terminal(format!(
                "transfer manifest {transfer_id} could not record its prepared proof"
            )));
        }
    }
    Ok(())
}

fn assert_payload_matches_reservation(
    transfer: &crate::db::TaskTransfer,
    payload: &OutgoingTransferPayload,
) -> Result<(), ImportFailure> {
    let matches = transfer.source_peer_id.as_deref() == Some(payload.task.source_peer_id.as_str())
        && transfer.source_task_id.as_deref() == Some(payload.task.source_task_id.as_str());
    if matches {
        return Ok(());
    }
    // A payload whose identity does not match the reservation it arrived under
    // is not a transient failure — it is a different transfer.
    Err(ImportFailure::Terminal(format!(
        "incoming transfer payload source identity does not match reservation: {}",
        transfer.id
    )))
}

// ---------------------------------------------------------------------------
// Repository acquisition
// ---------------------------------------------------------------------------

async fn acquire_repo(
    state: &Arc<AppState>,
    transfer_id: &str,
    destination_task_id: &str,
    payload: &OutgoingTransferPayload,
) -> Result<(String, PathBuf, Option<(String, String)>), ImportFailure> {
    let repo_name = payload.repo.name.clone().unwrap_or_else(|| "repo".into());
    let default_branch = payload
        .repo
        .default_branch
        .clone()
        .unwrap_or_else(|| "main".into());
    let expected_head = payload
        .task
        .head_oid
        .as_deref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no expected head".into()))?;
    let expected_base =
        payload.task.base_oid.as_deref().ok_or_else(|| {
            ImportFailure::Terminal("task bundle has no expected review base".into())
        })?;

    // Repository acquisition is durable transfer state, not a fresh search on
    // every attempt. Once chosen, retries must reuse this exact repository or
    // fail closed; otherwise a no-repo destination allocates one empty clone
    // per crash and can never reconcile the deterministic task id.
    if let Some((repo_id, head_oid, base_oid, bound_task, _)) = state
        .transfer_work()
        .open_db()?
        .transferred_task_manifest(transfer_id)
        .map_err(|error| format!("db error: {error}"))?
    {
        if head_oid != expected_head
            || base_oid != expected_base
            || bound_task.as_deref() != Some(destination_task_id)
        {
            return Err(ImportFailure::Terminal(format!(
                "transfer manifest acquisition binding mismatch: {transfer_id}"
            )));
        }
        let repo = state
            .transfer_work()
            .open_db()?
            .get_repo(&repo_id)
            .map_err(|error| format!("db error: {error}"))?
            .ok_or_else(|| {
                ImportFailure::Terminal(format!(
                    "transfer manifest repository disappeared: {repo_id}"
                ))
            })?;
        let repo_path = PathBuf::from(repo.path);
        let imported_ref =
            Some(import_verified_task_bundle(state, transfer_id, payload, &repo_path).await?);
        return Ok((repo_id, repo_path, imported_ref));
    }

    // A pre-manifest crash from an older destination may already have made
    // the deterministic task durable. Its repository is then the acquisition
    // identity; searching or allocating again would make repair impossible.
    if let Some(item) = state
        .transfer_work()
        .open_db()?
        .get_pipeline_item(destination_task_id)
        .map_err(|error| format!("db error: {error}"))?
    {
        let repo = state
            .transfer_work()
            .open_db()?
            .get_repo(&item.repo_id)
            .map_err(|error| format!("db error: {error}"))?
            .ok_or_else(|| {
                ImportFailure::Terminal(format!(
                    "existing transferred task repository disappeared: {}",
                    item.repo_id
                ))
            })?;
        state
            .transfer_work()
            .open_db()?
            .upsert_transferred_task_manifest(
                transfer_id,
                &item.repo_id,
                Some(destination_task_id),
                expected_head,
                expected_base,
            )
            .map_err(|error| {
                ImportFailure::Terminal(format!("transfer manifest admission failed: {error}"))
            })?;
        let repo_path = PathBuf::from(repo.path);
        let imported_ref =
            Some(import_verified_task_bundle(state, transfer_id, payload, &repo_path).await?);
        return Ok((item.repo_id, repo_path, imported_ref));
    }

    // Runs `git remote get-url` once per registered repo, so it is blocking
    // work proportional to how many repos this machine has.
    let matched = {
        let queue = state.transfer_work();
        let remote_url = payload.repo.remote_url.clone();
        let payload_path = payload.repo.path.clone();
        super::run_blocking("transfer repo match", move || {
            find_matching_repo(
                &queue.open_db()?,
                remote_url.as_deref(),
                payload_path.as_deref(),
            )
        })
        .await?
    };
    if let Some((repo_id, repo_path)) = matched {
        let repo_path = PathBuf::from(repo_path);
        state
            .transfer_work()
            .open_db()?
            .upsert_transferred_task_manifest(
                transfer_id,
                &repo_id,
                Some(destination_task_id),
                expected_head,
                expected_base,
            )
            .map_err(|error| {
                ImportFailure::Terminal(format!("transfer manifest admission failed: {error}"))
            })?;
        let imported_ref = if payload.repo.mode == RepoAcquisitionMode::TaskBundle {
            Some(import_verified_task_bundle(state, transfer_id, payload, &repo_path).await?)
        } else {
            None
        };
        return Ok((repo_id, repo_path, imported_ref));
    }

    let repo_path = match payload.repo.mode {
        RepoAcquisitionMode::ReuseLocal => {
            let repo_path = payload
                .repo
                .path
                .clone()
                .ok_or_else(|| "incoming transfer payload is missing a local repo path".to_string())
                .map_err(ImportFailure::Terminal)?;
            if !Path::new(&repo_path).exists() {
                return Err(ImportFailure::Terminal(format!(
                    "incoming transfer repo path does not exist: {repo_path}"
                )));
            }
            PathBuf::from(repo_path)
        }
        RepoAcquisitionMode::CloneRemote => {
            let remote_url = payload
                .repo
                .remote_url
                .clone()
                .ok_or_else(|| "incoming transfer payload is missing a remote URL".to_string())
                .map_err(ImportFailure::Terminal)?;
            let repo_name = repo_name.clone();
            super::run_blocking("transfer repo clone", move || {
                let repo_path = super::git::allocate_repo_path(&repos_home()?, &repo_name)?;
                super::git::clone_remote(&remote_url, &repo_path)?;
                Ok(repo_path)
            })
            .await?
        }
        RepoAcquisitionMode::BundleRepo => {
            let bundle = payload
                .repo
                .bundle
                .as_ref()
                .ok_or_else(|| "incoming transfer payload is missing bundle metadata".to_string())
                .map_err(ImportFailure::Terminal)?;
            let fetched = control::fetch_artifact(state, transfer_id, &bundle.artifact_id).await?;
            let checkout_ref = bundle
                .ref_name
                .clone()
                .or_else(|| payload.task.branch.clone())
                .or_else(|| payload.task.base_ref.clone());
            let repo_name = repo_name.clone();
            super::run_blocking("transfer repo restore", move || {
                let repo_path = super::git::allocate_repo_path(&repos_home()?, &repo_name)?;
                super::git::init_from_bundle(&repo_path, &fetched, checkout_ref.as_deref())?;
                Ok(repo_path)
            })
            .await?
        }
        RepoAcquisitionMode::TaskBundle => {
            let repo_name = repo_name.clone();
            let remote_url = payload.repo.remote_url.clone();
            if remote_url
                .as_deref()
                .is_some_and(|url| !super::git::is_credential_free_clone_source(url))
            {
                return Err(ImportFailure::Terminal(
                    "incoming transfer remote URL contains credentials or signed parameters".into(),
                ));
            }
            super::run_blocking("transfer repo restore", move || {
                let repo_path = super::git::allocate_repo_path(&repos_home()?, &repo_name)?;
                super::git::init_empty_repo(&repo_path)?;
                if let Some(remote_url) = remote_url.as_deref() {
                    super::git::add_origin(&repo_path, remote_url)?;
                }
                Ok(repo_path)
            })
            .await?
        }
    };

    // `add_repo` canonicalizes the path and reads the repo's default branch
    // with git before it writes the row.
    let repo_id = {
        let (state, path, name, branch) = (
            Arc::clone(state),
            repo_path.clone(),
            repo_name.clone(),
            default_branch.clone(),
        );
        super::run_blocking("transfer repo register", move || {
            register_repo(&state, &path, &name, &branch)
        })
        .await?
    };
    state
        .transfer_work()
        .open_db()?
        .upsert_transferred_task_manifest(
            transfer_id,
            &repo_id,
            Some(destination_task_id),
            expected_head,
            expected_base,
        )
        .map_err(|error| {
            ImportFailure::Terminal(format!("transfer manifest admission failed: {error}"))
        })?;
    let imported_ref = if payload.repo.mode == RepoAcquisitionMode::TaskBundle {
        Some(import_verified_task_bundle(state, transfer_id, payload, &repo_path).await?)
    } else {
        None
    };
    Ok((repo_id, repo_path, imported_ref))
}

async fn import_verified_task_bundle(
    state: &Arc<AppState>,
    transfer_id: &str,
    payload: &OutgoingTransferPayload,
    repo_path: &Path,
) -> Result<(String, String), ImportFailure> {
    let bundle = payload
        .repo
        .bundle
        .as_ref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no bundle metadata".into()))?;
    let source_ref = bundle
        .ref_name
        .as_deref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no source ref".into()))?;
    let expected_head = payload
        .task
        .head_oid
        .as_deref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no expected head".into()))?;
    let source_base_ref = bundle
        .base_ref_name
        .as_deref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no immutable base ref".into()))?;
    let expected_base =
        payload.task.base_oid.as_deref().ok_or_else(|| {
            ImportFailure::Terminal("task bundle has no expected review base".into())
        })?;
    let fetched = control::fetch_artifact(state, transfer_id, &bundle.artifact_id).await?;
    let (repo_path, source_ref, expected_head, source_base_ref, expected_base) = (
        repo_path.to_path_buf(),
        source_ref.to_string(),
        expected_head.to_string(),
        source_base_ref.to_string(),
        expected_base.to_string(),
    );
    let transfer_id = transfer_id.to_string();
    super::run_blocking("transfer task bundle import", move || {
        super::git::import_task_bundle_refs(
            &repo_path,
            &fetched,
            &transfer_id,
            &source_ref,
            &expected_head,
            &source_base_ref,
            &expected_base,
        )
    })
    .await
    .map_err(ImportFailure::Terminal)
}

async fn fetch_task_input_ledger(
    state: &Arc<AppState>,
    transfer_id: &str,
    payload: &OutgoingTransferPayload,
) -> Result<Vec<crate::db::ImportedTaskInput>, ImportFailure> {
    let metadata = payload
        .input_ledger
        .as_ref()
        .ok_or_else(|| ImportFailure::Terminal("task bundle has no durable input ledger".into()))?;
    let fetched = control::fetch_artifact(state, transfer_id, &metadata.artifact_id).await?;
    let bytes = super::run_blocking("transfer input ledger read", move || {
        std::fs::read(&fetched)
            .map_err(|error| format!("failed to read transferred task input ledger: {error}"))
    })
    .await?;
    payload::decode_task_input_ledger(
        &bytes,
        metadata,
        &payload.task.source_peer_id,
        &payload.task.source_task_id,
    )
    .map_err(ImportFailure::Terminal)
}

/// Matches the payload's repository against one this machine already has —
/// first by remote URL, then by the source's own path in case both machines
/// check the repo out to the same place.
fn find_matching_repo(
    db: &crate::db::Db,
    remote_url: Option<&str>,
    payload_path: Option<&str>,
) -> Result<Option<(String, String)>, String> {
    let normalized_remote = remote_url.map(str::trim).filter(|url| !url.is_empty());
    let repos = db
        .list_repos_for_maintenance()
        .map_err(|error| format!("db error: {error}"))?;
    if let Some(remote) = normalized_remote {
        for repo in &repos {
            if super::git::remote_url(Path::new(&repo.path)).as_deref() == Some(remote) {
                return Ok(Some((repo.id.clone(), repo.path.clone())));
            }
        }
    }
    if let Some(path) = payload_path {
        if let Some(repo) = repos.iter().find(|repo| repo.path == path) {
            return Ok(Some((repo.id.clone(), repo.path.clone())));
        }
    }
    Ok(None)
}

fn register_repo(
    state: &Arc<AppState>,
    repo_path: &Path,
    repo_name: &str,
    default_branch: &str,
) -> Result<String, String> {
    let db = state.transfer_work().open_db()?;
    let path = repo_path.to_string_lossy().to_string();
    let api = crate::mobile_api::MobileApi::new(state.config().clone(), db);
    match api.add_repo(crate::mobile_api::AddRepoRequest {
        path: path.clone(),
        name: Some(repo_name.to_string()),
        default_branch: Some(default_branch.to_string()),
    }) {
        Ok(repo) => Ok(repo.id),
        Err(crate::mobile_api::AddRepoError::DuplicatePath) => {
            let db = state.transfer_work().open_db()?;
            db.get_snapshot_repo_by_path(&path)
                .map_err(|error| format!("db error: {error}"))?
                .map(|repo| repo.id)
                .ok_or_else(|| format!("repo {path} is registered but could not be read back"))
        }
        Err(error) => Err(error.message()),
    }
}

fn repos_home() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".kanna").join("repos"))
}

// ---------------------------------------------------------------------------
// Resume state
// ---------------------------------------------------------------------------

/// Materializes the session artifacts the payload carries, returning the
/// session id the destination agent will resume — or `None` when the resume
/// must be abandoned because the conversation state could not be established.
/// The phase under which an import records what it materialized.
///
/// Materialization is not re-observable: once a transcript is on disk or an
/// OpenCode session is installed, attempt 2 sees an occupied destination and
/// would read it as "someone else was here" — abandoning the conversation this
/// transfer already imported, and acking the source anyway. Recording the
/// answer once is what makes a retry recognise its own success.
const MATERIALIZE_PHASE: &str = "materialize-resume-state";

/// A recorded materialization that resolved to "no resume".
///
/// The observation column stores `Option<String>`, and `None` there already
/// means "never observed" — so the abandoned-resume decision needs a value of
/// its own rather than an absent one.
const RESUME_ABANDONED: &str = "";

async fn materialize_resume_state(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    transfer_id: &str,
    payload: &OutgoingTransferPayload,
    destination_worktree: &Path,
) -> Result<Option<String>, ImportFailure> {
    // What an earlier attempt already decided. Reusing it is the whole point:
    // the destinations it wrote are exactly what a fresh look would now
    // misread.
    if let Some(recorded) = state
        .transfer_work()
        .open_db()?
        .read_transfer_work_observation(&work.id, MATERIALIZE_PHASE)
        .map_err(|error| format!("db error: {error}"))?
    {
        return Ok(recorded.filter(|session_id| session_id != RESUME_ABANDONED));
    }

    let Some(resume_session_id) = payload.task.resume_session_id.clone() else {
        return Ok(None);
    };
    let provider = payload.task.agent_provider.as_str();
    let artifacts: Vec<_> = payload
        .artifacts
        .iter()
        .filter(|artifact| artifact.provider == provider)
        .collect();
    if artifacts.is_empty() {
        return Ok(None);
    }

    let home = home_dir()?;
    let mut materialized = Vec::with_capacity(artifacts.len());
    for artifact in &artifacts {
        let source_path =
            control::fetch_artifact(state, transfer_id, &artifact.artifact_id).await?;
        // OpenCode keeps its conversations in a shared SQLite store that only
        // its own CLI may write, so this artifact never reaches the filesystem
        // fence. The import runs in the destination worktree because that is
        // what re-keys the session to this machine's path — without it
        // `opencode run --session <id>` is a silent no-op.
        if artifact.materialization == payload::TransferArtifactMaterialization::OpencodeImport {
            let (session_id, worktree) = (
                resume_session_id.clone(),
                destination_worktree.to_path_buf(),
            );
            // Read now rather than earlier: the guard is about what this
            // operator is using at the moment of the import, and an import that
            // retries minutes later must see the tasks that are open then.
            let live_worktrees = super::git::LiveLocalWorktrees::new(
                state
                    .transfer_work()
                    .open_db()?
                    .list_open_task_worktree_paths()
                    .map_err(|error| format!("db error: {error}"))?
                    .into_iter()
                    .map(|(_, path)| PathBuf::from(path)),
            );
            let imported = super::run_blocking("transfer opencode import", move || {
                Ok(super::git::import_opencode_session(
                    &source_path,
                    &session_id,
                    &worktree,
                    &live_worktrees,
                ))
            })
            .await?;
            match imported {
                Ok(()) => materialized.push((artifact.artifact_id.clone(), true)),
                // The receiver already owns this id. Reported like every other
                // occupied destination — `wrote = false`, which abandons the
                // resume rather than overwriting a conversation of the
                // operator's own.
                Err(super::git::OpencodeImportError::DestinationExists(session_id)) => {
                    log::warn!(
                        "skipping the transferred OpenCode session: {session_id} already exists here"
                    );
                    materialized.push((artifact.artifact_id.clone(), false));
                }
                // A payload this machine will refuse every time it looks.
                Err(refused @ super::git::OpencodeImportError::Refused(_)) => {
                    return Err(ImportFailure::Terminal(refused.to_string()));
                }
                // The CLI, not the payload — OpenCode's store is one shared
                // SQLite file that many agents write, and `import` exits
                // non-zero while another holds the write lock. Retrying is what
                // keeps a lock that clears in seconds from permanently losing a
                // conversation the source has already been shut down to hand over.
                Err(unavailable @ super::git::OpencodeImportError::Unavailable(_)) => {
                    return Err(ImportFailure::Retry(unavailable.to_string()));
                }
            }
            continue;
        }
        // Extracting a gzipped session archive is unbounded blocking work, and
        // it runs against the operator's home directory — the one place the
        // engine must never stall the runtime that is also serving terminals.
        let wrote = {
            let (home, provider, resume_session_id, filename, kind, materialization, worktree) = (
                home.clone(),
                provider.to_string(),
                resume_session_id.clone(),
                artifact.filename.clone(),
                artifact.kind.as_str().to_string(),
                artifact.materialization.as_str().to_string(),
                (artifact.kind == payload::TransferArtifactKind::SessionTranscript)
                    .then(|| destination_worktree.to_path_buf()),
            );
            super::run_blocking("transfer artifact materialization", move || {
                crate::transfer_artifact::materialize_transfer_artifact_at_home(
                    &home,
                    &source_path,
                    crate::transfer_artifact::TransferArtifactContract {
                        provider: &provider,
                        resume_session_id: &resume_session_id,
                        filename: &filename,
                        kind: &kind,
                        materialization: &materialization,
                        // A Claude transcript is cwd-keyed, so only the receiver
                        // can name where it lands. The sender never supplies a
                        // destination.
                        destination_worktree_path: worktree.as_deref(),
                    },
                )
            })
            .await
            // A contract violation, or a worker that panicked doing this — a
            // payload this machine cannot safely materialize is not something a
            // retry fixes.
            .map_err(ImportFailure::Terminal)?
        };
        materialized.push((artifact.artifact_id.clone(), wrote));
    }

    let owned: Vec<_> = artifacts.into_iter().cloned().collect();
    let resolved = if session::resume_survives_existing_destination(&owned, &materialized) {
        Some(resume_session_id)
    } else {
        log::warn!(
            "skipping transferred session import for {provider} session {resume_session_id}: \
             the provider destination already exists"
        );
        None
    };

    // Recorded before the task is created, so the answer a retry reads is the
    // one this attempt acted on. A crash between the two leaves the record and
    // the materialized state agreeing, which is what the retry needs.
    let recorded = state
        .transfer_work()
        .open_db()?
        .record_transfer_work_observation(
            &work.id,
            MATERIALIZE_PHASE,
            Some(resolved.as_deref().unwrap_or(RESUME_ABANDONED)),
        )
        .map_err(|error| format!("db error: {error}"))?;
    Ok(recorded.filter(|session_id| session_id != RESUME_ABANDONED))
}

async fn build_create_request(
    state: &Arc<AppState>,
    transfer_id: &str,
    repo_id: &str,
    payload: &OutgoingTransferPayload,
    imported_refs: Option<(String, String)>,
    resume_session_id: Option<String>,
) -> crate::mobile_api::CreateTaskRequest {
    crate::mobile_api::CreateTaskRequest {
        repo_id: repo_id.to_string(),
        prompt: payload.task.prompt.clone().unwrap_or_default(),
        display_name: payload.task.display_name.clone(),
        workflow_name: Some(payload.task.workflow.clone()),
        stage: Some(payload.task.stage.clone()),
        // Integrity-aware transfers fork from the private ref whose object id
        // was just proved. Legacy payloads retain their historical resolver.
        base_ref: imported_refs
            .as_ref()
            .map(|(head, _base)| head.clone())
            .or_else(|| payload::resolve_incoming_base_branch(payload)),
        // The source task's diff base is distinct from its fork point once the
        // exact transferred head is available locally.
        diff_base_ref: imported_refs
            .map(|(_head, base)| base)
            .or_else(|| payload.task.base_ref.clone()),
        review_context: None,
        agent: None,
        agent_provider: Some(payload.task.agent_provider.clone()),
        agent_type: Some(
            match payload.task.agent_type.as_deref() {
                Some("agent") | Some("sdk") => "agent",
                _ => "pty",
            }
            .to_string(),
        ),
        terminal_cols: None,
        terminal_rows: None,
        model: None,
        effort: None,
        permission_mode: None,
        allowed_tools: None,
        disallowed_tools: None,
        max_turns: None,
        max_budget_usd: None,
        setup_cmds: None,
        task_template: None,
        transfer_import: Some(crate::mobile_api::TransferImportSummary {
            head_oid: payload.task.head_oid.clone(),
            transfer_id: Some(transfer_id.to_string()),
            source_machine: resolve_source_machine_name(state, &payload.task.source_peer_id).await,
            repo_mode: Some(payload.repo.mode.as_str().to_string()),
            session_restored: resume_session_id.is_some(),
            workflow_definition: payload.task.workflow_definition.clone(),
            previous_stage_result: payload.task.previous_stage_result.clone(),
            previous_main_result: payload.task.previous_main_result.clone(),
            revision_feedback: payload.task.revision_feedback.clone(),
            history: payload
                .task
                .history
                .iter()
                .map(
                    |record| crate::mobile_api::TransferredHistoryRecordSummary {
                        sequence: record.sequence,
                        origin_peer_id: record.origin_peer_id.clone(),
                        origin_task_id: record.origin_task_id.clone(),
                        origin_run_id: record.origin_run_id.clone(),
                        stage: record.stage.clone(),
                        kind: record.kind.clone(),
                        agent: record.agent.clone(),
                        result: record.result.clone(),
                        feedback: record.feedback.clone(),
                        finished_at: record.finished_at.clone(),
                    },
                )
                .collect(),
        }),
        resume_session_id,
        recovery_snapshot: payload.recovery.clone(),
        blocker_task_ids: None,
        notify_task_id: None,
        parent_task_id: None,
    }
}

/// Peer display names live in the sidecar's registry, not in the payload.
/// Resolving one is best effort: an unreachable sidecar or an unknown peer
/// falls back to the peer id, which still identifies the machine.
async fn resolve_source_machine_name(state: &Arc<AppState>, peer_id: &str) -> Option<String> {
    let peers = state
        .transfer_sidecar()
        .control("list-peers", serde_json::json!({}))
        .await
        .ok()?;
    let name = peers.as_array()?.iter().find_map(|peer| {
        let matches = peer.get("peer_id").and_then(Value::as_str) == Some(peer_id);
        matches
            .then(|| peer.get("display_name").and_then(Value::as_str))
            .flatten()
            .map(str::to_string)
    });
    Some(name.unwrap_or_else(|| peer_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_item(id: &str) -> TransferWorkItem {
        TransferWorkItem {
            id: id.to_string(),
            kind: super::super::queue::KIND_IMPORT.to_string(),
            transfer_id: Some("transfer-1".to_string()),
            payload_json: "{}".to_string(),
            attempts: 2,
        }
    }

    /// The whole server-side chain of a refused pull, from the JSON the
    /// requester's sidecar emits to what the operator can finally see.
    ///
    /// `kanna-server` does not depend on the task-transfer crate, so this event
    /// shape is a contract pinned on both sides — the sender's half is
    /// `crates/task-transfer/tests/protocol.rs`. What is proved here is the
    /// half the 2026-09-08 report was missing: the machine that asked for the
    /// task ends up with a durable record and something to show for it, rather
    /// than a 404 and an empty window.
    #[tokio::test]
    async fn a_refused_pull_becomes_a_durable_record_and_a_snapshot_alert() {
        let state = crate::http_api::test_state_with_seed(
            "desktop-refused-pull-chain",
            "Studio Mac",
            |_| {},
        );
        let event = serde_json::json!({
            "type": "task_pull_refused",
            "request_id": "pull-peer-mbp-3",
            "source_peer_id": "peer-mbp",
            "source_task_id": "afed27d1",
            "reason": "task afed27d1 resumes codex session 5a2eb492 but its rollout could not \
                       be found under ~/.codex/sessions",
        });

        // The sidecar reader routes it to durable work rather than to the
        // window's advisory log, because it changes state here.
        assert!(super::super::queue::is_durable_transfer_event(
            "task_pull_refused"
        ));
        let work = super::super::queue::durable_event_work(&event, "sidecar-a")
            .expect("a refusal schedules work");
        assert_eq!(work.kind, super::super::queue::KIND_PULL_REFUSED);

        record_pull_refusal(&state, &event).await.expect("record");
        // A redelivery of the same event lands on the same row.
        record_pull_refusal(&state, &event)
            .await
            .expect("redeliver");

        let db = state.transfer_work().open_db().expect("db");
        let recorded = db.list_task_transfers("afed27d1").expect("list");
        assert_eq!(recorded.len(), 1, "a redelivery must not pile up rows");
        assert_eq!(recorded[0].direction, "incoming");
        assert_eq!(recorded[0].status, "failed");
        assert_eq!(recorded[0].local_task_id, None);
        assert!(recorded[0]
            .error
            .as_deref()
            .is_some_and(|reason| reason.contains("rollout could not be found")));
        // Terminal, not merely recorded: a row with no `completed_at` reads as
        // a transfer still in progress.
        assert!(recorded[0].completed_at.is_some());

        // …and it reaches the window, which has no task here to hang it on.
        let alerts = db.ui_snapshot().expect("snapshot").transfer_alerts;
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].source_task_id.as_deref(), Some("afed27d1"));
        assert_eq!(alerts[0].source_peer_id.as_deref(), Some("peer-mbp"));
    }

    /// A payload whose recompute path would answer `None`, so the only way a
    /// recorded answer can come back is if the memo is actually consulted.
    fn payload_promising(session_id: Option<&str>) -> OutgoingTransferPayload {
        payload::parse_outgoing_transfer_payload(&serde_json::json!({
            "target_peer_id": "peer-destination",
            "task": {
                "source_peer_id": "peer-source",
                "source_task_id": "task-source",
                "resume_session_id": session_id,
                "stage": "in progress",
                "pipeline": "single-reviewer",
                "agent_type": "pty",
                "agent_provider": "claude",
            },
            // No artifacts, so a second look at this payload resolves to "no
            // resume" — which is exactly what must *not* be returned once an
            // earlier attempt has already materialized one.
            "repo": { "mode": "reuse-local", "path": "/repo" },
            "artifacts": [],
        }))
        .expect("a valid payload")
    }

    #[tokio::test]
    async fn legacy_payload_is_a_recoverable_failure_before_source_finalization() {
        let work = work_item("import:legacy-transfer");
        let raw_payload = serde_json::json!({
            "target_peer_id": "peer-destination",
            "task": {
                "source_peer_id": "peer-source",
                "source_task_id": "task-source",
                "resume_session_id": null,
                "stage": "review",
                "pipeline": "single-reviewer",
                "branch": "task-source",
                "base_ref": "origin/main",
                "agent_type": "agent",
                "agent_provider": "codex",
            },
            "repo": { "mode": "reuse-local", "path": "/repo" },
            "artifacts": [],
        });
        let payload_json = serde_json::to_string(&raw_payload).expect("payload json");
        let state = crate::http_api::test_state_with_seed(
            "desktop-legacy-transfer-refusal",
            "Legacy Refusal",
            |db| {
                db.insert_task_transfer(&crate::db::NewTaskTransfer {
                    id: "legacy-transfer".into(),
                    direction: "incoming".into(),
                    status: "pending".into(),
                    source_peer_id: Some("peer-source".into()),
                    target_peer_id: None,
                    source_desktop_id: None,
                    target_desktop_id: None,
                    source_task_id: Some("task-source".into()),
                    local_task_id: None,
                    error: None,
                    payload_json: Some(payload_json),
                })
                .expect("incoming transfer");
            },
        );

        let failure = run_import(&state, &work, "legacy-transfer")
            .await
            .expect_err("legacy payload was allowed to finalize and import");
        let ImportFailure::Terminal(reason) = failure else {
            panic!("legacy payload was treated as retryable");
        };
        assert!(reason.contains("cannot prove the task head"), "{reason}");
        let transfer = state
            .transfer_work()
            .open_db()
            .expect("db")
            .get_task_transfer("legacy-transfer")
            .expect("read transfer")
            .expect("transfer");
        assert_eq!(transfer.local_task_id, None);
        assert_eq!(transfer.status, "claimed");
    }

    /// The retry seam migration 050 exists for.
    ///
    /// Attempt 1 fetches the artifacts and writes them to disk; by attempt 2
    /// those destinations are occupied *by attempt 1*, so a fresh look reads
    /// its own output as somebody else's and abandons the resume. The recorded
    /// answer is the only one taken against the machine as it was, so attempt 2
    /// has to return it rather than recompute.
    ///
    /// The DB primitive and the pure predicate are pinned elsewhere; what this
    /// covers is that `materialize_resume_state` is wired to them.
    #[tokio::test]
    async fn a_retried_import_returns_the_session_the_first_attempt_materialized() {
        let work = work_item("import:transfer-1");
        let recorded = "364643cc-5e6d-48fc-86ca-ca7764380900";
        let state =
            crate::http_api::test_state_with_seed("desktop-import-memo", "Import Memo", |db| {
                db.enqueue_transfer_work(&work_item("import:transfer-1").id, "import", None, "{}")
                    .expect("queue the work item");
                db.record_transfer_work_observation(
                    "import:transfer-1",
                    MATERIALIZE_PHASE,
                    Some(recorded),
                )
                .expect("attempt 1's answer");
            });

        // The payload names a *different* session and would recompute to
        // `None`, so neither value can be reached by accident.
        let resolved = materialize_resume_state(
            &state,
            &work,
            "transfer-1",
            &payload_promising(Some("11111111-2222-3333-4444-555555555555")),
            Path::new("/tmp/kanna-import-memo-destination"),
        )
        .await
        .expect("the retry failed instead of reusing attempt 1's answer");
        assert_eq!(resolved.as_deref(), Some(recorded));

        // Reading is not writing: the recorded answer is still attempt 1's, so
        // a third attempt sees the same thing.
        assert_eq!(
            state
                .transfer_work()
                .open_db()
                .expect("db")
                .read_transfer_work_observation("import:transfer-1", MATERIALIZE_PHASE)
                .expect("read"),
            Some(Some(recorded.to_string())),
        );
    }

    /// The other half of the recorded value: its encoding.
    ///
    /// `None` in the observation column already means "never observed", so an
    /// attempt that decided the resume had to be abandoned records the empty
    /// marker instead. Reading that back as a session id would spawn the
    /// destination agent with `--resume ""`. Unlike the test above this one does
    /// not distinguish the memo from a recompute — both answer `None` for this
    /// payload — it pins the decoding the memo needs to be usable at all.
    #[tokio::test]
    async fn the_abandoned_marker_reads_back_as_no_resume_rather_than_a_session_id() {
        let work = work_item("import:transfer-abandoned");
        let state = crate::http_api::test_state_with_seed(
            "desktop-import-abandoned",
            "Import Abandoned",
            |db| {
                db.enqueue_transfer_work("import:transfer-abandoned", "import", None, "{}")
                    .expect("queue the work item");
                db.record_transfer_work_observation(
                    "import:transfer-abandoned",
                    MATERIALIZE_PHASE,
                    Some(RESUME_ABANDONED),
                )
                .expect("attempt 1's answer");
            },
        );

        let resolved = materialize_resume_state(
            &state,
            &work,
            "transfer-abandoned",
            &payload_promising(Some("364643cc-5e6d-48fc-86ca-ca7764380900")),
            Path::new("/tmp/kanna-import-abandoned-destination"),
        )
        .await
        .expect("the retry failed instead of reusing attempt 1's answer");
        assert_eq!(
            resolved, None,
            "the abandoned marker leaked out as a session id",
        );
    }

    // -----------------------------------------------------------------
    // Item 4: retry/replay against the durable destination-server proof.
    //
    // These exercise the real `run_import`, a real destination SQLite DB,
    // and TWO real `kanna-task-transfer` sidecar *subprocesses* — a source
    // and a destination, genuinely paired (`start-pairing`/`accept-pairing`)
    // and driven through a genuine preflight+commit — not a helper predicate
    // standing in for any of it. Peer discovery uses the sidecar's existing
    // `KANNA_TRANSFER_DISCOVERY=registry` file-based mode (the same one
    // `crates/task-transfer/tests/sidecar_control.rs` uses), not real
    // multicast mDNS, so pairing is fast and does not depend on the test
    // environment's network configuration. `KANNA_TRANSFER_PORT=0` lets the
    // OS assign each sidecar's listener port, so nothing here can collide
    // with another worktree's or another test's fixed port.
    //
    // Requires the sidecar binary to actually be built first:
    //   cargo build -p kanna-task-transfer
    // (add --release for a release-profile binary). Left as a clear panic
    // naming this command, per this task's instructions, rather than a
    // silent skip.

    fn real_sidecar_binary_for_test() -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("kanna-server crate sits two path segments under the workspace root")
            .to_path_buf();
        let build_root = workspace_root.join(".build");
        for candidate in [
            build_root.join("debug").join("kanna-task-transfer"),
            build_root.join("release").join("kanna-task-transfer"),
        ] {
            if candidate.exists() {
                return candidate;
            }
        }
        panic!(
            "kanna-task-transfer sidecar binary not found under {}; build it first with \
             `cargo build -p kanna-task-transfer` (or add --release) from the repo root, \
             then re-run this test",
            build_root.display(),
        );
    }

    /// Snapshots the sidecar identity env vars these fixtures mutate and
    /// restores their prior values (or absence) on drop, including on panic
    /// unwind — `test_sidecar_guard` serializes the *processes* these tests
    /// spawn, it does not touch the ambient environment they read from at
    /// spawn time. Capture this before the first mutation and hold it for
    /// the whole test, declared after `test_sidecar_guard`'s own guard so it
    /// drops (and restores) first, while that lock is still held.
    struct Item4EnvVarGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl Item4EnvVarGuard {
        const NAMES: [&'static str; 5] = [
            "KANNA_TRANSFER_ROOT",
            "KANNA_TRANSFER_REGISTRY_DIR",
            "KANNA_TRANSFER_PEER_ID",
            "KANNA_TRANSFER_DISPLAY_NAME",
            "KANNA_TRANSFER_DISCOVERY",
        ];

        fn capture() -> Self {
            Self {
                saved: Self::NAMES
                    .iter()
                    .map(|&name| (name, std::env::var(name).ok()))
                    .collect(),
            }
        }
    }

    impl Drop for Item4EnvVarGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    struct Item4HomeGuard(Option<String>);

    impl Item4HomeGuard {
        fn set(path: &Path) -> Self {
            let prior = std::env::var("HOME").ok();
            std::env::set_var("HOME", path);
            Self(prior)
        }
    }

    impl Drop for Item4HomeGuard {
        fn drop(&mut self) {
            match self.0.as_deref() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn new_test_work_queue(label: &str) -> Arc<crate::transfer_engine::queue::TransferWorkQueue> {
        let db_path = crate::test_paths::unique_test_path_string(&format!(
            "kanna-transfer-item4-work-{label}"
        ));
        // `TransferWorkQueue::open_db` is a bare `Db::open`, which expects an
        // already-migrated file — in production this path is always the same
        // `config.db_path` the main kanna-server DB already migrated. This
        // queue's own dedicated path needs the same one-time migration a
        // fresh file never gets otherwise.
        crate::db::Db::open_for_tests(&db_path).expect("create and migrate transfer work db");
        crate::transfer_engine::queue::TransferWorkQueue::new(db_path)
    }

    /// Spawns a real sidecar subprocess rooted at `root`, discovering peers
    /// through the shared `registry_dir` rather than real mDNS. Reusing the
    /// same `(root, peer_id)` pair across calls after dropping the previous
    /// supervisor represents that same machine's sidecar restarting:
    /// identity and durable on-disk state (`root`) persist, everything
    /// in-memory does not.
    ///
    /// Forces the actual spawn *now*, via a harmless `identity` control call,
    /// rather than leaving it lazy: `TransferSidecarClient::spawn` reads the
    /// identity env vars this sets from the process environment only at
    /// spawn time, so a caller that built two supervisors back to back and
    /// let both spawn lazily on first real use would hand the second
    /// identity's environment to whichever supervisor happened to be used
    /// first — not necessarily the one this call built.
    ///
    /// Mutates process-global identity env vars — callers must hold both
    /// `crate::test_sidecar_guard()` and `Item4EnvVarGuard::capture()` for
    /// the whole test.
    async fn spawn_test_sidecar(
        root: &Path,
        registry_dir: &Path,
        peer_id: &str,
        config: &crate::config::Config,
        work: Arc<crate::transfer_engine::queue::TransferWorkQueue>,
    ) -> crate::transfer_sidecar::TransferSidecarSupervisor {
        std::env::set_var("KANNA_TRANSFER_ROOT", root);
        std::env::set_var("KANNA_TRANSFER_REGISTRY_DIR", registry_dir);
        std::env::set_var("KANNA_TRANSFER_PEER_ID", peer_id);
        std::env::set_var("KANNA_TRANSFER_DISPLAY_NAME", peer_id);
        std::env::set_var("KANNA_TRANSFER_DISCOVERY", "registry");
        let supervisor = crate::transfer_sidecar::TransferSidecarSupervisor::with_binary_for_test(
            config.clone(),
            work,
            real_sidecar_binary_for_test(),
        );
        supervisor
            .control("identity", serde_json::json!({}))
            .await
            .expect("sidecar must come up and answer under its own identity");
        supervisor
    }

    fn item4_test_config(label: &str) -> crate::config::Config {
        crate::config::Config {
            relay_url: "wss://relay.example".to_string(),
            device_token: "device-token".to_string(),
            firebase_project_id: "kanna-local".to_string(),
            firebase_auth_emulator_url: None,
            firebase_firestore_emulator_host: None,
            daemon_dir: crate::test_paths::unique_test_path_string(&format!(
                "kanna-daemon-item4-{label}"
            )),
            db_path: crate::db::Db::test_db_path(&format!("item4-{label}")),
            kanna_cli_path: None,
            desktop_id: format!("desktop-item4-{label}"),
            desktop_secret: Some("desktop-secret".to_string()),
            desktop_name: "Item4 Destination".to_string(),
            version: "test-version".to_string(),
            environment: "development".to_string(),
            lan_host: "127.0.0.1".to_string(),
            lan_port: 48120,
            // Let the OS assign the sidecar's listener port; two fixed test
            // ports collided across concurrently running worktrees.
            transfer_port: 0,
            activity_event_debounce_seconds: 300,
            pairing_store_path: crate::test_paths::unique_test_file(
                &format!("kanna-pairings-item4-{label}"),
                "json",
            ),
        }
    }

    /// A minimal, valid `task-bundle` payload — the only mode `run_import`
    /// accepts — for a transfer whose `local_task_id` is already known (the
    /// retry/re-entry shape every test below exercises). `stage` is the one
    /// field the negative-control tests vary, to route
    /// `verify_persisted_task_bundle` into an early, git-free, deterministic
    /// failure without a real repository fixture.
    fn item4_task_bundle_payload_json(source_task_id: &str, stage: &str) -> serde_json::Value {
        serde_json::json!({
            "target_peer_id": "peer-item4-dest",
            "task": {
                "source_peer_id": "peer-item4-source",
                "source_task_id": source_task_id,
                "resume_session_id": null,
                "stage": stage,
                "pipeline": "single-reviewer",
                "agent_type": "pty",
                "agent_provider": "claude",
                "workflow_definition": "{\"stages\":[{\"name\":\"in progress\"}]}",
                "head_oid": "a".repeat(40),
                "base_oid": "b".repeat(40),
            },
            "repo": {
                "mode": "task-bundle",
                "bundle": {
                    "artifact_id": "transfer-repo-bundle",
                    "filename": "transfer.bundle",
                    "ref_name": "refs/heads/task-item4-source",
                    "base_ref_name": "refs/heads/main",
                },
            },
            "input_ledger": {
                "artifact_id": "transfer-inputs",
                "filename": payload::TASK_INPUT_LEDGER_FILENAME,
                "sha256": "c".repeat(64),
                "count": 0,
            },
            "artifacts": [],
        })
    }

    fn item4_new_task_transfer(
        transfer_id: &str,
        local_task_id: &str,
        source_task_id: &str,
        payload_stage: &str,
    ) -> crate::db::NewTaskTransfer {
        crate::db::NewTaskTransfer {
            id: transfer_id.to_string(),
            direction: "incoming".to_string(),
            status: "importing".to_string(),
            source_peer_id: Some("peer-item4-source".to_string()),
            target_peer_id: None,
            source_desktop_id: None,
            target_desktop_id: None,
            source_task_id: Some(source_task_id.to_string()),
            local_task_id: Some(local_task_id.to_string()),
            error: None,
            payload_json: Some(
                item4_task_bundle_payload_json(source_task_id, payload_stage).to_string(),
            ),
        }
    }

    /// Blocks (via the event log's own `wait_for_events`, which blocks in
    /// bounded rechecks rather than a manual sleep loop) until an event of
    /// `expected_type` appears, advancing `cursor` past everything read.
    async fn wait_for_event_type(
        log: &Arc<crate::transfer_sidecar::TransferEventLog>,
        cursor: &mut u64,
        expected_type: &str,
        timeout: std::time::Duration,
    ) -> serde_json::Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let batch = log
                .wait_for_events(Some(*cursor), None, 50, remaining)
                .await;
            *cursor = batch.cursor;
            if let Some(found) = batch
                .events
                .iter()
                .find(|entry| entry["event"]["type"].as_str() == Some(expected_type))
            {
                return found["event"].clone();
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for sidecar event `{expected_type}`");
            }
        }
    }

    /// Polls a durable work queue for its next item, retrying on an empty
    /// read. `incoming_transfer_request` and `outgoing_transfer_committed`
    /// are *durable* sidecar events
    /// (`crate::transfer_engine::queue::is_durable_transfer_event`) — the
    /// sidecar subprocess's own stdout reader routes them straight into this
    /// queue's SQLite table, never into the advisory `TransferEventLog`
    /// `wait_for_event_type` reads, and that routing itself races the
    /// caller: `claim_next_transfer_work` can genuinely observe nothing yet
    /// even after the control call that triggered the event has already
    /// returned, since the sidecar's own stdout write and this queue's
    /// background reader are a further asynchronous hop past that.
    async fn wait_for_durable_work(
        work: &Arc<crate::transfer_engine::queue::TransferWorkQueue>,
        expected_kind: &str,
        timeout: std::time::Duration,
    ) -> crate::db::TransferWorkItem {
        use std::future::Future as _;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Register interest in the next `enqueue` *before* checking the
            // DB, not after: `wait_for_work` is a plain `async fn`, so
            // merely constructing its future runs none of its body —
            // nothing is actually registered with the queue's `Notify`
            // until the future is polled at least once. Polling it here
            // (with a no-op waker; we are not yet ready to actually sleep
            // on it) reaches and runs `wait_for_work`'s own
            // `notified.as_mut().enable()` line, which is what makes a
            // `notify_waiters()` racing the DB check below observable at
            // all — `Notify::notify_waiters` has no saved permit, so a
            // notification that fires while nothing is registered is lost
            // forever, not queued for the next waiter.
            let mut waiter =
                std::pin::pin!(work
                    .wait_for_work(deadline.saturating_duration_since(std::time::Instant::now())));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = waiter.as_mut().poll(&mut context);

            let claimed = work
                .open_db()
                .expect("open transfer work db")
                .claim_next_transfer_work(&[])
                .expect("claim transfer work");
            if let Some(item) = claimed {
                assert_eq!(
                    item.kind, expected_kind,
                    "unexpected durable work item claimed while waiting for `{expected_kind}`"
                );
                return item;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for durable transfer work of kind `{expected_kind}`");
            }
            // The same, already-registered future: a concurrent `enqueue`
            // between the poll above and here is not missed.
            waiter.await;
        }
    }

    /// `start-pairing` targets a peer_id directly; against the file-based
    /// registry there is a short real window between a peer process coming
    /// up and it having written its own registry entry, so this retries —
    /// *only* that specific, proven pre-dispatch discovery failure
    /// (`RuntimeError::PeerNotFound`'s own `"peer not found: "` prefix,
    /// `crates/task-transfer/src/runtime/events.rs`), matching
    /// `control::is_connection_failure`'s own recognition of the same
    /// fragment — instead of requiring the caller to poll the registry file
    /// itself, which would need a `kanna_task_transfer` dependency this
    /// crate deliberately does not take. Any other error (a genuine timeout,
    /// a rejected/failed handshake, a protocol error) is a real answer and
    /// must fail the test rather than being silently resubmitted.
    async fn start_pairing_with_retry(
        source: &crate::transfer_sidecar::TransferSidecarSupervisor,
        target_peer_id: &str,
        timeout: std::time::Duration,
    ) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match source
                .control(
                    "start-pairing",
                    serde_json::json!({ "peerId": target_peer_id }),
                )
                .await
            {
                Ok(response) => {
                    return response["verificationCode"]
                        .as_str()
                        .expect("start-pairing response missing verificationCode")
                        .to_string();
                }
                Err(error) if error.contains("peer not found:") => {
                    if std::time::Instant::now() >= deadline {
                        panic!(
                            "start-pairing against {target_peer_id} never found that peer in \
                             the registry: {error}"
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => {
                    panic!("start-pairing against {target_peer_id} failed: {error}");
                }
            }
        }
    }

    /// Drives a real LAN pairing handshake between two real sidecar
    /// subprocesses to completion, over the actual `start-pairing` /
    /// `accept-pairing` control ops and the actual `pairing_requested`
    /// advisory event — the same seam the desktop uses, exercised end to
    /// end rather than assumed.
    ///
    /// `source`'s `start-pairing` wire request cannot itself complete until
    /// `destination` calls `accept-pairing`
    /// (`crates/task-transfer/src/runtime/listener.rs`'s `StartPairing`
    /// handler holds the connection open, waiting on its own
    /// `approval_receiver`, before ever writing a response) — so this must
    /// race the two sides with `tokio::select!`, matching
    /// `crates/task-transfer/tests/runtime.rs`'s own `pair_peers` helper,
    /// rather than awaiting `start-pairing` to completion first. Awaiting it
    /// first deadlocks: nothing would ever call `accept-pairing`, and both
    /// sides' `peer_request_timeout` (15s) would fire instead.
    async fn pair_real_sidecars(
        source: &crate::transfer_sidecar::TransferSidecarSupervisor,
        destination: &crate::transfer_sidecar::TransferSidecarSupervisor,
        destination_peer_id: &str,
    ) {
        let pairing = start_pairing_with_retry(
            source,
            destination_peer_id,
            std::time::Duration::from_secs(30),
        );
        tokio::pin!(pairing);

        let destination_events = destination.events();
        let mut destination_cursor = 0u64;
        let requested = tokio::select! {
            biased;
            code = &mut pairing => {
                panic!(
                    "start-pairing completed before the destination ever emitted a \
                     pairing_requested event to accept, with code {code}"
                );
            }
            event = wait_for_event_type(
                &destination_events,
                &mut destination_cursor,
                "pairing_requested",
                std::time::Duration::from_secs(30),
            ) => event,
        };
        let pairing_request_id = requested["request_id"]
            .as_str()
            .expect("pairing_requested event missing request_id")
            .to_string();
        let verification_code = requested["verification_code"]
            .as_str()
            .expect("pairing_requested event missing verification_code")
            .to_string();
        destination
            .control(
                "accept-pairing",
                serde_json::json!({
                    "pairingRequestId": pairing_request_id,
                    "verificationCode": verification_code,
                }),
            )
            .await
            .expect("accept-pairing must succeed once the request is genuinely pending");

        // Only now can the source's still-pending start-pairing wire request
        // actually complete.
        let completed_code = pairing.await;
        assert_eq!(
            completed_code, verification_code,
            "the pairing request's code must match the code start-pairing returned"
        );
    }

    /// Drives a real preflight+commit from `source` to `destination_peer_id`
    /// over the wire, returning the transfer_id the destination's sidecar
    /// minted — this is what actually creates the destination's
    /// `incoming_reservations` entry `acknowledge_import_committed` needs;
    /// nothing about it is fabricated. The wire-level commit payload is
    /// deliberately minimal (matching
    /// `crates/task-transfer/tests/runtime.rs`'s own
    /// `destination_can_acknowledge_import_commit_back_to_source`): the
    /// sidecar does not itself validate the task-bundle contract, only
    /// `run_import`'s own parse of the *separately seeded*
    /// `task_transfer.payload_json` DB column does.
    async fn commit_real_transfer(
        source: &crate::transfer_sidecar::TransferSidecarSupervisor,
        destination_peer_id: &str,
        source_task_id: &str,
    ) -> String {
        let preflight = source
            .control(
                "prepare-outgoing-transfer",
                serde_json::json!({
                    "payload": {
                        "phase": "preflight",
                        "sourceTaskId": source_task_id,
                        "targetPeerId": destination_peer_id,
                    }
                }),
            )
            .await
            .expect("preflight must succeed against a paired peer");
        let transfer_id = preflight["transferId"]
            .as_str()
            .expect("preflight response missing transferId")
            .to_string();
        source
            .control(
                "prepare-outgoing-transfer",
                serde_json::json!({
                    "payload": {
                        "phase": "commit",
                        "transferId": transfer_id,
                        "payload": {
                            "target_peer_id": destination_peer_id,
                            "task": { "source_task_id": source_task_id },
                        },
                    }
                }),
            )
            .await
            .expect("commit must be accepted by a paired peer");
        transfer_id
    }

    /// A genuine, wire-admitted transfer reservation between two real
    /// sidecar subprocesses, plus everything a caller needs to spawn its own
    /// further destination incarnations against the same durable identity
    /// and read back what the source durably recorded.
    struct Item4RealTransfer {
        // Never read again after construction — held only so its `Drop` (and
        // the real child process it owns) outlives every destination
        // incarnation this reservation's callers spawn.
        #[allow(dead_code)]
        source: crate::transfer_sidecar::TransferSidecarSupervisor,
        source_work: Arc<crate::transfer_engine::queue::TransferWorkQueue>,
        transfer_id: String,
        registry_dir: PathBuf,
        destination_root: PathBuf,
        destination_peer_id: String,
        source_peer_id: String,
        source_task_id: String,
    }

    impl Item4RealTransfer {
        async fn spawn_destination(
            &self,
            label: &str,
        ) -> crate::transfer_sidecar::TransferSidecarSupervisor {
            let config = item4_test_config(label);
            let work = new_test_work_queue(label);
            spawn_test_sidecar(
                &self.destination_root,
                &self.registry_dir,
                &self.destination_peer_id,
                &config,
                work,
            )
            .await
        }
    }

    /// Pairs a real source and destination sidecar over a shared file-based
    /// registry (no real mDNS), then drives a real preflight+commit from
    /// source to destination, so the destination's own
    /// `incoming_reservations` entry for the returned `transfer_id` is
    /// genuine wire-admitted state — not a fixture-only assumption. The
    /// bootstrap destination incarnation used to receive the commit is
    /// dropped before returning: every caller spawns its own destination
    /// incarnation(s) against `destination_root` via
    /// `Item4RealTransfer::spawn_destination`, so `run_import` always talks
    /// to a process that must reload this reservation from disk, exactly
    /// like a real restart — including in the one-incarnation case, which is
    /// still a restart relative to the incarnation that actually received
    /// the commit.
    async fn establish_real_transfer_reservation(label: &str) -> Item4RealTransfer {
        let registry_dir =
            crate::test_paths::unique_test_dir(&format!("kanna-transfer-item4-registry-{label}"));
        std::fs::create_dir_all(&registry_dir).expect("create registry dir");

        let source_root =
            crate::test_paths::unique_test_dir(&format!("kanna-transfer-item4-source-{label}"));
        std::fs::create_dir_all(&source_root).expect("create source root");
        let source_peer_id = format!("peer-item4-source-{label}");
        let source_config = item4_test_config(&format!("{label}-source"));
        let source_work = new_test_work_queue(&format!("{label}-source"));
        let source = spawn_test_sidecar(
            &source_root,
            &registry_dir,
            &source_peer_id,
            &source_config,
            Arc::clone(&source_work),
        )
        .await;

        let destination_root =
            crate::test_paths::unique_test_dir(&format!("kanna-transfer-item4-dest-{label}"));
        std::fs::create_dir_all(&destination_root).expect("create destination root");
        let destination_peer_id = format!("peer-item4-dest-{label}");
        let bootstrap_config = item4_test_config(&format!("{label}-dest-bootstrap"));
        let bootstrap_work = new_test_work_queue(&format!("{label}-dest-bootstrap"));
        let destination_bootstrap = spawn_test_sidecar(
            &destination_root,
            &registry_dir,
            &destination_peer_id,
            &bootstrap_config,
            Arc::clone(&bootstrap_work),
        )
        .await;

        pair_real_sidecars(&source, &destination_bootstrap, &destination_peer_id).await;

        let source_task_id = format!("task-item4-source-{label}");
        let transfer_id =
            commit_real_transfer(&source, &destination_peer_id, &source_task_id).await;

        // `incoming_transfer_request` is durable (routed to the work queue,
        // never the advisory event log) — see `wait_for_durable_work`.
        let incoming_work = wait_for_durable_work(
            &bootstrap_work,
            crate::transfer_engine::queue::KIND_INCOMING_REQUEST,
            std::time::Duration::from_secs(15),
        )
        .await;
        let incoming: serde_json::Value = serde_json::from_str(&incoming_work.payload_json)
            .expect("parse incoming-transfer-request event");
        assert_eq!(
            incoming["transfer_id"].as_str(),
            Some(transfer_id.as_str()),
            "the destination's own incoming-transfer event must name the committed transfer"
        );

        // Drop the bootstrap incarnation: everything from here on reads only
        // the reservation genuinely persisted to disk, never this process's
        // still-warm in-memory state.
        drop(destination_bootstrap);

        Item4RealTransfer {
            source,
            source_work,
            transfer_id,
            registry_dir,
            destination_root,
            destination_peer_id,
            source_peer_id,
            source_task_id,
        }
    }

    /// Reads back the source sidecar's own durable record of the ack replay
    /// (`outgoing_transfer_committed`, queued via
    /// `crate::transfer_engine::queue::enqueue_durable_event` exactly as
    /// production does) and asserts every field the destination sent
    /// matches what this test actually persisted — proof of the values on
    /// the wire, not merely that the call was reached.
    async fn assert_real_ack_values(
        reservation: &Item4RealTransfer,
        local_task_id: &str,
        content_commitment: &str,
        destination_repo_id: &str,
    ) {
        // Durable, and racy for the same reason `incoming_transfer_request`
        // is: the sidecar's own event emission and this queue's background
        // reader are hops past `run_import`'s `Ok(())` return, not before it.
        let ack_work = wait_for_durable_work(
            &reservation.source_work,
            crate::transfer_engine::queue::KIND_OUTGOING_COMMITTED,
            std::time::Duration::from_secs(15),
        )
        .await;
        let ack_event: serde_json::Value =
            serde_json::from_str(&ack_work.payload_json).expect("parse outgoing-committed event");
        assert_eq!(
            ack_event["transfer_id"].as_str(),
            Some(reservation.transfer_id.as_str())
        );
        assert_eq!(
            ack_event["source_task_id"].as_str(),
            Some(reservation.source_task_id.as_str())
        );
        assert_eq!(
            ack_event["destination_local_task_id"].as_str(),
            Some(local_task_id)
        );
        assert_eq!(
            ack_event["content_commitment"].as_str(),
            Some(content_commitment),
            "the content commitment replayed to the source must be exactly the persisted proof"
        );
        assert_eq!(
            ack_event["destination_repo_id"].as_str(),
            Some(destination_repo_id)
        );
    }

    /// The decisive positive case: once `verify_persisted_task_bundle` has
    /// already persisted a content commitment for this transfer, a retry
    /// must skip it entirely — no artifact or input-ledger fetch — go
    /// straight to replaying the acknowledgment, and that replay must carry
    /// the exact persisted commitment, destination repo id, and task
    /// binding all the way to the source over a real wire round trip.
    #[tokio::test]
    async fn retry_with_persisted_commitment_skips_reverification_and_replays_the_real_ack() {
        let _guard = crate::test_sidecar_guard().await;
        let _env_guard = Item4EnvVarGuard::capture();
        let reservation = establish_real_transfer_reservation("persisted").await;

        let local_task_id = "task-item4-persisted";
        let repo_id = "repo-item4-persisted";
        let kanna_config = item4_test_config("persisted-kanna-server");
        {
            let db = crate::db::Db::open_for_tests(&kanna_config.db_path).expect("open test db");
            db.insert_test_repo(repo_id, "Item4 Repo")
                .expect("insert repo");
            db.insert_test_pipeline_item(
                local_task_id,
                repo_id,
                "resume the transferred agent",
                None,
                "in progress",
                "2026-09-09T00:00:00Z",
            )
            .expect("insert pipeline item");
            db.upsert_transferred_task_manifest(
                &reservation.transfer_id,
                repo_id,
                Some(local_task_id),
                &"a".repeat(40),
                &"b".repeat(40),
            )
            .expect("upsert manifest");
            assert!(db
                .mark_transferred_task_manifest_prepared(&reservation.transfer_id)
                .expect("mark prepared"));
            assert!(db
                .set_transferred_task_manifest_content_commitment(
                    &reservation.transfer_id,
                    "persisted-commitment-value",
                )
                .expect("persist commitment"));
            let mut transfer = item4_new_task_transfer(
                &reservation.transfer_id,
                local_task_id,
                &reservation.source_task_id,
                "in progress",
            );
            transfer.source_peer_id = Some(reservation.destination_peer_id.clone());
            db.insert_task_transfer(&transfer)
                .expect("insert task transfer");
        }

        let destination = reservation.spawn_destination("persisted-dest").await;
        let state = Arc::new(AppState::with_transfer_sidecar_for_test(
            kanna_config.clone(),
            destination,
        ));

        let work = work_item("import:transfer-item4-persisted");
        let result = run_import(&state, &work, &reservation.transfer_id).await;
        assert!(
            result.is_ok(),
            "expected the retry to succeed via a real ack replay: {result:?}"
        );

        assert_real_ack_values(
            &reservation,
            local_task_id,
            "persisted-commitment-value",
            repo_id,
        )
        .await;

        let commitment = state
            .transfer_work()
            .open_db()
            .expect("db")
            .transferred_task_manifest_content_commitment(&reservation.transfer_id)
            .expect("read commitment");
        assert_eq!(
            commitment.as_deref(),
            Some("persisted-commitment-value"),
            "a successful retry must never recompute or clear the persisted content commitment"
        );

        let transfer = state
            .transfer_work()
            .open_db()
            .expect("db")
            .get_task_transfer(&reservation.transfer_id)
            .expect("read transfer")
            .expect("transfer exists");
        assert_eq!(transfer.status, "completed");
    }

    /// Same skip-path and same real, wire-admitted reservation, but an extra
    /// destination sidecar *process* restart happens between the commit and
    /// the incarnation `run_import` actually talks to — so its in-memory
    /// `transfer_artifacts` cache and its in-memory `incoming_reservations`
    /// map are both genuinely empty, and only what
    /// `Item4RealTransfer::spawn_destination`'s fresh process reloads from
    /// `destination_root` on disk is available. The retry must still
    /// succeed, with the same real ack values reaching the source.
    #[tokio::test]
    async fn retry_with_persisted_commitment_survives_a_real_sidecar_process_restart() {
        let _guard = crate::test_sidecar_guard().await;
        let _env_guard = Item4EnvVarGuard::capture();
        let reservation = establish_real_transfer_reservation("restart").await;

        // One more incarnation between the commit and the one `run_import`
        // uses: proves the reservation survives more than the single
        // restart every other test already gets from
        // `establish_real_transfer_reservation` dropping its own bootstrap
        // incarnation.
        {
            let _extra = reservation.spawn_destination("restart-extra").await;
        }

        let local_task_id = "task-item4-restart";
        let repo_id = "repo-item4-restart";
        let kanna_config = item4_test_config("restart-kanna-server");
        {
            let db = crate::db::Db::open_for_tests(&kanna_config.db_path).expect("open test db");
            db.insert_test_repo(repo_id, "Item4 Repo")
                .expect("insert repo");
            db.insert_test_pipeline_item(
                local_task_id,
                repo_id,
                "resume the transferred agent",
                None,
                "in progress",
                "2026-09-09T00:00:00Z",
            )
            .expect("insert pipeline item");
            db.upsert_transferred_task_manifest(
                &reservation.transfer_id,
                repo_id,
                Some(local_task_id),
                &"a".repeat(40),
                &"b".repeat(40),
            )
            .expect("upsert manifest");
            assert!(db
                .mark_transferred_task_manifest_prepared(&reservation.transfer_id)
                .expect("mark prepared"));
            assert!(db
                .set_transferred_task_manifest_content_commitment(
                    &reservation.transfer_id,
                    "persisted-commitment-value-restart",
                )
                .expect("persist commitment"));
            let mut transfer = item4_new_task_transfer(
                &reservation.transfer_id,
                local_task_id,
                &reservation.source_task_id,
                "in progress",
            );
            transfer.source_peer_id = Some(reservation.destination_peer_id.clone());
            db.insert_task_transfer(&transfer)
                .expect("insert task transfer");
        }

        let destination = reservation.spawn_destination("restart-final").await;
        let state = Arc::new(AppState::with_transfer_sidecar_for_test(
            kanna_config.clone(),
            destination,
        ));

        let work = work_item("import:transfer-item4-restart");
        let result = run_import(&state, &work, &reservation.transfer_id).await;
        assert!(
            result.is_ok(),
            "the reservation, and therefore the retry, must survive a real sidecar restart: \
             {result:?}"
        );

        assert_real_ack_values(
            &reservation,
            local_task_id,
            "persisted-commitment-value-restart",
            repo_id,
        )
        .await;
    }

    /// Negative control: with no persisted content commitment, a retry must
    /// still take the full `verify_persisted_task_bundle` path — proven
    /// here, git-free and deterministically, by a deliberate stage mismatch
    /// that path's very first check catches. This is the gate's other
    /// failure mode: skip only when genuinely proven, never by default.
    #[tokio::test]
    async fn retry_with_no_persisted_commitment_still_takes_the_verification_path() {
        let transfer_id = "transfer-item4-unverified";
        let local_task_id = "task-item4-unverified";
        let state = crate::http_api::test_state_with_seed(
            "desktop-item4-unverified",
            "Item4 Unverified",
            |db| {
                db.insert_test_repo("repo-item4-unverified", "Item4 Repo")
                    .expect("insert repo");
                db.insert_test_pipeline_item(
                    local_task_id,
                    "repo-item4-unverified",
                    "resume the transferred agent",
                    None,
                    "in progress",
                    "2026-09-09T00:00:00Z",
                )
                .expect("insert pipeline item");
                db.upsert_transferred_task_manifest(
                    transfer_id,
                    "repo-item4-unverified",
                    Some(local_task_id),
                    &"a".repeat(40),
                    &"b".repeat(40),
                )
                .expect("upsert manifest");
                // Deliberately no content commitment persisted.
                db.insert_task_transfer(&item4_new_task_transfer(
                    transfer_id,
                    local_task_id,
                    "task-item4-source",
                    // The payload's stage disagrees with the pipeline_item's
                    // seeded "in progress" stage above.
                    "review",
                ))
                .expect("insert task transfer");
            },
        );

        let parsed = payload::parse_outgoing_transfer_payload(&item4_task_bundle_payload_json(
            "task-item4-source",
            "review",
        ))
        .expect("payload");
        let failure =
            verify_persisted_task_bundle(&state, &parsed, local_task_id, transfer_id, Some(&[]))
                .await
                .expect_err("a stage mismatch must refuse the import");
        let ImportFailure::Terminal(reason) = failure else {
            panic!("expected a terminal verification failure: {failure:?}");
        };
        assert!(
            reason.contains("stage does not match the source context"),
            "an absent content commitment must still route through \
             verify_persisted_task_bundle: {reason}"
        );

        let db = state.transfer_work().open_db().expect("db");
        assert!(
            db.transferred_task_manifest_content_commitment(transfer_id)
                .expect("read commitment")
                .is_none(),
            "a failed verification must not fabricate a commitment"
        );
    }

    /// Negative control: a content commitment durably proven for a
    /// *different* local task must not authorize this task's ack replay.
    /// This is the existing task-binding guard (`import.rs`'s own
    /// `bound_task != local_task_id` check), exercised specifically through
    /// the skip-path this task adds: the persisted-commitment gate answers
    /// `Some` for the transfer_id alone, before the binding is checked, so
    /// this proves that ordering still ends in a refusal rather than an
    /// authorized ack.
    #[tokio::test]
    async fn a_commitment_proven_for_another_task_cannot_authorize_this_tasks_ack() {
        let transfer_id = "transfer-item4-mismatch";
        let proven_task_id = "task-item4-mismatch-a";
        let retrying_task_id = "task-item4-mismatch-b";
        let state = crate::http_api::test_state_with_seed(
            "desktop-item4-mismatch",
            "Item4 Mismatch",
            |db| {
                db.insert_test_repo("repo-item4-mismatch", "Item4 Repo")
                    .expect("insert repo");
                for task_id in [proven_task_id, retrying_task_id] {
                    db.insert_test_pipeline_item(
                        task_id,
                        "repo-item4-mismatch",
                        "resume the transferred agent",
                        None,
                        "in progress",
                        "2026-09-09T00:00:00Z",
                    )
                    .expect("insert pipeline item");
                }
                // The manifest is genuinely proven, but bound to task A.
                db.upsert_transferred_task_manifest(
                    transfer_id,
                    "repo-item4-mismatch",
                    Some(proven_task_id),
                    &"a".repeat(40),
                    &"b".repeat(40),
                )
                .expect("upsert manifest");
                assert!(db
                    .mark_transferred_task_manifest_prepared(transfer_id)
                    .expect("mark prepared"));
                assert!(db
                    .set_transferred_task_manifest_content_commitment(
                        transfer_id,
                        "commitment-for-task-a",
                    )
                    .expect("persist commitment"));
                // But this retry's transfer row now names task B.
                db.insert_task_transfer(&item4_new_task_transfer(
                    transfer_id,
                    retrying_task_id,
                    "task-item4-source",
                    "in progress",
                ))
                .expect("insert task transfer");
            },
        );

        let work = work_item("import:transfer-item4-mismatch");
        let failure = run_import(&state, &work, transfer_id)
            .await
            .expect_err("a manifest proven for a different task must refuse this retry");
        let ImportFailure::Terminal(reason) = failure else {
            panic!("expected a terminal persisted-identity mismatch: {failure:?}");
        };
        assert!(
            reason.contains("conflicting persisted task identities"),
            "{reason}"
        );

        let commitment = state
            .transfer_work()
            .open_db()
            .expect("db")
            .transferred_task_manifest_content_commitment(transfer_id)
            .expect("read commitment");
        assert_eq!(
            commitment.as_deref(),
            Some("commitment-for-task-a"),
            "task A's proof must be untouched by task B's rejected attempt"
        );
    }

    async fn read_import_test_daemon_command(
        reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
    ) -> kanna_daemon::protocol::Command {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let command: kanna_daemon::protocol::Command =
                serde_json::from_str(line.trim()).unwrap();
            let response = match command {
                kanna_daemon::protocol::Command::NegotiateProtectedInput { .. } => {
                    Some(kanna_daemon::protocol::Event::ProtectedInputReady {
                        version: kanna_daemon::protocol::PROTECTED_INPUT_PROTOCOL_VERSION,
                    })
                }
                kanna_daemon::protocol::Command::NegotiateRawInput { .. } => {
                    Some(kanna_daemon::protocol::Event::RawInputReady {
                        version: kanna_daemon::protocol::RAW_INPUT_PROTOCOL_VERSION,
                    })
                }
                other => return other,
            };
            writer
                .write_all(
                    format!("{}\n", serde_json::to_string(&response.unwrap()).unwrap()).as_bytes(),
                )
                .await
                .unwrap();
        }
    }

    fn init_import_source_repo(path: &Path) -> (String, String, PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;

        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        std::fs::create_dir_all(path.join(".kanna/workflows")).unwrap();
        std::fs::create_dir_all(path.join(".kanna/test-provider-bin")).unwrap();
        let workflow = serde_json::json!({
            "name": "single-reviewer",
            "stages": [{
                "name": "in progress",
                "prompt": "$TASK_PROMPT",
                "policy": { "transition": "manual" }
            }]
        })
        .to_string();
        std::fs::write(
            path.join(".kanna/workflows/single-reviewer.json"),
            &workflow,
        )
        .unwrap();
        std::fs::write(
            path.join(".kanna/config.json"),
            serde_json::json!({
                "workspace": { "path": { "prepend": [".kanna/test-provider-bin"] } }
            })
            .to_string(),
        )
        .unwrap();
        let provider = path.join(".kanna/test-provider-bin/claude");
        std::fs::write(&provider, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(path.join("README.md"), "base\n").unwrap();
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Transfer Test"]);
        run(&["add", "."]);
        run(&["commit", "-m", "base"]);
        let base = super::super::git::commit_oid(path, "main").unwrap();
        run(&["checkout", "-b", "task-source"]);
        std::fs::write(path.join("task.txt"), "unpublished\n").unwrap();
        run(&["add", "task.txt"]);
        run(&["commit", "-m", "unpublished task work"]);
        let head = super::super::git::commit_oid(path, "task-source").unwrap();
        let bundle = path.join("transfer.bundle");
        run(&[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/task-source",
            "refs/heads/main",
        ]);
        (head, base, bundle, workflow)
    }

    /// The no-repository path used by the real incident: the destination
    /// durably binds its newly allocated repository before the next fallible
    /// operation. A simulated process interruption then re-enters `run_import`
    /// and must reuse that exact repo/path, finish the task, prove the complete
    /// snapshot before Spawn, and acknowledge over the real paired sidecars.
    #[tokio::test]
    async fn interrupted_new_repo_acquisition_reuses_one_manifest_binding_and_converges() {
        use tokio::io::{AsyncWriteExt, BufReader};

        let _guard = crate::test_sidecar_guard().await;
        let _env_guard = Item4EnvVarGuard::capture();
        let destination_home =
            crate::test_paths::unique_test_dir("kanna-transfer-acquisition-home");
        std::fs::create_dir_all(&destination_home).unwrap();
        let _home_guard = Item4HomeGuard::set(&destination_home);

        let reservation = establish_real_transfer_reservation("acquisition-retry").await;
        let source_repo = crate::test_paths::unique_test_dir("kanna-transfer-acquisition-source");
        let (head_oid, base_oid, bundle_path, workflow_definition) =
            init_import_source_repo(&source_repo);
        let source_remote = crate::test_paths::unique_test_dir("kanna-transfer-acquisition-origin");
        let init_remote = std::process::Command::new("git")
            .args(["init", "--bare", source_remote.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init_remote.status.success(), "{init_remote:?}");
        let source_input = crate::db::TaskInputRecord {
            id: 1,
            task_id: reservation.source_task_id.clone(),
            run_id: Some("source-run".into()),
            stage: Some("in progress".into()),
            source: "manager".into(),
            message: "preserve this imported directive".into(),
            delivered_at: "2026-09-10 12:00:00".into(),
            origin: None,
        };
        let ledger_bytes = payload::encode_task_input_ledger(
            &[source_input],
            &reservation.source_peer_id,
            &reservation.source_task_id,
        )
        .unwrap();
        let ledger_path = source_repo.join(payload::TASK_INPUT_LEDGER_FILENAME);
        std::fs::write(&ledger_path, &ledger_bytes).unwrap();
        for (artifact_id, path) in [
            ("acquisition-bundle", bundle_path.as_path()),
            ("acquisition-inputs", ledger_path.as_path()),
        ] {
            reservation
                .source
                .control(
                    "stage-artifact",
                    serde_json::json!({
                        "transferId": reservation.transfer_id,
                        "artifactId": artifact_id,
                        "path": path,
                        "owned": false,
                    }),
                )
                .await
                .expect("stage source artifact");
        }

        let destination_task_id = session::destination_task_id(&reservation.transfer_id);
        let payload_json = serde_json::json!({
            "target_peer_id": reservation.destination_peer_id.clone(),
            "task": {
                "cloud_task_id": format!("cloud-{}", reservation.transfer_id),
                "source_peer_id": reservation.source_peer_id.clone(),
                "source_task_id": reservation.source_task_id.clone(),
                "local_task_id": destination_task_id.clone(),
                "resume_session_id": null,
                "prompt": "resume the transferred agent",
                "stage": "in progress",
                "branch": "task-source",
                "head_oid": head_oid.clone(),
                "base_oid": base_oid.clone(),
                "workflow_definition": workflow_definition.clone(),
                "history": [{
                    "sequence": 0,
                    "origin_peer_id": reservation.source_peer_id.clone(),
                    "origin_task_id": reservation.source_task_id.clone(),
                    "origin_run_id": "source-run",
                    "stage": "in progress",
                    "kind": "main",
                    "agent": "implement",
                    "result": "source result",
                    "finished_at": "2026-09-10 11:59:00"
                }],
                "pipeline": "single-reviewer",
                "agent_type": "pty",
                "agent_provider": "claude"
            },
            "repo": {
                "mode": "task-bundle",
                "name": "Acquisition Retry",
                "path": source_repo.to_string_lossy(),
                "remote_url": source_remote.to_string_lossy(),
                "default_branch": "main",
                "bundle": {
                    "artifact_id": "acquisition-bundle",
                    "filename": "transfer.bundle",
                    "ref_name": "refs/heads/task-source",
                    "base_ref_name": "refs/heads/main"
                }
            },
            "input_ledger": {
                "artifact_id": "acquisition-inputs",
                "filename": payload::TASK_INPUT_LEDGER_FILENAME,
                "sha256": payload::sha256_hex(&ledger_bytes),
                "count": 1
            },
            "artifacts": []
        });
        let config = item4_test_config("acquisition-retry-server");
        std::fs::create_dir_all(&config.daemon_dir).unwrap();
        {
            let db = crate::db::Db::open_for_tests(&config.db_path).unwrap();
            db.insert_task_transfer(&crate::db::NewTaskTransfer {
                id: reservation.transfer_id.clone(),
                direction: "incoming".into(),
                status: "pending".into(),
                source_peer_id: Some(reservation.source_peer_id.clone()),
                target_peer_id: Some(reservation.destination_peer_id.clone()),
                source_desktop_id: None,
                target_desktop_id: None,
                source_task_id: Some(reservation.source_task_id.clone()),
                // Model the production producer: acquisition starts unbound;
                // the fenced importing transition binds the destination task.
                local_task_id: None,
                error: None,
                payload_json: Some(payload_json.to_string()),
            })
            .unwrap();
            db.enqueue_transfer_work(
                "import:acquisition-retry",
                super::super::queue::KIND_IMPORT,
                Some(&reservation.transfer_id),
                r#"{"interruptAfterAcquisition":true,"interruptAfterSpawn":true}"#,
            )
            .unwrap();
        }
        let destination = reservation
            .spawn_destination("acquisition-retry-dest")
            .await;
        let state = Arc::new(AppState::with_transfer_sidecar_for_test(
            config.clone(),
            destination,
        ));
        let work = TransferWorkItem {
            id: "import:acquisition-retry".into(),
            kind: super::super::queue::KIND_IMPORT.into(),
            transfer_id: Some(reservation.transfer_id.clone()),
            payload_json: r#"{"interruptAfterAcquisition":true,"interruptAfterSpawn":true}"#.into(),
            attempts: 1,
        };
        // Serve the same durable finalization request that the source server
        // receives. Its transfer ID correlates the pending sidecar waiter with
        // the bundle and ledger staged above; no pre-bound destination ID is
        // needed to skip finalization.
        let finalize = async {
            let request = wait_for_durable_work(
                &reservation.source_work,
                super::super::queue::KIND_FINALIZE,
                std::time::Duration::from_secs(15),
            )
            .await;
            assert_eq!(
                request.transfer_id.as_deref(),
                Some(reservation.transfer_id.as_str())
            );
            let event: Value = serde_json::from_str(&request.payload_json).unwrap();
            assert_eq!(event["transfer_id"], reservation.transfer_id);
            reservation
                .source
                .control(
                    "complete-outgoing-transfer-finalization",
                    serde_json::json!({
                        "transferId": reservation.transfer_id,
                        "payload": payload_json,
                        "finalizedCleanly": true,
                        "error": null,
                    }),
                )
                .await
                .expect("answer the correlated source finalization request");
            reservation
                .source_work
                .open_db()
                .unwrap()
                .complete_transfer_work(&request.id)
                .unwrap();
        };
        let (first, ()) = tokio::join!(
            run_import(&state, &work, &reservation.transfer_id),
            finalize,
        );
        assert!(
            matches!(first, Err(ImportFailure::Retry(ref reason)) if reason.contains("test interruption")),
            "unexpected first attempt: {first:?}"
        );
        let first_binding = state
            .transfer_work()
            .open_db()
            .unwrap()
            .transferred_task_manifest(&reservation.transfer_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            first_binding.3.as_deref(),
            Some(destination_task_id.as_str())
        );
        let acquired_path = state
            .transfer_work()
            .open_db()
            .unwrap()
            .get_repo(&first_binding.0)
            .unwrap()
            .unwrap()
            .path;
        assert_ne!(
            std::fs::canonicalize(&acquired_path).unwrap(),
            std::fs::canonicalize(&source_repo).unwrap(),
            "the regression must exercise a newly allocated destination repository"
        );
        assert_eq!(
            super::super::git::remote_url(Path::new(&acquired_path)).as_deref(),
            Some(source_remote.to_string_lossy().as_ref()),
            "new bundle-backed acquisition must preserve the credential-free origin"
        );
        assert_eq!(
            state
                .transfer_work()
                .open_db()
                .unwrap()
                .list_repos_for_maintenance()
                .unwrap()
                .len(),
            1
        );

        let socket_path = kanna_runtime_defaults::socket_path(Path::new(&config.daemon_dir));
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let daemon_db_path = config.db_path.clone();
        let transfer_id_for_daemon = reservation.transfer_id.clone();
        let task_id_for_daemon = destination_task_id.clone();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let command = read_import_test_daemon_command(&mut reader, &mut write_half).await;
            let session_id = match command {
                kanna_daemon::protocol::Command::Spawn { session_id, .. } => session_id,
                other => panic!("expected recovery Spawn, got {other:?}"),
            };
            let db = crate::db::Db::open(&daemon_db_path).unwrap();
            assert!(
                db.transferred_task_manifest_content_commitment(&transfer_id_for_daemon)
                    .unwrap()
                    .is_some(),
                "Spawn preceded the durable imported-snapshot proof"
            );
            db.record_task_input(
                &task_id_for_daemon,
                crate::db::TaskInputSource::Manager,
                "destination directive after first Spawn",
            )
            .unwrap()
            .expect("record destination-local directive");
            db.update_pipeline_item_stage(&task_id_for_daemon, "review")
                .unwrap();
            let worktree = db
                .get_task_worktree_path(&task_id_for_daemon)
                .unwrap()
                .expect("destination worktree");
            std::fs::write(
                Path::new(&worktree).join("destination-progress.txt"),
                "local\n",
            )
            .unwrap();
            for args in [
                vec!["add", "destination-progress.txt"],
                vec!["commit", "-m", "destination progress after transfer"],
            ] {
                let output = std::process::Command::new("git")
                    .args(args)
                    .current_dir(&worktree)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            write_half
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(&kanna_daemon::protocol::Event::SessionCreated {
                            session_id
                        })
                        .unwrap()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let second = run_import(&state, &work, &reservation.transfer_id).await;
        assert!(
            matches!(second, Err(ImportFailure::Retry(ref reason)) if reason.contains("after verified task spawn")),
            "the injected spawn-to-ack interruption did not fire: {second:?}"
        );
        daemon.await.unwrap();

        // Recreate both destination server state and its sidecar process. The
        // immutable commitment must authorize only the ack replay; it must not
        // compare or erase the destination work committed after Spawn.
        drop(state);
        let restarted_destination = reservation
            .spawn_destination("acquisition-retry-restarted")
            .await;
        let restarted_state = Arc::new(AppState::with_transfer_sidecar_for_test(
            config.clone(),
            restarted_destination,
        ));
        let third = run_import(&restarted_state, &work, &reservation.transfer_id).await;
        assert!(third.is_ok(), "restart did not converge: {third:?}");

        let proof_db = restarted_state.transfer_work().open_db().unwrap();
        let persisted_commitment = proof_db
            .transferred_task_manifest_content_commitment(&reservation.transfer_id)
            .unwrap()
            .expect("persisted commitment");
        let destination_repo_id = proof_db
            .transferred_task_manifest(&reservation.transfer_id)
            .unwrap()
            .unwrap()
            .0;
        drop(proof_db);
        assert_real_ack_values(
            &reservation,
            &destination_task_id,
            &persisted_commitment,
            &destination_repo_id,
        )
        .await;

        let db = restarted_state.transfer_work().open_db().unwrap();
        let second_binding = db
            .transferred_task_manifest(&reservation.transfer_id)
            .unwrap()
            .unwrap();
        assert_eq!(second_binding.0, first_binding.0);
        assert_eq!(second_binding.3, first_binding.3);
        assert_eq!(db.list_repos_for_maintenance().unwrap().len(), 1);
        assert_eq!(
            super::super::git::remote_url(Path::new(&acquired_path)).as_deref(),
            Some(source_remote.to_string_lossy().as_ref()),
            "retry must retain the original destination origin"
        );
        assert!(db
            .get_pipeline_item(&destination_task_id)
            .unwrap()
            .is_some());
        assert_eq!(
            db.get_pipeline_item(&destination_task_id)
                .unwrap()
                .unwrap()
                .stage
                .as_deref(),
            Some("review")
        );
        let inputs = db.list_all_task_inputs(&destination_task_id).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].message, "preserve this imported directive");
        assert!(inputs[0].origin.is_some());
        assert_eq!(inputs[1].message, "destination directive after first Spawn");
        assert!(inputs[1].origin.is_none());
        let history = db.transferred_task_history(&destination_task_id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].origin_run_id, "source-run");
        let destination_worktree = db
            .get_task_worktree_path(&destination_task_id)
            .unwrap()
            .unwrap();
        assert!(Path::new(&destination_worktree)
            .join("destination-progress.txt")
            .exists());
        assert_eq!(
            db.get_task_transfer(&reservation.transfer_id)
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );

        let _ = std::fs::remove_dir_all(destination_home);
        let _ = std::fs::remove_dir_all(source_repo);
        let _ = std::fs::remove_dir_all(source_remote);
    }
}
