use super::state::{db_write_error, AppState};
use super::task_blockers::{
    persist_resolved_task_blockers, persist_task_blocker_rows, resolve_task_blocker_ids,
};
use crate::db::Db;
use crate::mobile_api::MobileApi;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use kanna_agent_protocol::StateChangeScope;
use kanna_tool_catalog::encode_path_segment;
use std::sync::Arc;

const DEFAULT_RECENT_TASK_LIMIT: u32 = 50;
const MAX_RECENT_TASK_LIMIT: u32 = 200;

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum TaskRuntimeState {
    Busy,
    Waiting,
    Idle,
    Exited,
}

impl TaskRuntimeState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Exited => "exited",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum TaskSort {
    #[default]
    UpdatedAt,
    CreatedAt,
}

impl TaskSort {
    fn as_str(self) -> &'static str {
        match self {
            Self::UpdatedAt => "updatedAt",
            Self::CreatedAt => "createdAt",
        }
    }

    fn db_sort(self) -> crate::db::TaskListSort {
        match self {
            Self::UpdatedAt => crate::db::TaskListSort::UpdatedAt,
            Self::CreatedAt => crate::db::TaskListSort::CreatedAt,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskSortOrder {
    Asc,
    #[default]
    Desc,
}

impl TaskSortOrder {
    fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    fn db_order(self) -> crate::db::TaskListOrder {
        match self {
            Self::Asc => crate::db::TaskListOrder::Asc,
            Self::Desc => crate::db::TaskListOrder::Desc,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskListScope {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    machine_ids: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTasksResponse {
    tasks: Vec<crate::mobile_api::TaskSummary>,
    scope: TaskListScope,
    runtime_state: Option<String>,
    include_closed: bool,
    sort_by: String,
    order: String,
    limit: u32,
    truncated: bool,
    machine_errors: Vec<serde_json::Value>,
}

pub(super) async fn get_tasks(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<GetTasksQuery>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {e}"),
        )
    })?;
    if crate::mobile_api::record_orphaned_initialized_tasks(&db)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        state.publish_state_changed(StateChangeScope::Tasks);
    }
    let repo_id = task_listing_repo_filter(
        query.repo_id.as_deref(),
        query.all_repos,
        query.all_machines,
    )?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RECENT_TASK_LIMIT)
        .clamp(1, MAX_RECENT_TASK_LIMIT);
    let runtime_state = query.runtime_state.map(TaskRuntimeState::as_str);
    let api = MobileApi::new(state.config.clone(), db);
    let (tasks, truncated) = api
        .get_tasks(
            query.include_closed,
            repo_id,
            runtime_state,
            query.sort_by.db_sort(),
            query.order.db_order(),
            limit,
        )
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let response = GetTasksResponse {
        tasks,
        scope: TaskListScope {
            kind: if repo_id.is_some() {
                "repository"
            } else {
                "machine"
            }
            .to_string(),
            repo_id: repo_id.map(str::to_string),
            machine_ids: vec![state.config.desktop_id.clone()],
        },
        runtime_state: runtime_state.map(str::to_string),
        include_closed: query.include_closed,
        sort_by: query.sort_by.as_str().to_string(),
        order: query.order.as_str().to_string(),
        limit,
        truncated,
        machine_errors: Vec::new(),
    };
    if !query.all_machines {
        return Ok(Json(
            serde_json::to_value(response).expect("serialize task list"),
        ));
    }

    aggregate_get_tasks(
        &state,
        response,
        task_listing_remote_path(
            "/v1/tasks",
            &[
                ("includeClosed", query.include_closed.to_string()),
                ("allMachines", "false".to_string()),
                ("allRepos", "true".to_string()),
                ("sortBy", query.sort_by.as_str().to_string()),
                ("order", query.order.as_str().to_string()),
                ("limit", limit.to_string()),
            ],
            runtime_state.map(|state| ("runtimeState", state)),
        ),
        query.sort_by,
        query.order,
    )
    .await
}

pub(super) async fn list_recent_tasks(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListTasksQuery>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    if crate::mobile_api::record_orphaned_initialized_tasks(&db)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        state.publish_state_changed(StateChangeScope::Tasks);
    }
    let api = MobileApi::new(state.config.clone(), db);
    let repo_id = task_listing_repo_filter(
        query.repo_id.as_deref(),
        query.all_repos,
        query.all_machines,
    )?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RECENT_TASK_LIMIT)
        .clamp(1, MAX_RECENT_TASK_LIMIT);
    let tasks = api
        .list_recent_tasks_including_closed(query.include_closed, repo_id, limit)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !query.all_machines {
        return Ok(Json(serde_json::json!(tasks)));
    }
    aggregate_task_summaries(
        &state,
        tasks,
        task_listing_remote_path(
            "/v1/tasks/recent",
            &[
                ("includeClosed", query.include_closed.to_string()),
                ("allMachines", "false".to_string()),
                ("allRepos", query.all_repos.to_string()),
                ("limit", limit.to_string()),
            ],
            repo_id.map(|repo_id| ("repoId", repo_id)),
        ),
    )
    .await
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ClosedTaskIdentitiesResponse {
    tasks: Vec<crate::db::ClosedTaskIdentity>,
}

pub(super) async fn list_closed_task_identities(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ClosedTaskIdentitiesResponse>, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    let tasks = db
        .list_closed_task_identities()
        .map_err(|e| db_write_error("db error", e))?;
    Ok(Json(ClosedTaskIdentitiesResponse { tasks }))
}

