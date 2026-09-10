//! Subscription/mailbox semantics are independent of how a harness wakes.
//! One pending page provides backpressure; only a matching acknowledgement
//! advances the durable observation cursor. Wakes never carry directives.
use super::{
    harness_wake, lan_trust::DesktopLocalAccess, subscription_timing, task_events, task_input,
    AppState,
};
use crate::db::{Db, EventSubscription};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

type ApiError = (StatusCode, String);
static NEXT_SUBSCRIPTION: AtomicU64 = AtomicU64::new(0);

fn failure(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn database(state: &AppState) -> Result<Db, String> {
    Db::open(&state.config().db_path).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SubscribeRequest {
    task_id: String,
    repo_id: Option<String>,
    parent_task_id: Option<String>,
    #[serde(default)]
    task_ids: Vec<String>,
    #[serde(default)]
    exclude_task_ids: Vec<String>,
    #[serde(default)]
    local_only: bool,
    #[serde(default)]
    delivery: harness_wake::Delivery,
    #[serde(default)]
    diagnostic: bool,
    /// Additive allow-list, exactly like the public wait's `event_types`: a
    /// query filter reused verbatim, never part of the durable cursor.
    #[serde(default)]
    event_types: Vec<String>,
    /// Additive to the fixed baseline exclusion list below, not a
    /// replacement for it.
    #[serde(default)]
    exclude_event_types: Vec<String>,
    /// Per-subscription override of the collector's trailing-quiet duration.
    /// Omitted keeps the manager-adopted default; see `subscription_timing`.
    quiet_ms: Option<u64>,
    /// Per-subscription override of the collector's max collection hold.
    /// Validated at registration to be at least `quiet_ms`.
    max_hold_ms: Option<u64>,
    /// Per-subscription override of the minimum spacing between adapter-call
    /// admissions (the wake-rate gate, not the collection window).
    min_admission_interval_ms: Option<u64>,
}

fn load(state: &AppState, id: &str) -> Result<EventSubscription, ApiError> {
    database(state)
        .map_err(failure)?
        .event_subscription(id)
        .map_err(failure)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "subscription not found".into()))
}

fn save(state: &AppState, row: &mut EventSubscription) -> Result<bool, String> {
    database(state)?
        .save_event_subscription(row)
        .map_err(|e| e.to_string())
}

fn still_bound(state: &AppState, row: &EventSubscription) -> Result<bool, String> {
    let db = database(state)?;
    let task = db
        .get_pipeline_item(&row.task_id)
        .map_err(|e| e.to_string())?;
    let run = db
        .latest_stage_run(&row.task_id)
        .map_err(|e| e.to_string())?;
    Ok(task.is_some_and(|task| {
        task.closed_at.is_none() && task.stage == row.stage && task.branch == row.branch
    }) && run.is_some_and(|run| run.id == row.run_id))
}

/// Selection belongs to the wait, before batching. The returned page is the
/// mailbox record, including its checkpoint through excluded events.
async fn collect(
    state: Arc<AppState>,
    row: &EventSubscription,
    timeout: u64,
    collection: Arc<std::sync::Mutex<subscription_timing::Collection>>,
) -> Result<Value, String> {
    let mut query = row.query.clone();
    query["timeoutSecs"] = json!(timeout);
    if let Some(cursor) = &row.cursor {
        query["cursor"] = json!(cursor);
    }
    task_events::wait_subscription_events(state, query, collection).await
}

/// A fresh, subscription-scoped collector seeded from the row's own
/// (already-validated) quiet/max-hold overrides, falling back to the
/// manager-adopted defaults when absent.
fn fresh_collection(
    row: &EventSubscription,
) -> Arc<std::sync::Mutex<subscription_timing::Collection>> {
    Arc::new(std::sync::Mutex::new(
        subscription_timing::Collection::from_query(
            row.query.get("quietMs").and_then(Value::as_u64),
            row.query.get("maxHoldMs").and_then(Value::as_u64),
        ),
    ))
}

