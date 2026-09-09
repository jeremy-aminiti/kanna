//! Subscription/mailbox semantics are independent of how a harness wakes.
//! One pending page provides backpressure; only a matching acknowledgement
//! advances the durable observation cursor. Wakes never carry directives.
use super::{harness_wake, lan_trust::DesktopLocalAccess, task_events, task_input, AppState};
use crate::db::{Db, EventSubscription};
use axum::{
    extract::{Path, State},
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
) -> Result<Value, String> {
    let mut query = row.query.clone();
    query["timeoutSecs"] = json!(timeout);
    if let Some(cursor) = &row.cursor {
        query["cursor"] = json!(cursor);
    }
    task_events::wait_subscription_events(state, query).await
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
    let mut query = json!({
        "from": "now", "includeCurrentActivity": true, "shortCursor": false,
        "localOnly": request.local_only, "excludeTaskIds": request.exclude_task_ids.join(","),
        "excludeEventTypes": "task.activity_changed,task.runtime_settled,task.input_delivered",
        "limit": 100,
    });
    if !request.task_ids.is_empty() {
        query["taskIds"] = json!(request.task_ids.join(","));
    } else if let Some(parent) = request.parent_task_id {
        query["parentTaskId"] = json!(parent);
    } else {
        query["repoId"] = json!(request.repo_id.unwrap_or(task.repo_id));
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
        return Ok(Json(json!(existing)));
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
        return Ok(Json(json!(existing)));
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
    };
    drop(db);
    let batch = collect(state.clone(), &row, 0).await.map_err(failure)?;
    accept_page(&mut row, batch, true);
    database(&state)
        .map_err(failure)?
        .insert_event_subscription(&row)
        .map_err(failure)?;
    state.event_subscriptions_changed.notify_waiters();
    Ok(Json(json!(row)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReadRequest {
    acknowledge_batch_id: Option<i64>,
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
    Ok(Json(json!(row)))
}

pub(super) async fn unsubscribe(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
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
    Ok(Json(json!(row)))
}

/// What one worker iteration decided about the subscription's lifetime.
enum Step {
    /// This worker owns nothing more: the row is gone, inactive, or unbound.
    Stop,
    /// Observation continues; run another iteration.
    Iterate,
}

async fn work(state: Arc<AppState>, id: String) -> Result<(), String> {
    let mut changes = state.subscribe_state_changes();
    loop {
        let mut changed = Box::pin(state.event_subscriptions_changed.notified());
        changed.as_mut().enable();
        let step = step(&state, &id, changed.as_mut(), &mut changes).await;
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
            row.wake_state = "sending".into();
            if !save(state, &mut row)? {
                return Ok(Step::Iterate);
            }
            let result = harness_wake::deliver(state.clone(), &row).await;
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
    let batch = {
        let collection = collect(
            state.clone(),
            &row,
            kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS,
        );
        tokio::pin!(collection);
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
                batch = &mut collection => break batch,
            }
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
    loop {
        let mut changed = Box::pin(state.event_subscriptions_changed.notified());
        changed.as_mut().enable();
        match database(&state).and_then(|db| db.event_subscriptions().map_err(|e| e.to_string())) {
            Ok(rows) => {
                for row in rows.into_iter().filter(|row| row.active) {
                    if running.contains_key(&row.id) {
                        continue;
                    }
                    let worker_id = row.id.clone();
                    let worker_state = state.clone();
                    let handle = workers.spawn(async move {
                        let result = work(worker_state, row.id.clone()).await;
                        (row.id, result)
                    });
                    running.insert(worker_id, handle.id());
                }
            }
            Err(error) => log::error!("event subscription recovery failed: {error}"),
        }
        tokio::select! {
            _ = changed => {},
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                match result {
                    Ok((id, result)) => {
                        running.remove(&id);
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