pub(super) async fn get_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<GetTaskQuery>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    if crate::mobile_api::record_orphaned_initialized_tasks(&db)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        state.publish_state_changed(StateChangeScope::Tasks);
    }
    let api = MobileApi::new(state.config.clone(), db);
    let task = api
        .get_task(&task_id)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let Some(mut task) = task else {
        if !query.local_only && state.desktop_routing_available() {
            if let Ok(machine_ids) = state.list_active_relay_desktops().await {
                for machine_id in machine_ids {
                    if machine_id == state.config.desktop_id {
                        continue;
                    }
                    let encoded_task_id = encode_path_segment(&task_id);
                    let path = format!("/v1/tasks/{encoded_task_id}?localOnly=true");
                    if let Ok(response) = state
                        .invoke_relay_desktop(
                            machine_id.clone(),
                            "GET".to_string(),
                            path,
                            serde_json::Value::Null,
                        )
                        .await
                    {
                        if response.status == axum::http::StatusCode::OK.as_u16() {
                            return Err((
                                axum::http::StatusCode::NOT_FOUND,
                                format!(
                                    "task {task_id} was found on machine {machine_id}; pass machine_id: \"{machine_id}\" to kanna_get_task"
                                ),
                            ));
                        }
                    }
                }
            }
        }
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            format!("task not found: {task_id}"),
        ));
    };
    if query.agent_view
        && task
            .composer
            .as_ref()
            .is_some_and(|composer| composer.attestation != "typed")
    {
        task.composer = None;
    }
    if !query.brief {
        return Ok(Json(task).into_response());
    }
    let mut value = serde_json::to_value(task)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    brief_task_detail(&mut value, &state.config.desktop_id);
    Ok(Json(value).into_response())
}

/// Keep diagnostics intact; only known bulky/static fields are omitted. Using
/// the ordinary detail as the source preserves unknown/null runtime and git facts.
fn brief_task_detail(value: &mut serde_json::Value, machine_id: &str) {
    let Some(detail) = value.as_object_mut() else {
        return;
    };
    for key in ["prompt", "workflowDefinition", "ports", "pipelineName"] {
        detail.remove(key);
    }
    detail.insert("view".into(), serde_json::json!("brief"));
    detail.insert("briefVersion".into(), serde_json::json!(1));
    detail.insert("machineId".into(), serde_json::json!(machine_id));
    bound_brief_text(detail, "title", "titleTruncated", 200);
    if let Some(run) = detail
        .get_mut("latestRun")
        .and_then(serde_json::Value::as_object_mut)
    {
        bound_brief_text(run, "summary", "summaryTruncated", 1000);
    }
}

fn bound_brief_text(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    flag: &str,
    limit: usize,
) {
    let mut truncated = false;
    if let Some(serde_json::Value::String(text)) = object.get_mut(key) {
        if let Some((end, _)) = text.char_indices().nth(limit) {
            text.truncate(end);
            truncated = true;
        }
    }
    object.insert(flag.into(), serde_json::json!(truncated));
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetTaskQuery {
    #[serde(default)]
    brief: bool,
    #[serde(default)]
    local_only: bool,
    /// Agent tools must not receive provider-authored composer suggestions as
    /// content. Human UI callers omit this and keep the full visual state.
    #[serde(default)]
    agent_view: bool,
}

pub(super) async fn get_task_children(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<Vec<crate::mobile_api::TaskChild>>, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    let api = MobileApi::new(state.config.clone(), db);
    let children = api
        .list_task_children(&task_id)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("task not found: {task_id}"),
            )
        })?;
    Ok(Json(children))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskInputsQuery {
    tail: Option<i64>,
}

/// Enough history for any real task, and a ceiling so one pathological task
/// cannot make the route unbounded. `total` reports what was left out.
const DEFAULT_TASK_INPUT_TAIL: i64 = 100;
const MAX_TASK_INPUT_TAIL: i64 = 500;

/// The task's durable instruction history — what was said to its agent from
/// outside its session, and when. A review stage or dispatcher reads this
/// before claiming anything about what was or was not instructed: it runs in a
/// forked worktree with a fresh session, so the live terminal those messages
/// were typed into is not something it can see.
pub(super) async fn get_task_inputs(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<TaskInputsQuery>,
) -> Result<Json<crate::mobile_api::TaskInputs>, (axum::http::StatusCode, String)> {
    let tail = query
        .tail
        .unwrap_or(DEFAULT_TASK_INPUT_TAIL)
        .clamp(1, MAX_TASK_INPUT_TAIL);
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    let api = MobileApi::new(state.config.clone(), db);
    let inputs = api
        .list_task_inputs(&task_id, tail)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("task not found: {task_id}"),
            )
        })?;
    Ok(Json(inputs))
}

/// The full ordered foreign history a transfer carried in, with each
/// record's original provenance — the durable place a reviewer, a manager,
/// or a later hop re-exporting this task reads what a transferred task
/// inherited, since the task's own prompt only ever carries the latest
/// result of each kind.
pub(super) async fn get_task_transfer_history(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<crate::mobile_api::TaskTransferHistory>, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    let api = MobileApi::new(state.config.clone(), db);
    let history = api
        .list_transfer_history(&task_id)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("task not found: {task_id}"),
            )
        })?;
    Ok(Json(history))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateTaskRequest {
    #[serde(default, deserialize_with = "deserialize_nullable_field")]
    display_name: Option<Option<String>>,
}

fn deserialize_nullable_field<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

