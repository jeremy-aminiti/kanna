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
#[derive(Debug)]
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

/// Discovered desktop_ids this desktop could actually reach over LAN right
/// now: discovery's own candidate list, narrowed to the ones with a
/// currently-usable outbound grant under the exact same binding
/// `attempt_lan_invoke` itself requires (account, environment, local
/// identity, protocol version, unexpired). This is the "one eligible-machine
/// enumeration" every list/wait/stats/signal fanout consumer adds to its own
/// relay-presence ids, so a trusted discovered LAN peer is never dropped
/// from machine discovery merely because relay happens to be down - see
/// `cloud_desktops::list_cloud_desktops` for the first caller. Discovery
/// itself is never authority: an id only appears here because both a
/// candidate address *and* an already-established trust grant exist for it,
/// the same two facts `attempt_lan_invoke` itself checks before ever
/// dialing.
pub(crate) fn eligible_lan_desktop_ids(state: &Arc<AppState>) -> Vec<String> {
    let Some(store_path) = state.config().machine_trust_store_path() else {
        return Vec::new();
    };
    let Ok(now_ms) = crate::machine_trust::unix_time_ms() else {
        return Vec::new();
    };
    let current_account_uid = state.authenticated_account_uid();
    let Ok(store) = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path) else {
        return Vec::new();
    };
    state
        .lan_candidate_desktop_ids()
        .into_iter()
        .filter(|desktop_id| {
            store
                .outbound_grant_for(
                    desktop_id,
                    current_account_uid.as_deref(),
                    &state.config().environment,
                    &state.config().desktop_id,
                    now_ms,
                )
                .is_some()
        })
        .collect()
}

