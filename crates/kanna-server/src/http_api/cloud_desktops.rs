use super::lan_trust::DesktopLocalAccess;
use super::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MachineDescriptor {
    id: String,
    name: Option<String>,
    is_local: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MachineListResponse {
    current_machine_id: String,
    relay_available: bool,
    machines: Vec<MachineDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MachineInvokeRequest {
    method: String,
    path: String,
    #[serde(default)]
    body: serde_json::Value,
}

pub(super) async fn list_cloud_desktops(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
) -> Json<MachineListResponse> {
    let current_id = state.config().desktop_id.clone();
    let mut ids = vec![current_id.clone()];
    let (relay_available, error) = if state.desktop_routing_available() {
        match state.list_active_relay_desktops().await {
            Ok(active) => {
                ids.extend(active);
                (true, None)
            }
            Err(error) => (false, Some(error)),
        }
    } else {
        (false, Some(state.desktop_routing_unavailable_reason()))
    };
    ids.sort();
    ids.dedup();
    let machines = ids
        .into_iter()
        .map(|id| {
            let is_local = id == current_id;
            MachineDescriptor {
                id,
                name: is_local.then(|| state.config().desktop_name.clone()),
                is_local,
            }
        })
        .collect();
    Json(MachineListResponse {
        current_machine_id: current_id,
        relay_available,
        machines,
        error,
    })
}

/// `HttpInvokeResponse`'s fields plus which transport actually served this
/// call. A separate response shape rather than a new field on
/// `HttpInvokeResponse` itself: that struct has a dozen other construction
/// sites across this crate that have nothing to do with desktop-to-desktop
/// routing, and none of them need to grow a route to reason about.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MachineInvokeHttpResponse {
    status: u16,
    body: Option<serde_json::Value>,
    error: Option<String>,
    /// "local" | "lan" | "relay" - see `invoke_desktop::RouteProvenance`.
    route: &'static str,
}

pub(super) async fn invoke_cloud_desktop(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Path(desktop_id): Path<String>,
    Json(request): Json<MachineInvokeRequest>,
) -> Result<Json<MachineInvokeHttpResponse>, (axum::http::StatusCode, String)> {
    validate_invoke_request(&desktop_id, &request)?;
    let routed = super::invoke_desktop::invoke_desktop(
        state,
        desktop_id,
        request.method,
        request.path,
        request.body,
    )
    .await
    .map_err(|error| (axum::http::StatusCode::BAD_GATEWAY, error))?;
    Ok(Json(MachineInvokeHttpResponse {
        status: routed.response.status,
        body: routed.response.body,
        error: routed.response.error,
        route: routed.route.as_str(),
    }))
}

fn validate_invoke_request(
    desktop_id: &str,
    request: &MachineInvokeRequest,
) -> Result<(), (axum::http::StatusCode, String)> {
    if desktop_id.trim().is_empty() || desktop_id.chars().any(char::is_control) {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "machine id must not be empty or contain control characters".to_string(),
        ));
    }
    if !matches!(request.method.as_str(), "GET" | "POST" | "PATCH") {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "remote method must be GET, POST, or PATCH".to_string(),
        ));
    }
    if !request.path.starts_with("/v1/")
        || request.path.contains("://")
        || request.path.chars().any(char::is_control)
        || request.path.starts_with("/v1/cloud/desktops")
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "remote path must be a non-recursive /v1/ API path".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_invoke_request, MachineInvokeRequest};

    #[test]
    fn validates_catalog_http_requests_and_rejects_recursive_proxying() {
        let request = MachineInvokeRequest {
            method: "GET".to_string(),
            path: "/v1/tasks/recent".to_string(),
            body: serde_json::Value::Null,
        };
        assert!(validate_invoke_request("desktop-two", &request).is_ok());

        let recursive = MachineInvokeRequest {
            path: "/v1/cloud/desktops/desktop-two/invoke".to_string(),
            ..request
        };
        assert!(validate_invoke_request("desktop-two", &recursive).is_err());
    }
}