pub(super) async fn update_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(payload): Json<UpdateTaskRequest>,
) -> Result<Json<crate::mobile_api::TaskActionResponse>, (axum::http::StatusCode, String)> {
    let display_name = match payload.display_name {
        Some(Some(value)) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    "displayName must be non-empty when provided".to_string(),
                ));
            }
            Some(trimmed)
        }
        Some(None) => None,
        None => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "displayName must be provided".to_string(),
            ));
        }
    };
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    let task_id = db
        .resolve_pipeline_item_id(&task_id)
        .map_err(|e| db_write_error("db error", e))?
        .ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                "db error: not found".to_string(),
            )
        })?;
    db.update_pipeline_item_display_name(&task_id, display_name.as_deref())
        .map_err(|e| db_write_error("db error", e))?;
    state.publish_state_changed(StateChangeScope::Tasks);
    Ok(Json(crate::mobile_api::TaskActionResponse {
        task_id,
        follow_task: None,
        revision_budget: None,
    }))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchTasksQuery {
    query: String,
    repo_id: Option<String>,
    #[serde(default)]
    all_repos: bool,
    #[serde(default)]
    include_closed: bool,
    #[serde(default)]
    all_machines: bool,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListTasksQuery {
    repo_id: Option<String>,
    limit: Option<u32>,
    #[serde(default)]
    all_repos: bool,
    #[serde(default)]
    include_closed: bool,
    #[serde(default)]
    all_machines: bool,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetTasksQuery {
    repo_id: Option<String>,
    limit: Option<u32>,
    #[serde(default)]
    all_repos: bool,
    #[serde(default)]
    include_closed: bool,
    #[serde(default)]
    all_machines: bool,
    runtime_state: Option<TaskRuntimeState>,
    #[serde(default)]
    sort_by: TaskSort,
    #[serde(default)]
    order: TaskSortOrder,
}

pub(super) async fn search_tasks(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<SearchTasksQuery>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let db = Db::open(&state.config.db_path).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {}", e),
        )
    })?;
    if crate::mobile_api::record_orphaned_initialized_tasks(&db)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        state.publish_state_changed(StateChangeScope::Tasks);
    }
    let api = MobileApi::new(state.config.clone(), db);
    let repo_id = task_listing_repo_filter(
        query.repo_id.as_deref(),
        query.all_repos,
        query.all_machines,
    )?;
    let tasks = api
        .search_tasks_including_closed(&query.query, query.include_closed, repo_id)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !query.all_machines {
        return Ok(Json(serde_json::json!(tasks)));
    }
    aggregate_task_summaries(
        &state,
        tasks,
        task_listing_remote_path(
            "/v1/tasks/search",
            &[
                ("query", query.query),
                ("includeClosed", query.include_closed.to_string()),
                ("allMachines", "false".to_string()),
                ("allRepos", query.all_repos.to_string()),
            ],
            repo_id.map(|repo_id| ("repoId", repo_id)),
        ),
    )
    .await
}

fn task_listing_repo_filter(
    repo_id: Option<&str>,
    all_repos: bool,
    all_machines: bool,
) -> Result<Option<&str>, (axum::http::StatusCode, String)> {
    if all_machines && repo_id.is_some() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "repoId and allMachines cannot be used together; repository IDs are machine-local"
                .to_string(),
        ));
    }
    if all_repos && repo_id.is_some() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "repoId and allRepos cannot be used together".to_string(),
        ));
    }
    Ok((!all_repos).then_some(repo_id).flatten())
}

fn task_listing_remote_path(
    base: &str,
    params: &[(&str, String)],
    extra: Option<(&str, &str)>,
) -> String {
    let mut query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_path_segment(value)))
        .collect::<Vec<_>>();
    if let Some((key, value)) = extra {
        query.push(format!("{key}={}", encode_path_segment(value)));
    }
    format!("{base}?{}", query.join("&"))
}

async fn aggregate_get_tasks(
    state: &Arc<AppState>,
    mut response: GetTasksResponse,
    remote_path: String,
    sort: TaskSort,
    order: TaskSortOrder,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    response.scope.kind = "account".to_string();
    match state.list_active_relay_desktops().await {
        Ok(machine_ids) => {
            for machine_id in machine_ids {
                if machine_id == state.config.desktop_id {
                    continue;
                }
                response.scope.machine_ids.push(machine_id.clone());
                match state
                    .invoke_relay_desktop(
                        machine_id.clone(),
                        "GET".to_string(),
                        remote_path.clone(),
                        serde_json::Value::Null,
                    )
                    .await
                {
                    Ok(remote) if remote.status == 200 => match remote.body {
                        Some(body) => match serde_json::from_value::<GetTasksResponse>(body) {
                            Ok(mut peer) => {
                                response.truncated |= peer.truncated;
                                for task in &mut peer.tasks {
                                    task.machine_id = Some(machine_id.clone());
                                    if task.waiting_prompt_snippet.is_none() {
                                        task.waiting_prompt_snippet = task.snippet.take();
                                    }
                                }
                                response.tasks.append(&mut peer.tasks);
                            }
                            Err(error) => response.machine_errors.push(serde_json::json!({
                                "machineId": machine_id,
                                "error": format!("invalid filtered task-list response: {error}"),
                            })),
                        },
                        None => response.machine_errors.push(serde_json::json!({
                            "machineId": machine_id,
                            "error": "filtered task-list response had no body",
                        })),
                    },
                    Ok(remote) => response.machine_errors.push(serde_json::json!({
                        "machineId": machine_id,
                        "error": remote.error.unwrap_or_else(|| format!("HTTP {} (peer may not support kanna_get_tasks)", remote.status)),
                    })),
                    Err(error) => response.machine_errors.push(serde_json::json!({
                        "machineId": machine_id,
                        "error": error,
                    })),
                }
            }
        }
        Err(error) => response.machine_errors.push(serde_json::json!({
            "machineId": serde_json::Value::Null,
            "error": error,
        })),
    }

    response.tasks.sort_by(|left, right| {
        let left_time = match sort {
            TaskSort::UpdatedAt => &left.updated_at,
            TaskSort::CreatedAt => &left.created_at,
        };
        let right_time = match sort {
            TaskSort::UpdatedAt => &right.updated_at,
            TaskSort::CreatedAt => &right.created_at,
        };
        let ascending = left_time
            .cmp(right_time)
            .then_with(|| match sort {
                TaskSort::UpdatedAt => left.created_at.cmp(&right.created_at),
                TaskSort::CreatedAt => std::cmp::Ordering::Equal,
            })
            .then_with(|| left.machine_id.cmp(&right.machine_id))
            .then_with(|| left.id.cmp(&right.id));
        match order {
            TaskSortOrder::Asc => ascending,
            TaskSortOrder::Desc => ascending.reverse(),
        }
    });
    if response.tasks.len() > response.limit as usize {
        response.truncated = true;
        response.tasks.truncate(response.limit as usize);
    }
    Ok(Json(
        serde_json::to_value(response).expect("serialize aggregated task list"),
    ))
}