/// The shared merge every list/wait/stats/signal fan-out consumer needs:
/// relay's own active-desktop listing, extended unconditionally with
/// [`eligible_lan_desktop_ids`] so a trusted discovered LAN peer is never
/// dropped merely because relay happens to be unavailable - sorted and
/// deduplicated. The relay listing's own error, if any, is returned
/// alongside rather than folded away: some consumers must still surface it as
/// their own outage dimension (`cloud_desktops`'s `relay_available`/`error`,
/// `machine_stats`'s `machine_errors`), and one (`signal_agent`'s singleton
/// resolution) must fail closed on it when the merged id list is also empty,
/// so no caller can be made silently to swallow a real relay fault.
pub(crate) async fn relay_and_lan_desktop_ids(
    state: &Arc<AppState>,
) -> (Vec<String>, Option<String>) {
    let (mut ids, error) = match state.list_active_relay_desktops().await {
        Ok(ids) => (ids, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    ids.extend(eligible_lan_desktop_ids(state));
    ids.sort();
    ids.dedup();
    (ids, error)
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
    let Some(grant) = store.outbound_grant_for(
        desktop_id,
        current_account_uid.as_deref(),
        &state.config().environment,
        &state.config().desktop_id,
        now_ms,
    ) else {
        maybe_trigger_lan_bootstrap(state, desktop_id, current_account_uid.as_deref());
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

    dial_lan_invoke(LanDialRequest {
        this_desktop_id: &state.config().desktop_id,
        target_desktop_id: desktop_id,
        candidate,
        trust_anchor_pem: &trust_anchor_pem,
        bearer_secret: &bearer_secret,
        method,
        path,
        body,
    })
    .await
}

/// Opportunistically starts (or renews) outbound LAN trust with `desktop_id`
/// in the background, from the server-owned routing boundary itself rather
/// than a caller or a timer: the moment an actual operation needs a grant
/// that does not exist yet or has expired is exactly when establishing one
/// is worth the relay round trip, and covers first-use and renewal with the
/// same trigger. Never blocks or affects *this* invoke's own outcome - the
/// current attempt already fell back to relay regardless of what this does.
///
/// `begin_lan_bootstrap_attempt`'s in-flight guard is what keeps a burst of
/// calls for the same target (e.g. several requests before a slow relay
/// round trip completes) from starting more than one concurrent bootstrap -
/// deliberately not a scheduler or a retry timer: nothing here decides *when*
/// to try again beyond "the next time an operation needs this grant."
/// `lan_bootstrap::request_bootstrap`'s own durable pending-record idempotence
/// (see `machine_trust::MachineTrustStore::pending_or_create`) is what makes
/// a lost acknowledgement converge without duplicating trust.
fn maybe_trigger_lan_bootstrap(
    state: &Arc<AppState>,
    target_desktop_id: &str,
    current_account_uid: Option<&str>,
) {
    let Some(account_uid) = current_account_uid else {
        // Signed out: there is no account to bootstrap trust under.
        return;
    };
    if !state.begin_lan_bootstrap_attempt(target_desktop_id) {
        return;
    }
    let state = Arc::clone(state);
    let target_desktop_id = target_desktop_id.to_string();
    let account_uid = account_uid.to_string();
    tokio::spawn(async move {
        let result = super::lan_bootstrap::request_bootstrap(
            Arc::clone(&state),
            target_desktop_id.clone(),
            &account_uid,
        )
        .await;
        state.finish_lan_bootstrap_attempt(&target_desktop_id);
        if let Err(error) = result {
            log::warn!("LAN bootstrap of {target_desktop_id} failed: {error}");
        }
    });
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

/// The short-RPC budget for every wrapped call except a long-poll wait.
/// Mirrors the relay transport's own short-invoke budget (see
/// `RelayHttpInvokePermits`), which this module has no direct dependency on
/// but deliberately agrees with: both exist to keep an ordinary machine
/// operation from hanging past a caller's own patience.
const LAN_SHORT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The wait budget for a long-poll `/v1/task-events` request: the server on
/// the other end may legitimately hold the connection open for up to
/// `kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS`, so the transport timeout must
/// exceed that ceiling rather than truncate a healthy wait - the margin
/// covers connect/response overhead and this attempt's own recheck cadence,
/// not another wait window layered on top.
fn lan_request_timeout(path: &str) -> std::time::Duration {
    // Mirrors `RelayHttpInvokePermits::for_path`'s own classification of
    // this exact path - a separate, independently-owned budget for a
    // different transport, deliberately agreeing on which paths are
    // long-lived rather than sharing a type across the two modules.
    if path.split('?').next() == Some("/v1/task-events") {
        std::time::Duration::from_secs(kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS + 30)
    } else {
        LAN_SHORT_REQUEST_TIMEOUT
    }
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
///
/// Redirects are disabled: a 3xx is a definite response like any other
/// (`resolve_lan_outcome`'s own contract), never a signal to transparently
/// resend - possibly to a plaintext destination, possibly replaying a
/// bearer secret and application bytes onto a connection this desktop never
/// chose and never authenticated. `reqwest`'s default policy follows up to
/// 10 redirects; refusing that keeps every dispatch to exactly the one
/// pinned-TLS connection this function itself established.
/// Bundles `dial_lan_invoke`'s parameters - every one of them a distinct
/// fact the caller already resolved (an already-attested trust anchor and
/// bearer secret, an already-discovered candidate address, the wrapped
/// method/path/body) and none of them related enough to merge, so a struct
/// rather than fewer, wider parameters.
struct LanDialRequest<'a> {
    this_desktop_id: &'a str,
    target_desktop_id: &'a str,
    candidate: std::net::SocketAddr,
    trust_anchor_pem: &'a str,
    bearer_secret: &'a str,
    method: &'a str,
    path: &'a str,
    body: &'a serde_json::Value,
}

async fn dial_lan_invoke(request: LanDialRequest<'_>) -> LanAttemptOutcome {
    let LanDialRequest {
        this_desktop_id,
        target_desktop_id,
        candidate,
        trust_anchor_pem,
        bearer_secret,
        method,
        path,
        body,
    } = request;
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
        .timeout(lan_request_timeout(path))
        .redirect(reqwest::redirect::Policy::none())
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
            lan_routing_port: 4460,
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
            &target_config.environment,
        )
        .expect("create target identity");

        let target_state = Arc::new(AppState::new(target_config.clone()));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret("the-bearer-secret");
            store.accept_inbound(
                "desktop-source",
                &hash,
                "uid-1",
                "development",
                &target_config.desktop_id,
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
                    &source_config.desktop_id,
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-target",
                    "the-bearer-secret",
                    &source_config.desktop_id,
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

    /// Qualifies the pinned-TLS chain at the *same* real, routable interface
    /// address the LAN listener reachability test
    /// (`lan_listener::tests::listener_bound_to_all_interfaces_is_reachable_on_a_real_routable_address`)
    /// and the discovery investigation both used - not loopback. That
    /// listener-reachability test proves only a raw TCP connect succeeds at
    /// the real address; the test directly above proves the full pinned-TLS
    /// chain but only over loopback. Neither, nor the two together,
    /// substitutes for the other: loopback traverses the kernel's loopback
    /// fast path and never exercises the real network stack/interface a
    /// discovered candidate's dial actually would. Pinned-TLS identity here
    /// (`server_name_for_desktop`) is derived purely from `desktop_id`, never
    /// from the IP dialed, and `dial_lan_invoke`'s `TcpStream::connect`
    /// takes whatever `SocketAddr` the candidate carries - so retargeting
    /// the exact same chain at a real interface address, instead of
    /// loopback, is a faithful, minimal same-address qualification, not a
    /// different mechanism. A no-op (not a failure) on a host with no real
    /// non-loopback interface, matching the listener-reachability test's own
    /// portability rule.
    #[tokio::test]
    async fn a_real_lan_invoke_completes_over_a_real_tls_socket_on_the_same_real_routable_address()
    {
        let Some(real_ip) = if_addrs::get_if_addrs().ok().and_then(|interfaces| {
            interfaces
                .into_iter()
                .map(|interface| interface.addr.ip())
                .find(crate::lan_discovery::is_routable_lan_address)
        }) else {
            eprintln!(
                "skipping: no non-loopback interface on this host to qualify same-address pinned TLS on"
            );
            return;
        };

        let mut target_config = lan_e2e_test_config("desktop-target-real-addr");
        target_config.lan_host = "0.0.0.0".to_string();
        let target_identity_path = target_config.lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_config.desktop_id,
            &target_config.environment,
        )
        .expect("create target identity");

        let target_state = Arc::new(AppState::new(target_config.clone()));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret("the-bearer-secret");
            store.accept_inbound(
                "desktop-source-real-addr",
                &hash,
                "uid-1",
                "development",
                &target_config.desktop_id,
                now_ms,
            );
            store.save(&target_store_path).expect("seed target trust");
        }

        let listener_addr =
            super::super::lan_listener::spawn_for_test(Arc::clone(&target_state)).await;
        // The one deliberate difference from the loopback test above: the
        // real, routable interface address, not `Ipv4Addr::LOCALHOST`.
        let candidate = std::net::SocketAddr::new(real_ip, listener_addr.port());

        let source_config = lan_e2e_test_config("desktop-source-real-addr");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate("desktop-target-real-addr".to_string(), candidate);
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-target-real-addr",
                    "uid-1",
                    "development",
                    &source_config.desktop_id,
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-target-real-addr",
                    "the-bearer-secret",
                    &source_config.desktop_id,
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let routed = invoke_desktop(
            Arc::clone(&source_state),
            "desktop-target-real-addr".to_string(),
            "GET".to_string(),
            "/v1/status".to_string(),
            serde_json::Value::Null,
        )
        .await
        .expect("invoke_desktop should complete");

        assert_eq!(
            routed.route,
            RouteProvenance::Lan,
            "same-address pinned-TLS dial to {candidate} did not route over LAN: {:?}",
            routed.response
        );
        assert_eq!(routed.response.status, 200, "{:?}", routed.response);
        let body = routed.response.body.expect("status response body");
        assert_eq!(body["desktopId"], "desktop-target-real-addr");
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
            &target_config.environment,
        )
        .expect("create target identity");
        let target_state = Arc::new(AppState::new(target_config.clone()));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret("the-real-secret");
            store.accept_inbound(
                "desktop-source-2",
                &hash,
                "uid-1",
                "development",
                &target_config.desktop_id,
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
                    &source_config.desktop_id,
                    // Deliberately not "the-real-secret" the target accepted.
                    || Ok("a-wrong-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-target-2",
                    "a-wrong-secret",
                    &source_config.desktop_id,
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

    /// A real pinned-TLS peer answering with a 3xx must be a definite
    /// response, not an automatically-followed redirect: `reqwest`'s default
    /// policy follows up to 10 redirects, which could resend the bearer
    /// secret and application bytes to a destination this desktop never
    /// authenticated - possibly plaintext. Proven at a real socket: a
    /// minimal TLS responder presenting the target's actual attested
    /// identity, counting exactly how many requests it receives.
    #[tokio::test]
    async fn a_redirect_from_the_gateway_is_definite_with_no_follow_up_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let target_config = lan_e2e_test_config("desktop-redirect-target");
        let target_identity_path = target_config.lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_config.desktop_id,
            &target_config.environment,
        )
        .expect("create target identity");
        let server_config = crate::lan_tls::server_config(&target_identity).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind minimal redirect responder");
        let addr = listener.local_addr().unwrap();
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count_for_server = Arc::clone(&request_count);
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let request_count = Arc::clone(&request_count_for_server);
                tokio::spawn(async move {
                    request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut buf = [0_u8; 4096];
                    let _ = tls.read(&mut buf).await;
                    let body = "moved";
                    let response = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/elsewhere\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body,
                    );
                    let _ = tls.write_all(response.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });

        let source_config = lan_e2e_test_config("desktop-redirect-source");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate(
            "desktop-redirect-target".to_string(),
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                addr.port(),
            ),
        );
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-redirect-target",
                    "uid-1",
                    "development",
                    &source_config.desktop_id,
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-redirect-target",
                    "the-bearer-secret",
                    &source_config.desktop_id,
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let routed = invoke_desktop(
            Arc::clone(&source_state),
            "desktop-redirect-target".to_string(),
            "GET".to_string(),
            "/v1/status".to_string(),
            serde_json::Value::Null,
        )
        .await
        .expect("invoke_desktop should complete");

        assert_eq!(routed.route, RouteProvenance::Lan, "{:?}", routed.response);
        assert_eq!(routed.response.status, 302, "{:?}", routed.response);
        // Give an errant automatic follow-up a moment to land, if the
        // redirect policy were not actually disabled.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            request_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a redirect must never trigger a second request"
        );
    }

    /// Dropped-reply-exactly-once, over a real socket. A real pinned-TLS
    /// handshake completes, the target genuinely receives the dispatched
    /// request, then the connection is closed with zero response bytes
    /// written - simulating a peer crash or network drop *after* dispatch,
    /// never before it. `dial_lan_invoke`'s own contract
    /// (`error.is_connect()` is false once the handshake succeeded, so this
    /// is deliberately classified `PostDispatchUncertain`, not
    /// `PreDispatch`) must report this as `delivery_uncertain` and must
    /// never fall back to relay - replaying a mutation the peer may already
    /// have applied is exactly the risk that contract exists to avoid. This
    /// does not depend on real mDNS discovery resolving anything: like the
    /// redirect test above, it seeds an explicit candidate at a real raw
    /// TLS socket, the same production dial/pinning code every other real
    /// test here exercises.
    #[tokio::test]
    async fn a_dropped_reply_after_a_real_dispatch_is_delivery_uncertain_and_never_replayed_to_relay(
    ) {
        use tokio::io::AsyncReadExt;

        let target_config = lan_e2e_test_config("desktop-dropped-reply-target");
        let target_identity_path = target_config.lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_config.desktop_id,
            &target_config.environment,
        )
        .expect("create target identity");
        let server_config = crate::lan_tls::server_config(&target_identity).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind minimal dropped-reply responder");
        let addr = listener.local_addr().unwrap();
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count_for_server = Arc::clone(&request_count);
        let bytes_received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let bytes_received_for_server = Arc::clone(&bytes_received);
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let request_count = Arc::clone(&request_count_for_server);
                let bytes_received = Arc::clone(&bytes_received_for_server);
                tokio::spawn(async move {
                    request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    // Drain whatever the real client actually sent - proof
                    // the request was genuinely dispatched, not dropped
                    // before it ever reached the peer - then close without
                    // writing a single response byte.
                    let mut buf = [0_u8; 4096];
                    let mut total = 0_usize;
                    while let Ok(Ok(n)) = tokio::time::timeout(
                        std::time::Duration::from_millis(300),
                        tls.read(&mut buf),
                    )
                    .await
                    {
                        if n == 0 {
                            break;
                        }
                        total += n;
                    }
                    bytes_received.store(total, std::sync::atomic::Ordering::SeqCst);
                    // Deliberately no write_all/shutdown with a response -
                    // just drop the stream, closing the connection with
                    // nothing sent back.
                });
            }
        });

        let source_config = lan_e2e_test_config("desktop-dropped-reply-source");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate(
            "desktop-dropped-reply-target".to_string(),
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                addr.port(),
            ),
        );
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-dropped-reply-target",
                    "uid-1",
                    "development",
                    &source_config.desktop_id,
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-dropped-reply-target",
                    "the-bearer-secret",
                    &source_config.desktop_id,
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let routed = invoke_desktop(
            Arc::clone(&source_state),
            "desktop-dropped-reply-target".to_string(),
            "GET".to_string(),
            "/v1/status".to_string(),
            serde_json::Value::Null,
        )
        .await
        .expect(
            "invoke_desktop must complete with a terminal uncertain result, not an error \
             (which would mean it fell back to relay - relay is unconfigured in this test, so \
             a fallback attempt would itself fail)",
        );

        assert!(
            bytes_received.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "the target must have genuinely received the dispatched request before its reply \
             was dropped, not merely refused the connection"
        );
        assert_eq!(routed.route, RouteProvenance::Lan, "{:?}", routed.response);
        assert_eq!(routed.response.status, 0, "{:?}", routed.response);
        assert_eq!(
            routed.response.error.as_deref(),
            Some("delivery_uncertain"),
            "a reply dropped after real dispatch must be reported as delivery_uncertain: {:?}",
            routed.response
        );
        assert_eq!(
            request_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an uncertain delivery must never be retried automatically on the same LAN route - \
             exactly once, no replay"
        );
    }

    /// Fake-discovery/pinned-TLS rejection, through the full production
    /// dial path (`attempt_lan_invoke`/`dial_lan_invoke`), not just
    /// `lan_tls`'s own lower-level handshake unit tests
    /// (`a_client_pinned_to_an_unrelated_ca_is_rejected_before_any_application_byte`).
    /// A discovered candidate address is never itself trusted - it is only
    /// ever "where to try connecting" - so a real socket at that address
    /// presenting a genuinely different desktop's real, valid TLS identity
    /// (simulating an impersonator discovered where the real target was
    /// expected, e.g. a spoofed or stale candidate) must be rejected before
    /// any application byte crosses, and the rejection must surface as an
    /// ordinary `PreDispatch` - safe to fall back to relay - never a
    /// `Definite` or `PostDispatchUncertain` result. Uses `attempt_lan_invoke`
    /// directly (as `no_outbound_grant_triggers_a_real_background_bootstrap_attempt`
    /// does) rather than the full `invoke_desktop` wrapper, since relay is
    /// unconfigured in these tests and this assertion is about the LAN
    /// attempt's own outcome, not the relay fallback.
    #[tokio::test]
    async fn a_candidate_presenting_a_different_desktops_real_identity_is_rejected_before_dispatch()
    {
        let real_target_config = lan_e2e_test_config("desktop-fake-discovery-real-target");
        let real_target_identity_path = real_target_config.lan_tls_identity_path().unwrap();
        let real_target_identity = crate::lan_tls_identity::load_or_create(
            &real_target_identity_path,
            &real_target_config.desktop_id,
            &real_target_config.environment,
        )
        .expect("create real target identity");

        // A genuinely different desktop's own real, validly-issued identity -
        // not a corrupt cert, not `dangerous()`, a real impersonator with a
        // real (but wrong) CA, exactly what a spoofed/stale discovered
        // candidate would actually look like on the wire.
        let impersonator_config = lan_e2e_test_config("desktop-fake-discovery-impersonator");
        let impersonator_identity_path = impersonator_config.lan_tls_identity_path().unwrap();
        let impersonator_identity = crate::lan_tls_identity::load_or_create(
            &impersonator_identity_path,
            &impersonator_config.desktop_id,
            &impersonator_config.environment,
        )
        .expect("create impersonator identity");
        let impersonator_server_config =
            crate::lan_tls::server_config(&impersonator_identity).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind impersonator responder");
        let addr = listener.local_addr().unwrap();
        let application_bytes_reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let application_bytes_reached_for_server = Arc::clone(&application_bytes_reached);
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(impersonator_server_config);
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let application_bytes_reached = Arc::clone(&application_bytes_reached_for_server);
                tokio::spawn(async move {
                    // The handshake itself is expected to fail (the client
                    // pins to the *real* target's CA, not this
                    // impersonator's) - if it were ever to succeed and any
                    // byte were read afterward, that is exactly the failure
                    // this test exists to catch.
                    if let Ok(mut tls) = acceptor.accept(stream).await {
                        use tokio::io::AsyncReadExt;
                        let mut buf = [0_u8; 4096];
                        if let Ok(n) = tls.read(&mut buf).await {
                            if n > 0 {
                                application_bytes_reached
                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    }
                });
            }
        });

        let source_config = lan_e2e_test_config("desktop-fake-discovery-source");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        // The candidate address is where the impersonator actually listens -
        // exactly what a spoofed/stale discovery result would hand this
        // desktop; discovery supplies only the address, never the identity.
        source_state.set_lan_candidate(
            "desktop-fake-discovery-real-target".to_string(),
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                addr.port(),
            ),
        );
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-fake-discovery-real-target",
                    "uid-1",
                    "development",
                    &source_config.desktop_id,
                    || Ok("the-bearer-secret".to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-fake-discovery-real-target",
                    "the-bearer-secret",
                    &source_config.desktop_id,
                    // The attested CA is the *real* target's - never the
                    // impersonator's - exactly what a relay bootstrap
                    // would actually have attested for the real desktop_id.
                    Some(real_target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let outcome = attempt_lan_invoke(
            &source_state,
            "desktop-fake-discovery-real-target",
            "GET",
            "/v1/status",
            &serde_json::Value::Null,
        )
        .await;

        assert!(
            matches!(outcome, LanAttemptOutcome::PreDispatch(_)),
            "a candidate presenting a different desktop's real identity must be rejected as an \
             ordinary pre-dispatch failure (safe to fall back to relay), not treated as a \
             successful or uncertain result: {outcome:?}"
        );
        // Give the impersonator's own accept task a moment to have read
        // anything, if the handshake had wrongly succeeded.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !application_bytes_reached.load(std::sync::atomic::Ordering::SeqCst),
            "no application byte must ever reach an impersonating candidate"
        );
    }

    /// A transparent byte-level pass-through in front of the *real* target
    /// listener - it never terminates TLS itself, only relays whatever
    /// bytes arrive in each direction while capturing a copy - so a real
    /// TLS handshake and the real application dispatch happen end to end
    /// between the genuine client path and the genuine `lan_listener`,
    /// with this proxy sitting exactly where a network intermediary (a
    /// switch, a captor on the LAN segment) would. Also does not depend on
    /// real mDNS discovery: the candidate is an explicit address, as in
    /// every other real-socket test in this module.
    async fn spawn_capturing_tcp_proxy(
        target_addr: std::net::SocketAddr,
    ) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<u8>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind capturing proxy");
        let proxy_addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_task = Arc::clone(&captured);
        tokio::spawn(async move {
            while let Ok((mut client_stream, _)) = listener.accept().await {
                let captured = Arc::clone(&captured_for_task);
                tokio::spawn(async move {
                    let Ok(mut target_stream) = tokio::net::TcpStream::connect(target_addr).await
                    else {
                        return;
                    };
                    let (mut client_r, mut client_w) = client_stream.split();
                    let (mut target_r, mut target_w) = target_stream.split();
                    let client_to_target = async {
                        let mut buf = [0_u8; 4096];
                        loop {
                            let n = client_r.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            captured
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .extend_from_slice(&buf[..n]);
                            if target_w.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    let target_to_client = async {
                        let mut buf = [0_u8; 4096];
                        loop {
                            let n = target_r.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            captured
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .extend_from_slice(&buf[..n]);
                            if client_w.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    tokio::join!(client_to_target, target_to_client);
                });
            }
        });
        (proxy_addr, captured)
    }

    /// Encrypted-proxy-bytes: every byte a network intermediary in front of
    /// a real LAN dial ever observes must be TLS ciphertext - the bearer
    /// secret this dial's own header carries, the wrapped path, and the
    /// device-secret header name itself must never appear on the wire in
    /// the clear. Proven over the real production listener
    /// (`lan_listener::spawn_for_test`, the same one the passing
    /// end-to-end test uses) with a transparent capturing proxy
    /// (`spawn_capturing_tcp_proxy`) standing in for the candidate address,
    /// rather than by asserting anything about TLS in the abstract.
    #[tokio::test]
    async fn every_byte_a_lan_proxy_observes_is_encrypted_never_plaintext_secrets_or_paths() {
        let target_config = lan_e2e_test_config("desktop-encrypted-proxy-target");
        let target_identity_path = target_config.lan_tls_identity_path().unwrap();
        let target_identity = crate::lan_tls_identity::load_or_create(
            &target_identity_path,
            &target_config.desktop_id,
            &target_config.environment,
        )
        .expect("create target identity");
        let target_state = Arc::new(AppState::new(target_config.clone()));
        target_state.set_authenticated_account_uid(Some("uid-1".to_string()));

        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let target_store_path = target_config.machine_trust_store_path().unwrap();
        let bearer_secret = "the-encrypted-proxy-bearer-secret";
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            let hash = crate::pairing::hash_device_secret(bearer_secret);
            store.accept_inbound(
                "desktop-encrypted-proxy-source",
                &hash,
                "uid-1",
                "development",
                &target_config.desktop_id,
                now_ms,
            );
            store.save(&target_store_path).expect("seed target trust");
        }

        let listener_addr =
            super::super::lan_listener::spawn_for_test(Arc::clone(&target_state)).await;
        let (proxy_addr, captured) = spawn_capturing_tcp_proxy(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            listener_addr.port(),
        ))
        .await;

        let source_config = lan_e2e_test_config("desktop-encrypted-proxy-source");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        source_state.set_lan_candidate("desktop-encrypted-proxy-target".to_string(), proxy_addr);
        let source_store_path = source_config.machine_trust_store_path().unwrap();
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-encrypted-proxy-target",
                    "uid-1",
                    "development",
                    &source_config.desktop_id,
                    || Ok(bearer_secret.to_string()),
                    now_ms,
                )
                .expect("prepare pending");
            store
                .confirm_outbound(
                    "desktop-encrypted-proxy-target",
                    bearer_secret,
                    &source_config.desktop_id,
                    Some(target_identity.ca_certificate_pem.clone()),
                    now_ms + 1000,
                )
                .expect("confirm outbound grant");
            store.save(&source_store_path).expect("seed source trust");
        }

        let routed = invoke_desktop(
            Arc::clone(&source_state),
            "desktop-encrypted-proxy-target".to_string(),
            "GET".to_string(),
            "/v1/status".to_string(),
            serde_json::Value::Null,
        )
        .await
        .expect("invoke_desktop should complete through the capturing proxy");

        assert_eq!(routed.route, RouteProvenance::Lan, "{:?}", routed.response);
        assert_eq!(routed.response.status, 200, "{:?}", routed.response);

        let wire_bytes = captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            !wire_bytes.is_empty(),
            "the proxy must have actually observed real traffic, not an empty capture"
        );
        for plaintext_secret in [
            bearer_secret,
            super::super::lan_trust::DEVICE_SECRET_HEADER,
            "/v1/status",
        ] {
            assert!(
                !contains_subsequence(&wire_bytes, plaintext_secret.as_bytes()),
                "found {plaintext_secret:?} verbatim in {} bytes the proxy observed on the wire - \
                 the LAN dial is not actually encrypted",
                wire_bytes.len()
            );
        }
    }

    fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || haystack.len() < needle.len() {
            return false;
        }
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// Finding #2's production wiring, proven through the actual routing
    /// boundary rather than by manually seeding an outbound grant: with no
    /// grant and no candidate for a target, `attempt_lan_invoke` itself must
    /// kick off a real background bootstrap attempt using the real
    /// `lan_bootstrap`/`machine_trust` code paths. The relay call inside it
    /// fails fast (no relay connection in this test), but `request_bootstrap`
    /// durably records its pending candidate *before* ever touching the
    /// network - so that pending record's real presence on disk is direct
    /// proof the production wiring ran, not a simulation of it.
    #[tokio::test]
    async fn no_outbound_grant_triggers_a_real_background_bootstrap_attempt() {
        let source_config = lan_e2e_test_config("desktop-bootstrap-source");
        let source_state = Arc::new(AppState::new(source_config.clone()));
        source_state.set_authenticated_account_uid(Some("uid-1".to_string()));
        let store_path = source_config.machine_trust_store_path().unwrap();

        let outcome = attempt_lan_invoke(
            &source_state,
            "desktop-bootstrap-target",
            "GET",
            "/v1/status",
            &serde_json::Value::Null,
        )
        .await;
        assert!(
            matches!(outcome, LanAttemptOutcome::PreDispatch(_)),
            "no grant yet must still fall back to relay from this attempt's own perspective"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(store) =
                crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            {
                if store
                    .pending
                    .iter()
                    .any(|pending| pending.target_desktop_id == "desktop-bootstrap-target")
                {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "no pending bootstrap record appeared for desktop-bootstrap-target - \
                     the production request_bootstrap wiring did not run"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The in-flight guard itself: a burst of calls for the same target
    /// while one bootstrap attempt is already running must not each start
    /// their own concurrent attempt.
    #[test]
    fn lan_bootstrap_in_flight_guard_admits_only_one_concurrent_attempt_per_target() {
        let config = lan_e2e_test_config("desktop-guard");
        let state = Arc::new(AppState::new(config));

        assert!(state.begin_lan_bootstrap_attempt("desktop-target"));
        assert!(
            !state.begin_lan_bootstrap_attempt("desktop-target"),
            "a second concurrent attempt for the same target must be refused"
        );
        assert!(
            state.begin_lan_bootstrap_attempt("desktop-other"),
            "a different target must not be blocked by an unrelated in-flight attempt"
        );

        state.finish_lan_bootstrap_attempt("desktop-target");
        assert!(
            state.begin_lan_bootstrap_attempt("desktop-target"),
            "once finished, the same target may be attempted again"
        );
    }

    #[test]
    fn lan_request_timeout_uses_the_long_poll_budget_for_task_events() {
        let short = lan_request_timeout("/v1/status");
        let long_poll = lan_request_timeout("/v1/task-events?timeoutSecs=240");

        assert_eq!(short, LAN_SHORT_REQUEST_TIMEOUT);
        assert!(
            long_poll > std::time::Duration::from_secs(kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS),
            "the long-poll budget must exceed the longest wait a caller can actually request"
        );
        assert!(
            long_poll > short,
            "a long-poll request must never be truncated to the short-RPC budget"
        );
    }

    #[test]
    fn eligible_lan_desktop_ids_requires_both_a_candidate_and_a_usable_grant() {
        let config = lan_e2e_test_config("desktop-eligible-source");
        let state = Arc::new(AppState::new(config.clone()));
        state.set_authenticated_account_uid(Some("uid-1".to_string()));
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let store_path = config.machine_trust_store_path().unwrap();

        // "desktop-with-grant" has both a candidate and a real grant.
        state.set_lan_candidate(
            "desktop-with-grant".to_string(),
            "127.0.0.1:1".parse().unwrap(),
        );
        // "desktop-candidate-only" has a candidate but no grant at all.
        state.set_lan_candidate(
            "desktop-candidate-only".to_string(),
            "127.0.0.1:2".parse().unwrap(),
        );
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-with-grant",
                    "uid-1",
                    "development",
                    &config.desktop_id,
                    || Ok("secret".to_string()),
                    now_ms,
                )
                .unwrap();
            store
                .confirm_outbound(
                    "desktop-with-grant",
                    "secret",
                    &config.desktop_id,
                    Some("fake-ca".to_string()),
                    now_ms + 1000,
                )
                .unwrap();
            // "desktop-expired-grant" has a candidate and a grant, but it has
            // already expired.
            store
                .pending_or_create(
                    "desktop-expired-grant",
                    "uid-1",
                    "development",
                    &config.desktop_id,
                    || Ok("expired-secret".to_string()),
                    now_ms,
                )
                .unwrap();
            store
                .confirm_outbound(
                    "desktop-expired-grant",
                    "expired-secret",
                    &config.desktop_id,
                    Some("fake-ca".to_string()),
                    now_ms,
                )
                .unwrap();
            store.save(&store_path).unwrap();
        }
        state.set_lan_candidate(
            "desktop-expired-grant".to_string(),
            "127.0.0.1:3".parse().unwrap(),
        );

        let eligible = eligible_lan_desktop_ids(&state);

        assert_eq!(eligible, vec!["desktop-with-grant".to_string()]);
    }

    #[test]
    fn eligible_lan_desktop_ids_is_empty_when_signed_out() {
        let config = lan_e2e_test_config("desktop-eligible-signed-out");
        let state = Arc::new(AppState::new(config));
        // Deliberately never calling set_authenticated_account_uid.
        state.set_lan_candidate(
            "desktop-with-grant".to_string(),
            "127.0.0.1:1".parse().unwrap(),
        );

        assert!(eligible_lan_desktop_ids(&state).is_empty());
    }

    /// Every list/wait/stats/signal fan-out consumer (`task_events`,
    /// `tasks`, `signal_agent`, `machine_stats`, `cloud_desktops`) is meant
    /// to reach eligible LAN peers through this one merge point. A relay
    /// listing failure (a fresh `AppState` has no relay connection at all,
    /// exactly like a real outage) must still surface its own error, but it
    /// must not silently drop a trusted, currently discovered LAN peer from
    /// the merged id list.
    #[tokio::test]
    async fn relay_and_lan_desktop_ids_merges_a_trusted_lan_peer_through_a_relay_outage() {
        let config = lan_e2e_test_config("desktop-merge-source");
        let state = Arc::new(AppState::new(config.clone()));
        state.set_authenticated_account_uid(Some("uid-1".to_string()));
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        let store_path = config.machine_trust_store_path().unwrap();
        state.set_lan_candidate(
            "desktop-lan-peer".to_string(),
            "127.0.0.1:1".parse().unwrap(),
        );
        {
            let mut store = crate::machine_trust::MachineTrustStore::default();
            store
                .pending_or_create(
                    "desktop-lan-peer",
                    "uid-1",
                    "development",
                    &config.desktop_id,
                    || Ok("secret".to_string()),
                    now_ms,
                )
                .unwrap();
            store
                .confirm_outbound(
                    "desktop-lan-peer",
                    "secret",
                    &config.desktop_id,
                    Some("fake-ca".to_string()),
                    now_ms + 1000,
                )
                .unwrap();
            store.save(&store_path).unwrap();
        }

        let (ids, error) = relay_and_lan_desktop_ids(&state).await;

        assert!(error.is_some(), "relay's own outage must still be reported");
        assert_eq!(
            ids,
            vec!["desktop-lan-peer".to_string()],
            "a trusted discovered LAN peer must still be merged in despite the relay outage"
        );
    }

    /// The negative case a fail-closed consumer (`signal_agent`) relies on:
    /// with no relay listing and no eligible LAN peer either, the merge must
    /// come back empty so a caller can tell genuine total unreachability
    /// apart from "relay is down but a LAN peer still covers this."
    #[tokio::test]
    async fn relay_and_lan_desktop_ids_is_empty_on_relay_outage_with_no_lan_peer() {
        let config = lan_e2e_test_config("desktop-merge-source-none");
        let state = Arc::new(AppState::new(config));

        let (ids, error) = relay_and_lan_desktop_ids(&state).await;

        assert!(error.is_some());
        assert!(ids.is_empty());
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
        assert_eq!(routed.response.error.as_deref(), Some("delivery_uncertain"));
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