/// Query/body flag shared by every subscription endpoint: agent-facing callers
/// get the compact response by default; a diagnostic caller opts into the full
/// internal row (durable cursor, query, revision, and so on) explicitly.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct DiagnosticQuery {
    #[serde(default)]
    diagnostic: bool,
}

/// The durable `cursor` (top-level and inside `pending`) and the full `query`
/// are internal replay/observation plumbing an MCP or CLI caller never needs:
/// acknowledgement advances by `batchId` alone. The compact response keeps
/// only what an agent acts on — the batch's events, capacity/fault signals,
/// and the subscription's lifecycle fields — plus its watched scope with any
/// cursor-shaped key stripped for defense in depth.
fn compact(row: &EventSubscription) -> Value {
    let mut scope = row.query.clone();
    if let Some(object) = scope.as_object_mut() {
        object.remove("cursor");
    }
    let pending = row.pending.as_ref().map(|batch| {
        json!({
            "events": batch["events"],
            "hasMore": batch["hasMore"],
            "waitOutcome": batch["waitOutcome"],
            "machineErrors": batch["machineErrors"],
            "watchError": batch.get("watchError"),
        })
    });
    json!({
        "id": row.id,
        "active": row.active,
        "error": row.error,
        "wakeState": row.wake_state,
        "batchId": row.batch_id,
        "pending": pending,
        "query": scope,
    })
}

fn response(row: &EventSubscription, diagnostic: bool) -> Value {
    if diagnostic {
        json!(row)
    } else {
        compact(row)
    }
}

fn accept_page(row: &mut EventSubscription, mut batch: Value, observed: bool) {
    if batch["machineErrors"]
        .as_array()
        .is_some_and(|errors| !errors.is_empty())
    {
        batch["watchError"] = json!("Some machines could not be observed; reconcile the reported gaps and resubscribe after recovery.");
    }
    if batch.get("watchError").is_some()
        || batch["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    {
        row.batch_id += 1;
        row.pending = Some(batch);
        row.wake_state = if observed { "observed" } else { "pending" }.into();
    } else {
        row.cursor = batch["cursor"].as_str().map(str::to_owned);
    }
}