async fn aggregate_task_summaries(
    state: &Arc<AppState>,
    mut tasks: Vec<crate::mobile_api::TaskSummary>,
    remote_path: String,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let mut machine_errors = Vec::new();
    match state.list_active_relay_desktops().await {
        Ok(machine_ids) => {
            for machine_id in machine_ids {
                if machine_id == state.config.desktop_id {
                    continue;
                }
                match state
                    .invoke_relay_desktop(
                        machine_id.clone(),
                        "GET".to_string(),
                        remote_path.clone(),
                        serde_json::Value::Null,
                    )
                    .await
                {
                    Ok(response) if response.status == 200 => match response.body {
                        Some(body) => match serde_json::from_value::<
                            Vec<crate::mobile_api::TaskSummary>,
                        >(body)
                        {
                            Ok(mut remote_tasks) => {
                                for task in &mut remote_tasks {
                                    task.machine_id = Some(machine_id.clone());
                                    if task.waiting_prompt_snippet.is_none() {
                                        task.waiting_prompt_snippet = task.snippet.take();
                                    }
                                }
                                tasks.append(&mut remote_tasks);
                            }
                            Err(error) => machine_errors.push(serde_json::json!({
                                "machineId": machine_id,
                                "error": format!("invalid task-list response: {error}"),
                            })),
                        },
                        None => machine_errors.push(serde_json::json!({
                            "machineId": machine_id,
                            "error": "task-list response had no body",
                        })),
                    },
                    Ok(response) => machine_errors.push(serde_json::json!({
                        "machineId": machine_id,
                        "error": response.error.unwrap_or_else(|| format!("HTTP {}", response.status)),
                    })),
                    Err(error) => machine_errors.push(serde_json::json!({
                        "machineId": machine_id,
                        "error": error,
                    })),
                }
            }
        }
        Err(error) => machine_errors.push(serde_json::json!({
            "machineId": serde_json::Value::Null,
            "error": error,
        })),
    }
    tasks.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(Json(serde_json::json!({
        "tasks": tasks,
        "machineErrors": machine_errors,
    })))
}

/// Persist a newly created task's pull-request review context.
///
/// The context is *candidate information about the forge* — which PR, which
/// head commit, which base — supplied by whoever created the task. It is
/// deliberately not an approval and grants nothing: what it does is give the
/// operator's own control, and later the merge master, a durable PR identity
/// to work from. Without it a review child names only its `task-*` branch and
/// the local `pr/<n>` ref it forked from, neither of which the forge can
/// merge, and every consumer would be left parsing the review session's
/// terminal.
fn persist_created_task_review_context(
    db: &Db,
    task_id: &str,
    review_context: Option<&crate::db::ReviewContextInput>,
) -> Result<(), (axum::http::StatusCode, String)> {
    let Some(context) = review_context else {
        return Ok(());
    };
    db.upsert_task_review_context(task_id, context)
        .map(|_| ())
        .map_err(|error| (axum::http::StatusCode::BAD_REQUEST, error.to_string()))
}

pub(super) async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<crate::mobile_api::CreateTaskRequest>,
) -> Result<Json<crate::mobile_api::CreateTaskResponse>, (axum::http::StatusCode, String)> {
    create_task_with_requested_id(state, payload, None).await
}

pub(super) async fn put_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(payload): Json<crate::mobile_api::CreateTaskRequest>,
) -> Result<Json<crate::mobile_api::CreateTaskResponse>, (axum::http::StatusCode, String)> {
    validate_requested_task_id(&task_id)?;
    let _flight = state
        .begin_requested_task_creation(&task_id)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::CONFLICT,
                format!("task creation already in progress: {task_id}"),
            )
        })?;
    create_task_with_requested_id(state, payload, Some(task_id)).await
}

/// Transfer-only creation entry point. Historical inputs are persisted after
/// the task/worktree preparation transaction and before the daemon spawn, so
/// the resumed agent can read its complete directive record on its first turn.
pub(crate) async fn create_transferred_task_in_process(
    state: Arc<AppState>,
    payload: crate::mobile_api::CreateTaskRequest,
    requested_task_id: String,
    inputs: Vec<crate::db::ImportedTaskInput>,
    transfer_payload: crate::transfer_engine::payload::OutgoingTransferPayload,
) -> Result<crate::mobile_api::CreateTaskResponse, (axum::http::StatusCode, String)> {
    validate_requested_task_id(&requested_task_id)?;
    let _flight = state
        .begin_requested_task_creation(&requested_task_id)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::CONFLICT,
                format!("task creation already in progress: {requested_task_id}"),
            )
        })?;
    create_task_with_requested_id_and_inputs(
        state,
        payload,
        Some(requested_task_id),
        inputs,
        Some(transfer_payload),
    )
    .await
    .map(|Json(response)| response)
}

pub(super) async fn create_task_with_requested_id(
    state: Arc<AppState>,
    payload: crate::mobile_api::CreateTaskRequest,
    requested_task_id: Option<String>,
) -> Result<Json<crate::mobile_api::CreateTaskResponse>, (axum::http::StatusCode, String)> {
    create_task_with_requested_id_and_inputs(state, payload, requested_task_id, Vec::new(), None)
        .await
}

