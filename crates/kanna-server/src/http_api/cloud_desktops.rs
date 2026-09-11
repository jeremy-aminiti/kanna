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
    // A trusted discovered LAN peer must not disappear from machine
    // discovery merely because relay happens to be unavailable - added
    // unconditionally, alongside relay presence rather than instead of it,
    // and never affecting `relay_available`/`error`, which report relay's
    // own dimension exactly as before.
    ids.extend(super::invoke_desktop::eligible_lan_desktop_ids(&state));
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
    use super::*;
    use axum::extract::{Path, State};

    /// A genuinely isolated per-desktop config, deliberately not
    /// `test_state_with_seed` - that shared helper places `pairing_store_path`
    /// directly under one shared temp root with only the *filename* made
    /// unique, so `Config::machine_trust_store_path`/`lan_tls_identity_path`
    /// (both derived from `pairing_store_path`'s *parent* directory) resolve
    /// to the exact same path for every desktop that uses it - fine for
    /// this file's existing single-desktop tests, but a real collision for
    /// a two-desktop LAN scenario (confirmed directly: an earlier version of
    /// this test using `test_state_with_seed` for both desktops had the
    /// source's later `.save()` silently overwrite the target's earlier
    /// inbound grant, since both resolved to one shared `machine-trust.json`,
    /// exactly the class of collision `crate::test_paths`'s own doc
    /// comments warn about. Mirrors `invoke_desktop.rs`'s own
    /// `lan_e2e_test_config` pattern, which does not share this bug.
    fn isolated_lan_test_config(desktop_id: &str) -> crate::config::Config {
        let dir = crate::test_paths::unique_test_dir(&format!("cloud-desktops-lan-{desktop_id}"));
        crate::config::Config {
            relay_url: String::new(),
            device_token: "device-token".to_string(),
            firebase_project_id: "kanna-local".to_string(),
            firebase_auth_emulator_url: None,
            firebase_firestore_emulator_host: None,
            daemon_dir: dir.join("daemon").to_string_lossy().into_owned(),
            db_path: crate::db::Db::test_db_path(&format!("cloud-desktops-lan-{desktop_id}")),
            kanna_cli_path: None,
            desktop_id: desktop_id.to_string(),
            desktop_secret: Some("desktop-secret".to_string()),
            desktop_name: format!("{desktop_id} Mac"),
            version: "test-version".to_string(),
            environment: "development".to_string(),
            lan_host: "127.0.0.1".to_string(),
            lan_port: 48120,
            transfer_port: 4455,
            lan_routing_port: 4460,
            activity_event_debounce_seconds: 300,
            pairing_store_path: dir.join("pairings.json").to_string_lossy().into_owned(),
        }
    }

    /// Real CLI/MCP route provenance, at the exact HTTP/JSON contract those
    /// surfaces parse - `crates/kanna-cli/src/commands/tool.rs`'s
    /// `machine_status_with_route` and `crates/kanna-mcp/src/main.rs`'s
    /// equivalent both read `MachineInvokeHttpResponse.route` off this
    /// handler's real JSON response, not off `invoke_desktop` the Rust
    /// function directly. Every existing real-LAN-dial test in
    /// `invoke_desktop.rs` calls that function directly, one layer below
    /// the actual HTTP handler CLI/MCP talk to - this proves the boundary
    /// those two crates actually depend on, through the real axum handler,
    /// with a real pinned-TLS dial to an explicitly-seeded candidate (not
    /// real mDNS discovery, which remains blocked on this host - see the
    /// task checkpoint).
    #[tokio::test]
    async fn invoke_cloud_desktop_reports_lan_route_provenance_in_its_real_http_response() {
        let target_config = isolated_lan_test_config("desktop-cli-mcp-target");
        let target_state = Arc::new(AppState::new(target_config));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        let target_identity_path = target_state.config().lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_state.config().desktop_id,
            &target_state.config().environment,
        )
        .expect("create target identity");

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_state.config().machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret("the-bearer-secret");
            store.accept_inbound(
                "desktop-cli-mcp-source",
                &hash,
                "uid-1",
                "development",
                &target_state.config().desktop_id,
                now_ms,
            );
            store.save(&target_store_path).expect("seed target trust");
        }

        let listener_addr =
            super::super::lan_listener::spawn_for_test(Arc::clone(&target_state)).await;
        let candidate = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            listener_addr.port(),
        );

        let source_config = isolated_lan_test_config("desktop-cli-mcp-source");
        let source_state = Arc::new(AppState::new(source_config));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate("desktop-cli-mcp-target".to_string(), candidate);
        let source_store_path = source_state.config().machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-cli-mcp-target",
                    "uid-1",
                    "development",
                    &source_state.config().desktop_id,
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-cli-mcp-target",
                    "the-bearer-secret",
                    &source_state.config().desktop_id,
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let response = invoke_cloud_desktop(
            DesktopLocalAccess,
            State(Arc::clone(&source_state)),
            Path("desktop-cli-mcp-target".to_string()),
            Json(MachineInvokeRequest {
                method: "GET".to_string(),
                path: "/v1/status".to_string(),
                body: serde_json::Value::Null,
            }),
        )
        .await
        .expect("the real HTTP handler must complete")
        .0;

        assert_eq!(
            response.route, "lan",
            "CLI/MCP's exact route-provenance contract must report \"lan\" for a real LAN \
             dial's actual HTTP response: {response:?}"
        );
        assert_eq!(response.status, 200, "{response:?}");
    }

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

    /// Finding #2's LAN-peer-enumeration completion: a trusted discovered
    /// LAN peer must not vanish from machine discovery merely because relay
    /// happens to be unavailable. `desktop_routing_available()` defaults to
    /// `false` on a freshly-constructed `AppState` (no relay setup here at
    /// all), matching a genuine outage; the LAN peer must still be listed,
    /// and `relay_available`/`error` must still truthfully report the
    /// outage as its own separate dimension.
    #[tokio::test]
    async fn a_trusted_lan_peer_is_listed_even_while_relay_is_unavailable() {
        let state =
            crate::http_api::test_state_with_seed("desktop-observer", "Observer Mac", |_db| {});
        state.set_authenticated_account_uid(Some("uid-1".to_string()));
        state.set_lan_candidate(
            "desktop-lan-peer".to_string(),
            "127.0.0.1:1".parse().unwrap(),
        );
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let store_path = state.config().machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-lan-peer",
                    "uid-1",
                    "development",
                    &state.config().desktop_id,
                    || Ok("secret".to_string()),
                    now_ms,
                )
                .unwrap();
            store
                .confirm_outbound(
                    "desktop-lan-peer",
                    "secret",
                    &state.config().desktop_id,
                    Some("fake-ca".to_string()),
                    now_ms + 1000,
                )
                .unwrap();
            store.save(&store_path).unwrap();
        }

        let response = list_cloud_desktops(DesktopLocalAccess, State(Arc::clone(&state))).await;

        assert!(
            !response.relay_available,
            "relay must still truthfully report unavailable"
        );
        assert!(
            response.error.is_some(),
            "the relay outage must still be reported as its own error"
        );
        assert!(
            response.machines.iter().any(|m| m.id == "desktop-lan-peer"),
            "a trusted discovered LAN peer must still be listed: {:?}",
            response.machines
        );
    }

    /// The negative case: a discovered candidate with no established trust
    /// grant is not a machine this desktop can reach, and must not appear.
    #[tokio::test]
    async fn an_untrusted_discovered_candidate_is_not_listed() {
        let state =
            crate::http_api::test_state_with_seed("desktop-observer-2", "Observer Mac", |_db| {});
        state.set_authenticated_account_uid(Some("uid-1".to_string()));
        state.set_lan_candidate(
            "desktop-untrusted-peer".to_string(),
            "127.0.0.1:1".parse().unwrap(),
        );

        let response = list_cloud_desktops(DesktopLocalAccess, State(Arc::clone(&state))).await;

        assert!(
            !response
                .machines
                .iter()
                .any(|m| m.id == "desktop-untrusted-peer"),
            "a merely-discovered, never-trusted candidate must not be listed: {:?}",
            response.machines
        );
    }
}
