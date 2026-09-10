use super::lan_trust::TrustedLanDeviceAccess;
use super::state::{AppState, TunneledHttpInvoke};
use super::task_files::AuthenticatedTaskFileAccess;
use crate::db::Db;
use crate::task_graph::{TaskGraph, TaskGraphError};
use axum::extract::{ConnectInfo, Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskGraphQuery {
    from_ref: Option<String>,
}

pub(super) async fn get_task_graph(
    State(state): State<Arc<AppState>>,
    relay_access: Option<Extension<AuthenticatedTaskFileAccess>>,
    lan_access: Option<Extension<TrustedLanDeviceAccess>>,
    tunneled: Option<Extension<TunneledHttpInvoke>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path(task_id): Path<String>,
    Query(query): Query<TaskGraphQuery>,
) -> Result<Json<TaskGraph>, (StatusCode, String)> {
    let desktop_local = tunneled.is_none()
        && peer.is_some_and(|Extension(ConnectInfo(addr))| addr.ip().is_loopback());
    if relay_access.is_none() && lan_access.is_none() && !desktop_local {
        return Err((
            StatusCode::UNAUTHORIZED,
            "task graph requires an authenticated relay or a paired device".to_string(),
        ));
    }
    super::blocking::run_handler_blocking("task graph read", move || {
        let db = Db::open(&state.config().db_path).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error: {error}"),
            )
        })?;
        crate::task_graph::read_task_graph(&db, &task_id, query.from_ref.as_deref())
            .map(Json)
            .map_err(map_task_graph_error)
    })
    .await
}

fn map_task_graph_error(error: TaskGraphError) -> (StatusCode, String) {
    let status = match error {
        TaskGraphError::TaskNotFound => StatusCode::NOT_FOUND,
        TaskGraphError::WorkspaceUnavailable => StatusCode::CONFLICT,
        TaskGraphError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string())
}