fn persist_transferred_task_context(
    db: &Db,
    task_id: &str,
    context: Option<&crate::mobile_api::TransferImportSummary>,
) -> Result<(), (axum::http::StatusCode, String)> {
    let Some(context) = context else {
        return Ok(());
    };
    let (Some(transfer_id), Some(workflow_definition)) = (
        context.transfer_id.as_deref(),
        context.workflow_definition.as_deref(),
    ) else {
        return Ok(());
    };
    db.upsert_transferred_task_context(
        task_id,
        transfer_id,
        workflow_definition,
        context.previous_stage_result.as_deref(),
        context.previous_main_result.as_deref(),
        context.revision_feedback.as_deref(),
    )
    .map_err(|error| db_write_error("could not persist transferred task context", error))?;
    if context.history.is_empty() {
        return Ok(());
    }
    let records: Vec<crate::db::TransferredHistoryRecord> = context
        .history
        .iter()
        .map(|record| crate::db::TransferredHistoryRecord {
            sequence: record.sequence as i64,
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
    db.import_transferred_task_history(task_id, &records)
        .map_err(|error| db_write_error("could not persist transferred task history", error))
}

async fn create_task_with_requested_id_and_inputs(
    state: Arc<AppState>,
    payload: crate::mobile_api::CreateTaskRequest,
    requested_task_id: Option<String>,
    imported_inputs: Vec<crate::db::ImportedTaskInput>,
    transfer_payload: Option<crate::transfer_engine::payload::OutgoingTransferPayload>,
) -> Result<Json<crate::mobile_api::CreateTaskResponse>, (axum::http::StatusCode, String)> {
    if let Some(task_id) = requested_task_id.as_deref() {
        validate_requested_task_id(task_id)?;
    }
    if payload.notify_task_id.is_some() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "notifyTaskId has been removed; observe completion with /v1/task-events or the task wait surface"
                .to_string(),
        ));
    }
    if let Some(snapshot) = payload.recovery_snapshot.as_ref() {
        snapshot
            .validate()
            .map_err(|message| (axum::http::StatusCode::BAD_REQUEST, message))?;
    }
    if let Some(transfer_import) = payload.transfer_import.as_ref() {
        transfer_import
            .validate()
            .map_err(|message| (axum::http::StatusCode::BAD_REQUEST, message))?;
        if transfer_payload.is_none() {
            return Err((
                axum::http::StatusCode::FORBIDDEN,
                "transferImport is reserved for the verified transfer lifecycle".to_string(),
            ));
        }
    }
    // Validated here, before anything is created: a review context that cannot
    // identify what is being reviewed must fail the request outright. Creating
    // the task and silently dropping the context would leave a review whose
    // operator control is absent for no visible reason.
    let review_context = match payload.review_context.as_ref() {
        Some(context) => Some(
            context
                .validated()
                .map_err(|error| (axum::http::StatusCode::BAD_REQUEST, error.to_string()))?,
        ),
        None => None,
    };

    #[cfg(test)]
    if payload.transfer_import.is_none() {
        if let Some(task_creator) = state.task_creator.clone() {
            return task_creator(payload)
                .map(Json)
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e));
        }
    }

    // Everything before the daemon spawn is synchronous git/SQLite work —
    // task preparation creates the worktree and runs repo-config setup, so
    // it must run on the blocking pool, never on a runtime worker.
    enum PreparedCreateOutcome {
        Done(crate::mobile_api::CreateTaskResponse),
        DormantCreated(crate::mobile_api::CreateTaskResponse),
        RepairFresh {
            existing: crate::mobile_api::CreateTaskResponse,
            prepared: crate::task_creator::PreparedStageRerun,
        },
        RepairResume {
            existing: crate::mobile_api::CreateTaskResponse,
            prepared: Box<crate::task_creator::PreparedStageRunSpawn>,
        },
        Spawn {
            prepared: crate::task_creator::PreparedTaskSpawn,
            resolved_blocker_ids: Vec<String>,
        },
    }
    let imported_inputs = Arc::new(imported_inputs);
    let transfer_head_oid = payload
        .transfer_import
        .as_ref()
        .and_then(|import| import.head_oid.clone());
    let transfer_id_for_proof = payload
        .transfer_import
        .as_ref()
        .and_then(|import| import.transfer_id.clone());
    let transfer_import_for_gate = payload.transfer_import.clone();
    let outcome = {
        let state = Arc::clone(&state);
        let imported_inputs = Arc::clone(&imported_inputs);
        super::blocking::run_handler_blocking("task create prepare", move || {
            if let Some(task_id) = requested_task_id.as_deref() {
                let db = Db::open(&state.config.db_path).map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {}", e),
                    )
                })?;
                if let Some(existing) =
                    existing_create_task_response(&db, task_id, &payload.repo_id, &payload.prompt)?
                {
                    let bound_manifest = db
                        .transferred_task_manifest_for_task(task_id)
                        .map_err(|error| db_write_error("db error", error))?;
                    if let Some((_, _, _, bound_task, state)) = bound_manifest.as_ref() {
                        if bound_task.as_deref() == Some(task_id) && state != "prepared" {
                            let owner_transfer_id = payload
                                .transfer_import
                                .as_ref()
                                .and_then(|summary| summary.transfer_id.as_deref());
                            let owner_manifest = match owner_transfer_id {
                                Some(transfer_id) => db
                                    .transferred_task_manifest(transfer_id)
                                    .map_err(|error| db_write_error("db error", error))?,
                                None => None,
                            };
                            let owns_import = owner_manifest.as_ref().is_some_and(
                                |(repo_id, _, _, local_task_id, manifest_state)| {
                                    repo_id == payload.repo_id
                                        && local_task_id.as_deref() == Some(task_id)
                                        && manifest_state == "importing"
                                },
                            );
                            if !owns_import {
                                return Err((
                                    axum::http::StatusCode::CONFLICT,
                                    format!(
                                        "transferred task {task_id} has no durable prepared proof"
                                    ),
                                ));
                            }
                        }
                    }
                    if payload.transfer_import.is_some() {
                        let transfer_id = payload
                            .transfer_import
                            .as_ref()
                            .and_then(|summary| summary.transfer_id.as_deref())
                            .ok_or_else(|| {
                                (
                                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                                    "transferred task is missing transfer identity".to_string(),
                                )
                            })?;
                        let manifest = db
                            .transferred_task_manifest(transfer_id)
                            .map_err(|error| db_write_error("db error", error))?;
                        let owned_for_task =
                            manifest
                                .as_ref()
                                .is_some_and(|(_, _, _, local_task_id, state)| {
                                    matches!(state.as_str(), "importing" | "prepared")
                                        && local_task_id.as_deref() == Some(task_id)
                                });
                        if !owned_for_task {
                            return Err((
                                axum::http::StatusCode::CONFLICT,
                                format!("transferred task {task_id} has no durable prepared proof"),
                            ));
                        }
                    }
                    persist_transferred_task_context(
                        &db,
                        task_id,
                        payload.transfer_import.as_ref(),
                    )?;
                    db.import_task_inputs(task_id, &imported_inputs)
                        .map_err(|error| db_write_error("could not import task inputs", error))?;
                    let existing_is_open = db
                        .get_pipeline_item(task_id)
                        .map_err(|e| db_write_error("db error", e))?
                        .is_some_and(|item| item.closed_at.is_none());
                    if existing_is_open
                        && !db
                            .has_durable_running_task_session(task_id)
                            .map_err(|e| db_write_error("db error", e))?
                    {
                        let prepared = crate::task_creator::prepare_create_task_repair_for_api(
                            &db,
                            &state.config,
                            task_id,
                        )
                        .map_err(|error| {
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                format!("task spawn repair prepare failed: {error}"),
                            )
                        })?;
                        if let Some(prepared) = prepared {
                            return Ok(PreparedCreateOutcome::RepairFresh { existing, prepared });
                        }
                        let latest = db
                            .latest_stage_run(task_id)
                            .map_err(|error| db_write_error("db error", error))?;
                        if latest.as_ref().is_some_and(|run| {
                            matches!(run.status.as_str(), "cancelled" | "failed")
                        }) {
                            let prepared = crate::task_creator::prepare_resume_task_for_api(
                                &db,
                                &state.config,
                                task_id,
                            )
                            .map_err(|error| {
                                (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    format!("task resume repair prepare failed: {error}"),
                                )
                            })?;
                            return Ok(PreparedCreateOutcome::RepairResume {
                                existing,
                                prepared: Box::new(prepared),
                            });
                        }
                        let prepared = crate::task_creator::prepare_rerun_stage_for_api(
                            &db,
                            &state.config,
                            task_id,
                        )
                        .map_err(|error| {
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                format!("task spawn repair prepare failed: {error}"),
                            )
                        })?;
                        return Ok(PreparedCreateOutcome::RepairFresh { existing, prepared });
                    }
                    return Ok(PreparedCreateOutcome::Done(existing));
                }
            }

            let requested_task = requested_task_id.as_ref().map(|task_id| {
                (
                    task_id.clone(),
                    payload.repo_id.clone(),
                    payload.prompt.clone(),
                )
            });
            let blocker_task_ids = payload.blocker_task_ids.clone().unwrap_or_default();
            let resolved_blocker_ids = if blocker_task_ids.is_empty() {
                Vec::new()
            } else {
                let db = Db::open(&state.config.db_path).map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {}", e),
                    )
                })?;
                resolve_task_blocker_ids(&db, &blocker_task_ids)?
            };
            if !resolved_blocker_ids.is_empty() {
                let db = Db::open(&state.config.db_path).map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {}", e),
                    )
                })?;
                let has_open_blockers =
                    resolved_blocker_ids
                        .iter()
                        .try_fold(false, |has_open, id| {
                            if has_open {
                                Ok(true)
                            } else {
                                let blocker = db
                                    .get_pipeline_item(id)
                                    .map_err(|e| db_write_error("db error", e))?
                                    .ok_or_else(|| {
                                        (
                                            axum::http::StatusCode::NOT_FOUND,
                                            format!("task not found: {id}"),
                                        )
                                    })?;
                                Ok(blocker.closed_at.is_none())
                            }
                        })?;
                if has_open_blockers {
                    let created = match crate::task_creator::create_dormant_task_for_api_with_error(
                        &db,
                        payload,
                        requested_task_id.clone(),
                    ) {
                        Ok(created) => created,
                        Err(error) => {
                            let requested_task =
                                requested_task.as_ref().map(|(task_id, repo_id, prompt)| {
                                    (task_id.as_str(), repo_id.as_str(), prompt.as_str())
                                });
                            let existing =
                                resolve_create_task_prepare_error(&db, error, requested_task)?;
                            return Ok(PreparedCreateOutcome::Done(existing));
                        }
                    };
                    if let Err(err) = persist_created_task_review_context(
                        &db,
                        &created.task_id,
                        review_context.as_ref(),
                    )
                    .and_then(|()| {
                        persist_resolved_task_blockers(&db, &created.task_id, &resolved_blocker_ids)
                    }) {
                        let rollback_result = db.delete_task_creation_artifacts(&created.task_id);
                        return Err(match rollback_result {
                            Ok(()) => err,
                            Err(rollback_err) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                format!("{}; rollback failed: {}", err.1, rollback_err),
                            ),
                        });
                    }
                    return Ok(PreparedCreateOutcome::DormantCreated(created));
                }
            }

            let transfer_context = payload.transfer_import.clone();
            let prepared = {
                let db = Db::open(&state.config.db_path).map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {}", e),
                    )
                })?;
                match crate::task_creator::prepare_task_for_api_with_error(
                    &db,
                    &state.config,
                    payload,
                    requested_task_id,
                ) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let requested_task =
                            requested_task.as_ref().map(|(task_id, repo_id, prompt)| {
                                (task_id.as_str(), repo_id.as_str(), prompt.as_str())
                            });
                        let existing =
                            resolve_create_task_prepare_error(&db, error, requested_task)?;
                        return Ok(PreparedCreateOutcome::Done(existing));
                    }
                }
            };
            if let Err((status, reason)) = persist_transferred_task_context(
                &Db::open(&state.config.db_path).map_err(|error| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {error}"),
                    )
                })?,
                crate::task_creator::prepared_task_id(&prepared),
                transfer_context.as_ref(),
            ) {
                let db = Db::open(&state.config.db_path).map_err(|error| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {error}"),
                    )
                })?;
                let rollback = crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                return Err((
                    status,
                    match rollback {
                        Ok(()) => reason,
                        Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                    },
                ));
            }
            if !imported_inputs.is_empty() {
                let db = Db::open(&state.config.db_path).map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {e}"),
                    )
                })?;
                if let Err(error) = db.import_task_inputs(
                    crate::task_creator::prepared_task_id(&prepared),
                    &imported_inputs,
                ) {
                    let reason = format!("could not import task inputs: {error}");
                    let rollback =
                        crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                    return Err((
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        match rollback {
                            Ok(()) => reason,
                            Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                        },
                    ));
                }
            }
            if let Some(import) = transfer_import_for_gate.as_ref() {
                let db = Db::open(&state.config.db_path).map_err(|error| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {error}"),
                    )
                })?;
                // Every failure below is discovered after `prepared` already
                // created the task's pipeline_item row and git worktree, so
                // (like the transfer-context and imported-inputs gates just
                // above) each one must roll that artifact back rather than
                // leaving an orphaned, never-admitted task behind.
                let transfer_id = match import.transfer_id.as_deref() {
                    Some(transfer_id) => transfer_id,
                    None => {
                        let reason = "transferred task is missing transfer identity".to_string();
                        let rollback =
                            crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                        return Err((
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            match rollback {
                                Ok(()) => reason,
                                Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                            },
                        ));
                    }
                };
                let expected_head = match import.head_oid.as_deref() {
                    Some(expected_head) => expected_head,
                    None => {
                        let reason = "transferred task is missing committed head".to_string();
                        let rollback =
                            crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                        return Err((
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            match rollback {
                                Ok(()) => reason,
                                Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                            },
                        ));
                    }
                };
                let (worktree, branch) = crate::task_creator::prepared_task_worktree(&prepared);
                let actual = match crate::transfer_engine::git::commit_oid(
                    std::path::Path::new(worktree),
                    branch,
                ) {
                    Ok(actual) => actual,
                    Err(reason) => {
                        let rollback =
                            crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                        return Err((
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            match rollback {
                                Ok(()) => reason,
                                Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                            },
                        ));
                    }
                };
                if actual != expected_head {
                    let reason = "transferred task head changed during preparation".to_string();
                    let rollback =
                        crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                    return Err((
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        match rollback {
                            Ok(()) => reason,
                            Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                        },
                    ));
                }
                let manifest = match db.transferred_task_manifest(transfer_id) {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        let (status, reason) = db_write_error("db error", error);
                        let rollback =
                            crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                        return Err((
                            status,
                            match rollback {
                                Ok(()) => reason,
                                Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                            },
                        ));
                    }
                };
                if manifest.as_ref().is_none_or(|(_, _, _, task, state)| {
                    task.as_deref() != Some(crate::task_creator::prepared_task_id(&prepared))
                        || state != "importing"
                }) {
                    let reason = "transferred task lacks an importing manifest".to_string();
                    let rollback =
                        crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                    return Err((
                        axum::http::StatusCode::CONFLICT,
                        match rollback {
                            Ok(()) => reason,
                            Err(rollback) => format!("{reason}; rollback failed: {rollback}"),
                        },
                    ));
                }
            }
            if !resolved_blocker_ids.is_empty() || review_context.is_some() {
                let db = Db::open(&state.config.db_path).map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("db error: {}", e),
                    )
                })?;
                if let Err(err) = persist_created_task_review_context(
                    &db,
                    crate::task_creator::prepared_task_id(&prepared),
                    review_context.as_ref(),
                )
                .and_then(|()| {
                    persist_task_blocker_rows(
                        &db,
                        crate::task_creator::prepared_task_id(&prepared),
                        &resolved_blocker_ids,
                    )
                }) {
                    let rollback_result =
                        crate::task_creator::rollback_prepared_task_for_api(&db, &prepared);
                    return Err(match rollback_result {
                        Ok(()) => err,
                        Err(rollback_err) => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!("{}; rollback failed: {}", err.1, rollback_err),
                        ),
                    });
                }
            }
            Ok(PreparedCreateOutcome::Spawn {
                prepared,
                resolved_blocker_ids,
            })
        })
        .await?
    };
    if let Some(transfer_payload) = transfer_payload.as_ref() {
        let task_id = match &outcome {
            PreparedCreateOutcome::Done(existing)
            | PreparedCreateOutcome::DormantCreated(existing)
            | PreparedCreateOutcome::RepairFresh { existing, .. }
            | PreparedCreateOutcome::RepairResume { existing, .. } => existing.task_id.as_str(),
            PreparedCreateOutcome::Spawn { prepared, .. } => {
                crate::task_creator::prepared_task_id(prepared)
            }
        };
        let transfer_id = transfer_id_for_proof.as_deref().ok_or_else(|| {
            (
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "transferred task is missing transfer identity".to_string(),
            )
        })?;
        let db = Db::open(&state.config.db_path).map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error: {error}"),
            )
        })?;
        let already_proved = db
            .transferred_task_manifest_content_commitment(transfer_id)
            .map_err(|error| db_write_error("db error", error))?
            .is_some();
        if already_proved {
            let binding = db
                .transferred_task_manifest(transfer_id)
                .map_err(|error| db_write_error("db error", error))?;
            if binding.as_ref().is_none_or(|(_, _, _, bound_task, state)| {
                bound_task.as_deref() != Some(task_id) || state != "prepared"
            }) {
                return Err((
                    axum::http::StatusCode::CONFLICT,
                    format!("transferred task {task_id} has conflicting prepared proof"),
                ));
            }
        } else {
            drop(db);
            crate::transfer_engine::import::verify_persisted_task_bundle(
                &state,
                transfer_payload,
                task_id,
                transfer_id,
                Some(imported_inputs.as_ref()),
            )
            .await
            .map_err(|error| {
                (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    format!("transferred task preparation proof failed: {error:?}"),
                )
            })?;
        }
    }
    let (prepared, resolved_blocker_ids) = match outcome {
        PreparedCreateOutcome::Done(existing) => return Ok(Json(existing)),
        PreparedCreateOutcome::DormantCreated(created) => {
            state.publish_state_changed(StateChangeScope::Tasks);
            state.publish_state_changed(StateChangeScope::Blockers);
            return Ok(Json(created));
        }
        PreparedCreateOutcome::RepairFresh { existing, prepared } => {
            let mut daemon = crate::daemon_client::DaemonClient::connect(&state.config.daemon_dir)
                .await
                .map_err(|e| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("daemon error: {}", e),
                    )
                })?;
            crate::task_creator::rerun_prepared_stage_for_api(
                &state.config.db_path,
                &mut daemon,
                &state.session_replacements,
                prepared,
            )
            .await
            .map_err(|error| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task spawn repair failed: {error}"),
                )
            })?;
            state.publish_state_changed(StateChangeScope::Tasks);
            return Ok(Json(existing));
        }
        PreparedCreateOutcome::RepairResume { existing, prepared } => {
            match crate::task_creator::daemon_session_presence(
                &state.config.daemon_dir,
                prepared.session_id(),
            )
            .await
            {
                crate::task_creator::DaemonSessionPresence::Present => {
                    let db_path = state.config.db_path.clone();
                    let repair_task_id = existing.task_id.clone();
                    let restored = super::blocking::run_handler_blocking(
                        "task create live-session reconciliation",
                        move || {
                            crate::http_api::restore_task_run_for_live_session(
                                &db_path,
                                &repair_task_id,
                            )
                            .map_err(|error| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, error))
                        },
                    )
                    .await?;
                    if restored {
                        log::warn!(
                            "requested-id repair found live daemon session for {}; restored interrupted run instead of spawning",
                            existing.task_id
                        );
                        state.publish_state_changed(StateChangeScope::Tasks);
                    }
                    return Ok(Json(existing));
                }
                crate::task_creator::DaemonSessionPresence::Unknown => {
                    return Err((
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        format!(
                            "could not verify that task session is dead: {}",
                            existing.task_id
                        ),
                    ));
                }
                crate::task_creator::DaemonSessionPresence::Absent => {}
            }
            let mut daemon = crate::daemon_client::DaemonClient::connect(&state.config.daemon_dir)
                .await
                .map_err(|error| {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("daemon error: {error}"),
                    )
                })?;
            crate::task_creator::spawn_prepared_stage_run_for_api(
                &state.config.db_path,
                &mut daemon,
                &state.session_replacements,
                *prepared,
            )
            .await
            .map_err(|error| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task resume repair failed: {error}"),
                )
            })?;
            state.publish_state_changed(StateChangeScope::Tasks);
            return Ok(Json(existing));
        }
        PreparedCreateOutcome::Spawn {
            prepared,
            resolved_blocker_ids,
        } => {
            // Transfer bundles are admitted only after the prepared worktree
            // proves the pinned committed head.  This runs before the daemon
            // is contacted, so no agent can execute an unverified import.
            if let Some(expected_head) = transfer_head_oid.as_deref() {
                let (worktree, branch) = crate::task_creator::prepared_task_worktree(&prepared);
                let actual =
                    crate::transfer_engine::git::commit_oid(std::path::Path::new(worktree), branch)
                        .map_err(|error| {
                            (
                                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                                format!("transferred task head verification failed: {error}"),
                            )
                        })?;
                if actual != expected_head {
                    return Err((axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        format!("transferred task head mismatch: expected {expected_head}, got {actual}")));
                }
            }
            (prepared, resolved_blocker_ids)
        }
    };
    let mut daemon = crate::daemon_client::DaemonClient::connect(&state.config.daemon_dir)
        .await
        .map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("daemon error: {}", e),
            )
        })?;
    let created = crate::task_creator::spawn_prepared_task_for_api_with_diagnostics(
        &state.config.db_path,
        &mut daemon,
        prepared,
    )
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    state.publish_state_changed(StateChangeScope::Tasks);
    if !resolved_blocker_ids.is_empty() {
        state.publish_state_changed(StateChangeScope::Blockers);
    }
    Ok(Json(created))
}

