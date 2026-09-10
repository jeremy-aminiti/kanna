//! The single choke point for a general machine invoke to another desktop.
//!
//! Every production caller that used to reach `AppState::invoke_relay_desktop`
//! directly should call [`invoke_desktop`] instead. `invoke_relay_desktop`
//! itself is untouched and remains exactly what this module falls back to.
//!
//! The LAN attempt itself ([`attempt_lan_invoke`]) is a documented stub: a
//! real attempt needs a mutually-authenticated TLS client dialing the
//! desktop's own pinned trust anchor, which needs `rustls`/`tokio-rustls`/
//! `rcgen` added as direct dependencies of this crate - a Cargo.toml/
//! Cargo.lock change this task is holding until `cargo` gates are available
//! to verify it compiles. Until then, `invoke_desktop`'s observable behavior
//! is identical to calling `invoke_relay_desktop` directly: what's new here
//! is the shared routing seam, the fallback/uncertainty decision table, and
//! truthful route provenance - all real and unit-tested independent of the
//! transport that will eventually fill in `attempt_lan_invoke`.

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

/// Stub pending the TLS client (see module docs). Looks up whether an
/// outbound grant exists at all, purely so the eventual real attempt has
/// somewhere obvious to start; it never dials anything yet, so a grant's
/// presence changes nothing observable until this function's body is
/// replaced with a real TLS dial.
async fn attempt_lan_invoke(
    state: &Arc<AppState>,
    desktop_id: &str,
    _method: &str,
    _path: &str,
    _body: &serde_json::Value,
) -> LanAttemptOutcome {
    let Some(store_path) = state.config().machine_trust_store_path() else {
        return LanAttemptOutcome::PreDispatch("no machine trust store configured".to_string());
    };
    let current_account_uid = state.authenticated_account_uid();
    let has_grant = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
        .ok()
        .zip(crate::machine_trust::unix_time_ms().ok())
        .is_some_and(|(store, now_ms)| {
            store
                .outbound_grant_for(desktop_id, current_account_uid.as_deref(), now_ms)
                .is_some()
        });
    if !has_grant {
        return LanAttemptOutcome::PreDispatch(format!(
            "no unexpired outbound LAN grant for desktop {desktop_id}"
        ));
    }
    // TODO(lan-tls): dial the grant's pinned trust anchor over TLS and send
    // the invoke here once rustls/tokio-rustls/rcgen are added as direct
    // kanna-server dependencies. Until then, a grant's presence is
    // deliberately inert: falling back to relay is always correct because
    // nothing above has ever opened a connection.
    LanAttemptOutcome::PreDispatch("LAN transport not yet implemented".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
