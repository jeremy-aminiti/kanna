//! The single choke point for a general machine invoke to another desktop.
//!
//! Every production caller that used to reach `AppState::invoke_relay_desktop`
//! directly should call [`invoke_desktop`] instead. `invoke_relay_desktop`
//! itself is untouched and remains exactly what this module falls back to.
//!
//! [`attempt_lan_invoke`] dials the target with a client pinned to exactly
//! the CA a relay bootstrap attested for it (see `lan_tls`), at whatever
//! address discovery last observed (`AppState::lan_candidate_for`) - never
//! trusting that address for anything but where to *try* connecting. With
//! no candidate, no grant, or no attested trust anchor yet, the fallback/
//! uncertainty decision table below still applies unchanged: those are
//! ordinary `PreDispatch` cases, so behavior degrades to relay exactly as
//! it always has, never fails the caller's request outright.

use super::state::{AppState, HttpInvokeResponse};
use std::sync::Arc;

/// Where a machine invoke's result actually came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteProvenance {
    Local,
    Lan,
    Relay,
}

impl RouteProvenance {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RouteProvenance::Local => "local",
            RouteProvenance::Lan => "lan",
            RouteProvenance::Relay => "relay",
        }
    }
}

pub(crate) struct RoutedInvokeResponse {
    pub response: HttpInvokeResponse,
    pub route: RouteProvenance,
}

/// The three things attempting a LAN invoke can resolve to. This is the
/// whole fallback/uncertainty contract in one type: only `PreDispatch`
/// (no candidate, or a failure proven to have happened before any
/// application byte was sent) may ever fall back to relay; the other two
/// variants are terminal and must never trigger one.
enum LanAttemptOutcome {
    /// No trusted+reachable candidate, or the attempt failed at or before
    /// establishing the connection - nothing reached the peer, so falling
    /// back to relay cannot double-apply anything.
    PreDispatch(#[allow(dead_code)] String),
    /// The request was dispatched but the response was lost before a
    /// definite status could be read. Must be reported as delivery_uncertain
    /// and never retried automatically, on this route or on relay - the peer
    /// may already have applied it.
    PostDispatchUncertain,
    /// A definite HTTP response came back, including a non-2xx application
    /// or authorization error. This *is* the answer; it must propagate
    /// unchanged and never trigger a fallback.
    Definite(HttpInvokeResponse),
}

/// Resolves a [`LanAttemptOutcome`] into either a terminal routed response
/// (`Some(_)`), or `None` telling the caller it is safe to fall back to
/// relay. Kept as a pure function, independent of the actual transport, so
/// the fallback/uncertainty contract itself is unit-testable without a
/// network or a TLS stack.
fn resolve_lan_outcome(outcome: LanAttemptOutcome) -> Option<RoutedInvokeResponse> {
    match outcome {
        LanAttemptOutcome::PreDispatch(_reason) => None,
        LanAttemptOutcome::PostDispatchUncertain => Some(RoutedInvokeResponse {
            response: HttpInvokeResponse {
                status: 0,
                body: None,
                error: Some("delivery_uncertain".to_string()),
            },
            route: RouteProvenance::Lan,
        }),
        LanAttemptOutcome::Definite(response) => Some(RoutedInvokeResponse {
            response,
            route: RouteProvenance::Lan,
        }),
    }
}

/// The shared routing boundary for a general machine invoke. Local dispatch
/// and the unchanged relay fallback are unconditionally correct today; the
/// LAN branch is real machinery wired to a stub (see module docs).
pub(crate) async fn invoke_desktop(
    state: Arc<AppState>,
    desktop_id: String,
    method: String,
    path: String,
    body: serde_json::Value,
) -> Result<RoutedInvokeResponse, String> {
    if desktop_id == state.config().desktop_id {
        let response =
            super::routes::dispatch_authenticated_http_invoke(state, &method, &path, body).await;
        return Ok(RoutedInvokeResponse {
            response,
            route: RouteProvenance::Local,
        });
    }

    let outcome = attempt_lan_invoke(&state, &desktop_id, &method, &path, &body).await;
    if let Some(routed) = resolve_lan_outcome(outcome) {
        return Ok(routed);
    }

    let response = state
        .invoke_relay_desktop(desktop_id, method, path, body)
        .await?;
    Ok(RoutedInvokeResponse {
        response,
        route: RouteProvenance::Relay,
    })
}

/// The real LAN attempt: requires an unexpired outbound grant under the
/// *current* account (with an already-attested TLS trust anchor) and a
/// discovered candidate address, dials it with a client pinned to exactly
/// that trust anchor, and verifies the standard TLS handshake - normal
/// WebPKI chain validation plus normal hostname verification against
/// `desktop_id` (the leaf's own SAN) - before the bearer secret or any
/// application byte ever goes out. Discovery only ever supplies the
/// address to *attempt*; it is never itself trusted.
async fn attempt_lan_invoke(
    state: &Arc<AppState>,
    desktop_id: &str,
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> LanAttemptOutcome {
    let Some(store_path) = state.config().machine_trust_store_path() else {
        return LanAttemptOutcome::PreDispatch("no machine trust store configured".to_string());
    };
    let Ok(now_ms) = crate::machine_trust::unix_time_ms() else {
        return LanAttemptOutcome::PreDispatch("clock unavailable".to_string());
    };
    let current_account_uid = state.authenticated_account_uid();
    let Ok(store) = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path) else {
        return LanAttemptOutcome::PreDispatch(format!(
            "machine trust store for {desktop_id} is unreadable"
        ));
    };
    let Some(grant) =
        store.outbound_grant_for(desktop_id, current_account_uid.as_deref(), now_ms)
    else {
        return LanAttemptOutcome::PreDispatch(format!(
            "no unexpired outbound LAN grant for desktop {desktop_id}"
        ));
    };
    let Some(trust_anchor_pem) = grant.trust_anchor_pem.clone() else {
        return LanAttemptOutcome::PreDispatch(format!(
            "no attested TLS trust anchor yet for desktop {desktop_id}"
        ));
    };
    let bearer_secret = grant.bearer_secret.clone();
    let Some(candidate) = state.lan_candidate_for(desktop_id) else {
        return LanAttemptOutcome::PreDispatch(format!(
            "no LAN candidate address discovered for desktop {desktop_id}"
        ));
    };

    dial_lan_invoke(
        &state.config().desktop_id,
        desktop_id,
        candidate,
        &trust_anchor_pem,
        &bearer_secret,
        method,
        path,
        body,
    )
    .await
}

/// Mirrors `lan_listener`'s own response envelope shape - duplicated rather
/// than shared across the module boundary for the same reason
/// `MachineInvokeResponse` is duplicated between kanna-mcp and kanna-cli:
/// a tiny wire shape, not worth a shared-visibility fight over private
/// struct fields.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LanGatewayResponse {
    status: u16,
    body: Option<serde_json::Value>,
    error: Option<String>,
}