pub(super) fn validate_requested_task_id(
    task_id: &str,
) -> Result<(), (axum::http::StatusCode, String)> {
    // New clients generate the same 8-hex IDs as the server. Keep accepting
    // longer IDs for mobile versions released before the short-ID change;
    // tighten this to exactly 8 only after those versions are no longer in the
    // supported mobile population. Existing long-ID tasks remain valid IDs.
    let valid_length = (8..=64).contains(&task_id.len());
    let lowercase_hex = task_id
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid_length && lowercase_hex {
        Ok(())
    } else {
        Err((
            axum::http::StatusCode::BAD_REQUEST,
            "taskId must be 8 to 64 lowercase hexadecimal characters".to_string(),
        ))
    }
}

fn existing_create_task_response(
    db: &Db,
    task_id: &str,
    repo_id: &str,
    prompt: &str,
) -> Result<Option<crate::mobile_api::CreateTaskResponse>, (axum::http::StatusCode, String)> {
    let Some(item) = db
        .get_pipeline_item(task_id)
        .map_err(|e| db_write_error("db error", e))?
    else {
        return Ok(None);
    };
    if item.repo_id != repo_id || item.prompt.as_deref() != Some(prompt) {
        return Err((
            axum::http::StatusCode::CONFLICT,
            format!("taskId already exists with different task data: {task_id}"),
        ));
    }
    let title = item
        .display_name
        .clone()
        .or(item.prompt.clone())
        .unwrap_or_else(|| item.id.clone());
    let stage = item.stage.clone().ok_or_else(|| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("task {task_id} is missing its current stage"),
        )
    })?;
    let agent_type = item.agent_type.clone().ok_or_else(|| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("task {task_id} is missing its current agent type"),
        )
    })?;
    let worktree_path = db
        .get_task_worktree_path(task_id)
        .map_err(|e| db_write_error("db error", e))?;
    Ok(Some(crate::mobile_api::CreateTaskResponse {
        task_id: item.id,
        repo_id: item.repo_id,
        title,
        prompt: prompt.to_string(),
        stage,
        agent_type,
        worktree_path,
    }))
}

pub(super) fn resolve_create_task_prepare_error(
    db: &Db,
    error: crate::task_creator::PrepareTaskError,
    requested_task: Option<(&str, &str, &str)>,
) -> Result<crate::mobile_api::CreateTaskResponse, (axum::http::StatusCode, String)> {
    match error {
        crate::task_creator::PrepareTaskError::RequestedTaskIdAlreadyExists => {
            let Some((task_id, repo_id, prompt)) = requested_task else {
                return Err((
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "generated task id collided with an existing task".to_string(),
                ));
            };
            existing_create_task_response(db, task_id, repo_id, prompt)?.ok_or_else(|| {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("taskId collision disappeared before replay: {task_id}"),
                )
            })
        }
        crate::task_creator::PrepareTaskError::InvalidRequest(error) => {
            Err((axum::http::StatusCode::BAD_REQUEST, error))
        }
        crate::task_creator::PrepareTaskError::Other(error) => {
            Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, error))
        }
    }
}