pub(super) async fn subscribe(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<SubscribeRequest>,
) -> Result<Json<Value>, ApiError> {
    let db = database(&state).map_err(failure)?;
    let owner = db
        .resolve_pipeline_item_id(&request.task_id)
        .map_err(failure)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "subscriber task not found".into()))?;
    let _mutation = state
        .try_begin_requested_task_mutation(&owner)
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "subscriber is changing; retry subscription".into(),
            )
        })?;
    let task = db
        .get_pipeline_item(&owner)
        .map_err(failure)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "subscriber task not found".into()))?;
    let run = db
        .latest_stage_run(&owner)
        .map_err(failure)?
        .ok_or_else(|| (StatusCode::CONFLICT, "subscriber has no stage run".into()))?;
    if task.closed_at.is_some() {
        return Err((StatusCode::CONFLICT, "subscriber is closed".into()));
    }
    if request.delivery == harness_wake::Delivery::CodexAppServer
        && run.agent_provider.as_deref() != Some("codex")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "codex_app_server delivery requires a Codex manager run".into(),
        ));
    }
    request.task_ids.sort();
    request.task_ids.dedup();
    request.exclude_task_ids.push(owner.clone());
    request.exclude_task_ids.sort();
    request.exclude_task_ids.dedup();
    let scopes = usize::from(request.repo_id.is_some())
        + usize::from(request.parent_task_id.is_some())
        + usize::from(!request.task_ids.is_empty());
    if scopes > 1 {
        return Err((StatusCode::BAD_REQUEST, "choose one event scope".into()));
    }
    // Validate on the resolved (default-filled) values, since those are what
    // actually govern the collector, but persist only the caller's explicit
    // overrides below — an untouched request keeps the exact query shape a
    // pre-existing row has, so registration-retry equality is unaffected.
    let quiet_ms = request
        .quiet_ms
        .unwrap_or(subscription_timing::QUIET.as_millis() as u64);
    let max_hold_ms = request
        .max_hold_ms
        .unwrap_or(subscription_timing::MAX_HOLD.as_millis() as u64);
    let min_admission_interval_ms = request
        .min_admission_interval_ms
        .unwrap_or(subscription_timing::ADMISSION_INTERVAL.as_millis() as u64);
    let floor_ms = subscription_timing::MIN_OVERRIDE.as_millis() as u64;
    if quiet_ms < floor_ms || max_hold_ms < floor_ms || min_admission_interval_ms < floor_ms {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("quiet_ms, max_hold_ms and min_admission_interval_ms must each be at least {floor_ms}ms"),
        ));
    }
    if max_hold_ms < quiet_ms {
        return Err((
            StatusCode::BAD_REQUEST,
            "max_hold_ms must be at least quiet_ms".into(),
        ));
    }
    // No ceiling by policy. `Duration::from_millis` accepts any `u64`, and so
    // does the `Instant + Duration` arithmetic these values feed into
    // (`Collection::deadline`, `Admission`): a `u64` millisecond count can
    // never exceed `Duration`'s own (far larger) capacity, so there is no
    // reachable overflow to guard against here — confirmed empirically
    // (`Instant::now().checked_add(Duration::from_millis(u64::MAX))` never
    // returns `None`), not just assumed.
    // Additive to the fixed baseline, so an untouched request produces the
    // exact same string as before.
    let mut exclude_event_types = vec![
        "task.activity_changed".to_string(),
        "task.runtime_settled".to_string(),
        "task.input_delivered".to_string(),
    ];
    if !request.exclude_event_types.is_empty() {
        exclude_event_types.extend(request.exclude_event_types.iter().cloned());
        exclude_event_types.sort();
        exclude_event_types.dedup();
    }
    let mut query = json!({
        "from": "now", "includeCurrentActivity": true, "shortCursor": false,
        "localOnly": request.local_only, "excludeTaskIds": request.exclude_task_ids.join(","),
        "excludeEventTypes": exclude_event_types.join(","),
        "limit": 100,
    });
    if !request.task_ids.is_empty() {
        query["taskIds"] = json!(request.task_ids.join(","));
    } else if let Some(parent) = request.parent_task_id {
        query["parentTaskId"] = json!(parent);
    } else {
        query["repoId"] = json!(request.repo_id.unwrap_or(task.repo_id));
    }
    if !request.event_types.is_empty() {
        let mut event_types = request.event_types.clone();
        event_types.sort();
        event_types.dedup();
        query["eventTypes"] = json!(event_types.join(","));
    }
    if request.quiet_ms.is_some() {
        query["quietMs"] = json!(quiet_ms);
    }
    if request.max_hold_ms.is_some() {
        query["maxHoldMs"] = json!(max_hold_ms);
    }
    if request.min_admission_interval_ms.is_some() {
        query["minAdmissionIntervalMs"] = json!(min_admission_interval_ms);
    }
    // A registration retry reuses the mailbox and never resets its cursor.
    if let Some(existing) = db
        .event_subscriptions()
        .map_err(failure)?
        .into_iter()
        .find(|row| row.active && row.task_id == owner && row.run_id == run.id)
    {
        if existing.query != query || existing.delivery != request.delivery.as_str() {
            return Err((StatusCode::CONFLICT, "subscriber already has a different subscription; unsubscribe it before changing scope or delivery".into()));
        }
        return Ok(Json(response(&existing, request.diagnostic)));
    }
    // Retrying a paused watch preserves its observation position. Discarding
    // that position requires an explicit unsubscribe, never a registration
    // retry that silently starts again at `now`.
    if let Some(mut existing) = db
        .event_subscriptions()
        .map_err(failure)?
        .into_iter()
        .rev()
        .find(|row| {
            !row.active
                && row.error.is_some()
                && row.wake_state != "stopped"
                && row.task_id == owner
                && row.run_id == run.id
                && row.stage == task.stage
                && row.branch == task.branch
                && row.query == query
                && row.delivery == request.delivery.as_str()
        })
    {
        existing.active = true;
        existing.error = None;
        if !save(&state, &mut existing).map_err(failure)? {
            return Err((
                StatusCode::CONFLICT,
                "subscription changed; retry registration".into(),
            ));
        }
        state.event_subscriptions_changed.notify_waiters();
        return Ok(Json(response(&existing, request.diagnostic)));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(failure)?
        .as_nanos();
    let mut row = EventSubscription {
        id: format!(
            "watch-{nanos}-{}",
            NEXT_SUBSCRIPTION.fetch_add(1, Ordering::Relaxed)
        ),
        task_id: owner,
        run_id: run.id,
        stage: task.stage,
        branch: task.branch,
        query,
        delivery: request.delivery.as_str().into(),
        revision: 0,
        cursor: None,
        pending: None,
        batch_id: 0,
        wake_state: "idle".into(),
        error: None,
        active: true,
        wake_admitted: false,
    };
    drop(db);
    // A single zero-timeout bootstrap check: whatever is already settled,
    // never a wait, so there is nothing here for a chained collection to own.
    let batch = collect(state.clone(), &row, 0, fresh_collection(&row))
        .await
        .map_err(failure)?;
    accept_page(&mut row, batch, true);
    database(&state)
        .map_err(failure)?
        .insert_event_subscription(&row)
        .map_err(failure)?;
    state.event_subscriptions_changed.notify_waiters();
    Ok(Json(response(&row, request.diagnostic)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReadRequest {
    acknowledge_batch_id: Option<i64>,
    #[serde(default)]
    diagnostic: bool,
}

pub(super) async fn read(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<ReadRequest>,
) -> Result<Json<Value>, ApiError> {
    let mut row = load(&state, &id)?;
    if let Some(batch_id) = request.acknowledge_batch_id {
        if batch_id != row.batch_id {
            return Err((
                StatusCode::CONFLICT,
                "batch changed; read before acknowledging".into(),
            ));
        }
        if let Some(batch) = row.pending.take() {
            row.cursor = batch["cursor"].as_str().map(str::to_owned);
            row.wake_state = "idle".into();
            row.error = batch["watchError"].as_str().map(str::to_owned);
            if row.error.is_some() {
                row.active = false;
            }
            if !save(&state, &mut row).map_err(failure)? {
                return Err((
                    StatusCode::CONFLICT,
                    "subscription changed; read again".into(),
                ));
            }
            state.event_subscriptions_changed.notify_waiters();
        }
    }
    Ok(Json(response(&row, request.diagnostic)))
}

pub(super) async fn unsubscribe(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<DiagnosticQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut row = load(&state, &id)?;
    row.active = false;
    row.wake_state = "stopped".into();
    if !save(&state, &mut row).map_err(failure)? {
        return Err((
            StatusCode::CONFLICT,
            "subscription changed; retry unsubscribe".into(),
        ));
    }
    state.event_subscriptions_changed.notify_waiters();
    Ok(Json(response(&row, query.diagnostic)))
}

/// What one worker iteration decided about the subscription's lifetime.
enum Step {
    /// This worker owns nothing more: the row is gone, inactive, or unbound.
    Stop,
    /// Observation continues; run another iteration.
    Iterate,
}

async fn work(
    state: Arc<AppState>,
    id: String,
    admission: &mut super::subscription_timing::Admission,
) -> Result<(), String> {
    let mut changes = state.subscribe_state_changes();
    loop {
        let mut changed = Box::pin(state.event_subscriptions_changed.notified());
        changed.as_mut().enable();
        let step = step(&state, &id, changed.as_mut(), &mut changes, admission).await;
        match step {
            Ok(Step::Stop) => return Ok(()),
            Ok(Step::Iterate) => {}
            // Storage faults are this machine being busy — a loaded disk, a
            // writer holding SQLite past the busy timeout — not a decision to
            // stop watching. Keep the row active and retry on the next
            // notification rather than deactivating an observer nobody would
            // be woken to replace.
            Err(error) => {
                log::warn!("event subscription {id} deferred after a storage fault: {error}");
                tokio::select! { _ = changed.as_mut() => {}, _ = changes.recv() => {} }
            }
        }
    }
}

async fn step(
    state: &Arc<AppState>,
    id: &str,
    mut changed: std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>,
    changes: &mut tokio::sync::broadcast::Receiver<kanna_agent_protocol::ServerFrame>,
    admission: &mut super::subscription_timing::Admission,
) -> Result<Step, String> {
    let Some(mut row) = database(state)?
        .event_subscription(id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(Step::Stop);
    };
    if !row.active {
        return Ok(Step::Stop);
    }
    if !still_bound(state, &row)? {
        row.active = false;
        row.error = Some("subscriber stage/session was replaced or closed".into());
        save(state, &mut row)?;
        return Ok(Step::Stop);
    }
    if row.pending.is_some() {
        if row.wake_state == "pending" && row.delivery == "poll" {
            row.wake_state = "ready".into();
            if !save(state, &mut row)? {
                return Ok(Step::Iterate);
            }
        } else if row.wake_state == "pending" {
            if let Some(deadline) = admission.deadline() {
                // Expiry is a scheduled admission, not a transport retry.
                // Re-enter step to reload the row and binding before CAS: ack,
                // retirement or replacement may have invalidated this page.
                tokio::select! {
                    _ = changed.as_mut() => {},
                    _ = changes.recv() => {},
                    _ = tokio::time::sleep_until(deadline) => {},
                }
                return Ok(Step::Iterate);
            }
            row.wake_state = "sending".into();
            row.wake_admitted = true;
            if !save(state, &mut row)? {
                return Ok(Step::Iterate);
            }
            admission.admitted();
            #[cfg(test)]
            if let Some(events) = &state.subscription_test_events {
                let _ = events.send(super::subscription_timing::TestEvent::Admitted(
                    row.batch_id,
                    tokio::time::Instant::now(),
                ));
            }
            let result = harness_wake::deliver(state.clone(), &row).await;
            #[cfg(test)]
            if let Some(barrier) = &state.subscription_delivery_barrier {
                if let Some(events) = &state.subscription_test_events {
                    let _ = events.send(super::subscription_timing::TestEvent::Delivered);
                }
                if let Ok(permit) = barrier.acquire().await {
                    permit.forget();
                }
            }
            match result {
                Ok(outcome) => {
                    row.wake_state = outcome.into();
                    row.error = None;
                }
                // Nothing reached the daemon and the cause clears itself — a
                // daemon handoff, an unreadable daemon, a held mutation lease.
                // Stay `pending` so the next notification re-attempts it; the
                // retained text says why the last try failed without ever
                // claiming the batch was delivered.
                Err(failure) if failure.retry == task_input::DeliveryRetry::Transient => {
                    row.wake_state = "pending".into();
                    row.error = Some(failure.message);
                }
                Err(failure) => {
                    row.wake_state = "error".into();
                    row.error = Some(failure.message);
                }
            }
            save(state, &mut row)?;
        } else if row.wake_state == "sending" {
            // A server died after reserving delivery. Preserve the page;
            // sending another wake could submit a second turn.
            row.wake_state = "uncertain".into();
            row.error =
                Some("server restarted during wake delivery; mailbox remains readable".into());
            save(state, &mut row)?;
        }
        tokio::select! { _ = changed.as_mut() => {}, _ = changes.recv() => {} }
        return Ok(Step::Iterate);
    }
    // A state notification asks us to revalidate the owner, not to replace
    // its observation. Dropping collect also drops the aggregate's retained
    // peer legs, but an admitted relay request keeps running on the peer.
    // Recreating that wait for every local state edge exhausts its long-poll
    // permits even with only one subscription.
    //
    // One native call is capped at MAX_WAIT_TIMEOUT_SECS (240s) regardless of
    // this subscription's own quiet/max-hold window, which can exceed it
    // (defaults are 300s each). `collection` is shared across every chained
    // call below, so the true first relevant observation — and hence the
    // subscription's own deadline — survives across calls instead of
    // resetting each time a call returns merely because its own native
    // receiver expired. A native "events" outcome means the subscription's
    // own criteria (urgent, full page, or quiet/max-hold reached) were
    // genuinely satisfied; "timeout" means only that one call's own budget
    // ran out, so the chain continues with the advanced cursor.
    let collection = fresh_collection(&row);
    let mut working_cursor = row.cursor.clone();
    // Each native call's own `events`/page-capacity accounting starts fresh
    // (its `collected` local is empty and its own `limit` is the full page
    // size), so a chain of calls that each return fewer than a page would
    // otherwise both drop every timed-out call's already-observed events (the
    // native cursor already advanced past them) and let each call fill up to
    // a full page of its own, overrunning the subscription's real capacity.
    // Retain every relevant event this chain has actually observed here, and
    // shrink each subsequent call's own limit by that count so the chain's
    // total never exceeds one page.
    let mut retained_events: Vec<Value> = Vec::new();
    let batch = 'chain: loop {
        let native_timeout = {
            let guard = collection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.intrinsic_deadline() {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    remaining
                        .as_secs()
                        .saturating_add(u64::from(remaining.subsec_nanos() > 0))
                        .clamp(1, kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS)
                }
                None => kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS,
            }
        };
        let mut call_row = row.clone();
        call_row.cursor = working_cursor.clone();
        if !retained_events.is_empty() {
            if let Some(limit) = call_row.query.get("limit").and_then(Value::as_i64) {
                call_row.query["limit"] = json!((limit - retained_events.len() as i64).max(1));
            }
        }
        let native_batch = {
            let collection_call =
                collect(state.clone(), &call_row, native_timeout, collection.clone());
            tokio::pin!(collection_call);
            loop {
                let mut notification = Box::pin(state.event_subscriptions_changed.notified());
                notification.as_mut().enable();
                let current = database(state)?
                    .event_subscription(id)
                    .map_err(|e| e.to_string())?;
                if !current.is_some_and(|current| {
                    current.active
                        && current.revision == row.revision
                        && current.pending.is_none()
                        && current.query == row.query
                        && current.cursor == row.cursor
                }) || !still_bound(state, &row)?
                {
                    // Genuine retirement or a changed mailbox invalidates this
                    // collect. There is no relay cancellation protocol: at most
                    // one abandoned leg per peer per retirement finishes at its
                    // receiver's own deadline (MAX_WAIT_TIMEOUT_SECS). Repeated
                    // retirements can overlap those holds; this is not a bound on
                    // all historical requests from the subscription. Never put a
                    // cancelled aggregate back in the registry: it may already
                    // have advanced checkpoints for events not yet in the mailbox.
                    return Ok(Step::Iterate);
                }
                tokio::select! {
                    _ = notification => {},
                    _ = changes.recv() => {},
                    batch = &mut collection_call => break batch,
                }
            }
        };
        match native_batch {
            Ok(mut batch) => {
                working_cursor = batch["cursor"].as_str().map(str::to_owned);
                if let Some(events) = batch["events"].as_array() {
                    retained_events.extend(events.iter().cloned());
                }
                let machine_errors_present = batch["machineErrors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty());
                if batch["waitOutcome"] == "timeout" && !machine_errors_present {
                    // Whether this leg's own timeout also means the
                    // subscription is genuinely done cannot be decided from
                    // how this call's timeout was originally sized: a later
                    // relevant event observed mid-call can push the live
                    // Collection's intrinsic deadline further out (quiet is
                    // anchored to the latest observation), so a call sized to
                    // the deadline as it stood at dispatch can still return
                    // "timeout" well before the subscription's now-later
                    // deadline. Re-read the live collection here, after the
                    // call, rather than trusting a pre-call snapshot.
                    let live_deadline_reached = {
                        let guard = collection
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard
                            .intrinsic_deadline()
                            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                    };
                    if !live_deadline_reached {
                        #[cfg(test)]
                        subscription_timing::leg_timed_out(state);
                        continue 'chain;
                    }
                }
                batch["events"] = json!(std::mem::take(&mut retained_events));
                break 'chain Ok(batch);
            }
            Err(error) => break 'chain Err(error),
        }
    };
    match batch {
        Ok(batch) => accept_page(&mut row, batch, false),
        Err(error) => {
            let batch = json!({"events": [], "cursor": row.cursor,
                "watchError": format!("event watch stopped: {error}; reconcile current state before establishing a new subscription")});
            accept_page(&mut row, batch, false);
        }
    }
    save(state, &mut row)?;
    Ok(Step::Iterate)
}

/// Lifecycle-owned workers; dropping the service aborts its own workers.
pub(crate) async fn run(state: Arc<AppState>) {
    let mut workers = tokio::task::JoinSet::new();
    let mut running = HashMap::new();
    // Keep monotonic pacing across pause/same-id recovery in this service.
    // On process/service recovery rearm at most one cooldown, including old
    // rows without the additive hint. Fresh registration remains immediate.
    let mut admission_clocks: HashMap<String, super::subscription_timing::Admission> =
        HashMap::new();
    let mut recovering = true;
    loop {
        let mut changed = Box::pin(state.event_subscriptions_changed.notified());
        changed.as_mut().enable();
        match database(&state).and_then(|db| db.event_subscriptions().map_err(|e| e.to_string())) {
            Ok(rows) => {
                admission_clocks.retain(|id, _| rows.iter().any(|row| &row.id == id));
                for row in rows.into_iter().filter(|row| row.active) {
                    if running.contains_key(&row.id) {
                        continue;
                    }
                    let worker_id = row.id.clone();
                    let worker_state = state.clone();
                    let mut admission = admission_clocks.remove(&row.id).unwrap_or_else(|| {
                        let interval =
                            super::subscription_timing::Admission::interval_from_query(&row.query);
                        super::subscription_timing::Admission::new(
                            recovering || row.wake_admitted,
                            interval,
                        )
                    });
                    let handle = workers.spawn(async move {
                        let result = work(worker_state, row.id.clone(), &mut admission).await;
                        (row.id, result, admission)
                    });
                    running.insert(worker_id, handle.id());
                }
                recovering = false;
            }
            Err(error) => log::error!("event subscription recovery failed: {error}"),
        }
        tokio::select! {
            _ = changed => {},
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                match result {
                    Ok((id, result, admission)) => {
                        running.remove(&id);
                        admission_clocks.insert(id.clone(), admission);
                        if let Err(error) = result {
                            log::error!("event subscription {id} failed: {error}");
                            if let Ok(mut row) = load(&state, &id) {
                                row.active = false;
                                row.error = Some(error);
                                if let Err(error) = save(&state, &mut row) { log::error!("subscription failure persistence: {error}"); }
                            }
                        }
                    },
                    Err(error) => {
                        log::error!("event subscription worker panicked: {error}");
                        let id = running.iter().find(|(_, worker)| **worker == error.id()).map(|(id, _)| id.clone());
                        if let Some(id) = id {
                            running.remove(&id);
                            if let Ok(mut row) = load(&state, &id) {
                                row.active = false;
                                row.error = Some(format!("subscription worker stopped: {error}"));
                                if let Err(error) = save(&state, &mut row) { log::error!("subscription failure persistence: {error}"); }
                            }
                        }
                    },
                }
            }
        }
    }
}