/// Builds a client trusting only `trust_anchor_pem` (the CA a relay
/// bootstrap already attested for this exact target - never the system
/// roots, never anything discovery supplied), overrides DNS for
/// `target_desktop_id` to the discovered `candidate` address, and sends
/// the invoke. `resolve` is what lets TLS verification run against the
/// stable logical name (`target_desktop_id`, matching the leaf's SAN)
/// while the TCP connection itself goes wherever discovery pointed -
/// exactly the "connect anywhere, verify identity independently of that"
/// split the design calls for.
async fn dial_lan_invoke(
    this_desktop_id: &str,
    target_desktop_id: &str,
    candidate: std::net::SocketAddr,
    trust_anchor_pem: &str,
    bearer_secret: &str,
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> LanAttemptOutcome {
    let client_config = match crate::lan_tls::client_config_pinned_to_ca(trust_anchor_pem) {
        Ok(config) => config,
        Err(error) => return LanAttemptOutcome::PreDispatch(error),
    };
    let client_config = match std::sync::Arc::try_unwrap(client_config) {
        Ok(config) => config,
        Err(shared) => (*shared).clone(),
    };
    let client = match reqwest::Client::builder()
        .use_preconfigured_tls(client_config)
        .resolve(target_desktop_id, candidate)
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return LanAttemptOutcome::PreDispatch(format!(
                "failed to build LAN client for {target_desktop_id}: {error}"
            ))
        }
    };
    // The listener's own gateway endpoint is always POST - it is an RPC
    // wrapper carrying the actual method/path/body as its payload, exactly
    // like the general API's own /v1/cloud/desktops/{id}/invoke. `method`
    // here names the *wrapped* request, never the outer HTTP method.
    let url = format!("https://{target_desktop_id}:{}/invoke", candidate.port());
    let request = client
        .post(&url)
        .header(super::lan_trust::DEVICE_ID_HEADER, this_desktop_id)
        .header(super::lan_trust::DEVICE_SECRET_HEADER, bearer_secret)
        .json(&serde_json::json!({ "method": method, "path": path, "body": body }));

    match request.send().await {
        Ok(response) => {
            let outer_status = response.status();
            // The gateway itself answered 200: unwrap its {status, body,
            // error} envelope to get the *wrapped* invoke's own result -
            // exactly the shape invoke_cloud_desktop's own callers already
            // unwrap for the relay/local paths, so a caller of
            // invoke_desktop sees the same shape regardless of transport.
            // Anything else (401 from the bearer check, 400 from the
            // gateway's own path validation) has no such envelope: that
            // status/body pair *is* the definite answer.
            if outer_status == reqwest::StatusCode::OK {
                match response.json::<LanGatewayResponse>().await {
                    Ok(envelope) => LanAttemptOutcome::Definite(HttpInvokeResponse {
                        status: envelope.status,
                        body: envelope.body,
                        error: envelope.error,
                    }),
                    Err(_) => LanAttemptOutcome::PostDispatchUncertain,
                }
            } else {
                let body = response.json::<serde_json::Value>().await.ok();
                LanAttemptOutcome::Definite(HttpInvokeResponse {
                    status: outer_status.as_u16(),
                    body,
                    error: None,
                })
            }
        }
        Err(error) => {
            // `is_connect` covers failures at or before TCP/TLS
            // establishment - nothing reached the peer, so relay fallback
            // cannot double-apply anything. Anything else (a timeout after
            // the request was already written, a connection reset mid
            // response) is deliberately treated as uncertain rather than
            // guessed at: this is the conservative direction, since the
            // alternative risks replaying a mutation the peer may already
            // have applied.
            if error.is_connect() {
                LanAttemptOutcome::PreDispatch(format!(
                    "LAN connect to {target_desktop_id} failed: {error}"
                ))
            } else {
                LanAttemptOutcome::PostDispatchUncertain
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lan_e2e_test_config(desktop_id: &str) -> crate::config::Config {
        let dir = crate::test_paths::unique_test_dir(&format!("lan-e2e-{desktop_id}"));
        crate::config::Config {
            relay_url: String::new(),
            device_token: "device-token".to_string(),
            firebase_project_id: "kanna-local".to_string(),
            firebase_auth_emulator_url: None,
            firebase_firestore_emulator_host: None,
            daemon_dir: dir.join("daemon").to_string_lossy().into_owned(),
            db_path: crate::db::Db::test_db_path(&format!("lan-e2e-{desktop_id}")),
            kanna_cli_path: None,
            desktop_id: desktop_id.to_string(),
            desktop_secret: Some("desktop-secret".to_string()),
            desktop_name: format!("{desktop_id} Mac"),
            version: "test-version".to_string(),
            environment: "development".to_string(),
            lan_host: "127.0.0.1".to_string(),
            lan_port: 48120,
            transfer_port: 4455,
            activity_event_debounce_seconds: 300,
            pairing_store_path: dir.join("pairings.json").to_string_lossy().into_owned(),
        }
    }

    /// The full chain end to end over a real loopback TLS socket: a target's
    /// real `lan_listener` accepts a connection from the real
    /// `invoke_desktop` client path, completes a standard rustls handshake
    /// pinned to the target's actual attested CA, authenticates the bearer
    /// secret against the target's real `machine_trust` store, and dispatches
    /// into the target's real router - proving the seam this task exists to
    /// build, not a simulation of any part of it.
    #[tokio::test]
    async fn a_real_lan_invoke_completes_over_a_real_tls_socket_end_to_end() {
        let target_config = lan_e2e_test_config("desktop-target");
        let target_identity_path = target_config.lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_config.desktop_id,
        )
        .expect("create target identity");

        let target_state = Arc::new(AppState::new(target_config.clone()));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret("the-bearer-secret");
            store.accept_inbound("desktop-source", &hash, "uid-1", "development", now_ms);
            store.save(&target_store_path).expect("seed target trust");
        }

        let listener_addr = super::super::lan_listener::spawn_for_test(Arc::clone(&target_state))
            .await;
        let candidate =
            std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), listener_addr.port());

        let source_config = lan_e2e_test_config("desktop-source");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate("desktop-target".to_string(), candidate);
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-target",
                    "uid-1",
                    "development",
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-target",
                    "the-bearer-secret",
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let routed = invoke_desktop(
            Arc::clone(&source_state),
            "desktop-target".to_string(),
            "GET".to_string(),
            "/v1/status".to_string(),
            serde_json::Value::Null,
        )
        .await
        .expect("invoke_desktop should complete");

        assert_eq!(routed.route, RouteProvenance::Lan, "{:?}", routed.response);
        assert_eq!(routed.response.status, 200, "{:?}", routed.response);
        let body = routed.response.body.expect("status response body");
        assert_eq!(body["desktopId"], "desktop-target");
    }

    /// A real TLS handshake can succeed (the client trusts the target's
    /// real CA) while the application-layer bearer secret still does not
    /// verify - a source whose outbound grant somehow diverged from what
    /// the target actually accepts (a stale/corrupted grant, a manually
    /// edited store). This is a *definite* 401 answered by the real
    /// listener, not a connection failure, so route ends up Lan and no
    /// relay fallback happens even though the wrapped call did not
    /// succeed - matching the fallback contract exactly.
    #[tokio::test]
    async fn a_real_lan_invoke_with_the_wrong_bearer_secret_is_rejected_definitely() {
        let target_config = lan_e2e_test_config("desktop-target-2");
        let target_identity_path = target_config.lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_config.desktop_id,
        )
        .expect("create target identity");
        let target_state = Arc::new(AppState::new(target_config.clone()));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret("the-real-secret");
            store.accept_inbound("desktop-source-2", &hash, "uid-1", "development", now_ms);
            store.save(&target_store_path).expect("seed target trust");
        }

        let listener_addr =
            super::super::lan_listener::spawn_for_test(Arc::clone(&target_state)).await;
        let candidate = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            listener_addr.port(),
        );

        let source_config = lan_e2e_test_config("desktop-source-2");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate("desktop-target-2".to_string(), candidate);
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-target-2",
                    "uid-1",
                    "development",
                    // Deliberately not "the-real-secret" the target accepted.
                    || Ok("a-wrong-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-target-2",
                    "a-wrong-secret",
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let routed = invoke_desktop(
            Arc::clone(&source_state),
            "desktop-target-2".to_string(),
            "GET".to_string(),
            "/v1/status".to_string(),
            serde_json::Value::Null,
        )
        .await
        .expect("invoke_desktop should complete");

        assert_eq!(routed.route, RouteProvenance::Lan, "{:?}", routed.response);
        assert_eq!(
            routed.response.status, 401,
            "a mismatched bearer secret must be answered definitely, not treated as a connection failure: {:?}",
            routed.response
        );
    }

    #[test]
    fn preflight_negative_falls_back_to_relay() {
        let outcome = LanAttemptOutcome::PreDispatch("no candidate".to_string());
        assert!(resolve_lan_outcome(outcome).is_none());
    }

    #[test]
    fn before_send_failure_falls_back_to_relay_identically_to_no_candidate() {
        let outcome = LanAttemptOutcome::PreDispatch("connection refused".to_string());
        assert!(resolve_lan_outcome(outcome).is_none());
    }

    #[test]
    fn after_send_uncertainty_never_falls_back_and_reports_delivery_uncertain() {
        let routed = resolve_lan_outcome(LanAttemptOutcome::PostDispatchUncertain)
            .expect("uncertain delivery is terminal, not a fallback trigger");
        assert_eq!(routed.route, RouteProvenance::Lan);
        assert_eq!(
            routed.response.error.as_deref(),
            Some("delivery_uncertain")
        );
    }

    #[test]
    fn a_definite_response_propagates_even_when_it_is_an_application_error() {
        let response = HttpInvokeResponse {
            status: 403,
            body: Some(serde_json::json!({"error": "forbidden"})),
            error: None,
        };
        let routed = resolve_lan_outcome(LanAttemptOutcome::Definite(response.clone()))
            .expect("a definite response is terminal");
        assert_eq!(routed.route, RouteProvenance::Lan);
        assert_eq!(routed.response, response);
    }

    #[test]
    fn a_definite_success_response_also_never_falls_back() {
        let response = HttpInvokeResponse {
            status: 200,
            body: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let routed = resolve_lan_outcome(LanAttemptOutcome::Definite(response.clone()))
            .expect("a definite response is terminal regardless of status");
        assert_eq!(routed.response, response);
    }

    #[test]
    fn route_provenance_reports_the_expected_strings() {
        assert_eq!(RouteProvenance::Local.as_str(), "local");
        assert_eq!(RouteProvenance::Lan.as_str(), "lan");
        assert_eq!(RouteProvenance::Relay.as_str(), "relay");
    }
}
