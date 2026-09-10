//! The relay-authenticated same-account LAN trust bootstrap.
//!
//! `bootstrap_lan_trust` is the server-side handler: it runs on the *target*
//! and is reachable only through [`RelayAttestedSource`] - never locally,
//! never through a legacy device-token tunnel, never through a v1 relay that
//! cannot attest a source desktop at all. `request_bootstrap` is the
//! *source*-side counterpart a future discovery-driven caller uses to
//! establish outbound trust with a target it has decided to bootstrap; it
//! does not decide *when* to bootstrap (that is Bonjour discovery's job,
//! still to come) and it does not touch Bonjour, LAN sockets, or TLS at
//! all - it only talks to `machine_trust` and the existing, unchanged
//! `invoke_desktop`/relay transport.
//!
//! Nothing here touches `pairing::PairingStore`, `state.pairing_session`, or
//! push-identity material - this is a fully separate code path from the
//! human/mobile QR ceremony, per the reconciled contract.

use super::lan_trust::RelayAttestedSource;
use super::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LanBootstrapRequest {
    candidate_secret: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LanBootstrapResponse {
    ca_certificate_pem: String,
    /// The target's own attested expiry for the inbound grant it just
    /// created - the source must use this value verbatim rather than
    /// computing its own from receipt time, so a late-arriving
    /// acknowledgement cannot silently extend the effective lease beyond
    /// what the target actually granted.
    expires_at_unix_ms: u64,
}

/// Accepts an inbound same-account bootstrap. `source` is only ever
/// constructible by [`RelayAttestedSource`]'s own extractor, so
/// `source.source_desktop_id`/`source.account_uid` are exactly as
/// trustworthy as `AuthenticatedHttpInvoke` already documents them to be -
/// this handler performs no additional identity verification of its own,
/// by design: verifying identity again here would be a second place to get
/// it wrong.
pub(super) async fn bootstrap_lan_trust(
    source: RelayAttestedSource,
    State(state): State<Arc<AppState>>,
    Json(request): Json<LanBootstrapRequest>,
) -> Result<Json<LanBootstrapResponse>, (StatusCode, String)> {
    let candidate_secret = request.candidate_secret.trim();
    if candidate_secret.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "candidate secret must not be empty".to_string(),
        ));
    }

    let store_path = state.config().machine_trust_store_path().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "machine trust store is not configured".to_string(),
    ))?;
    let identity_path = state.config().lan_tls_identity_path().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "LAN TLS identity is not configured".to_string(),
    ))?;
    let identity =
        crate::lan_tls_identity::load_or_create(&identity_path, &state.config().desktop_id)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;

    let now_ms = crate::machine_trust::unix_time_ms()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let secret_hash = crate::pairing::hash_device_secret(candidate_secret);
    let expires_at_unix_ms = {
        let _guard = crate::machine_trust::persistence_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut store = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
        let inbound = store.accept_inbound(
            &source.source_desktop_id,
            &secret_hash,
            &source.account_uid,
            &state.config().environment,
            now_ms,
        );
        store
            .save(&store_path)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
        inbound.expires_at_unix_ms
    };

    Ok(Json(LanBootstrapResponse {
        ca_certificate_pem: identity.ca_certificate_pem,
        expires_at_unix_ms,
    }))
}

/// Durably records (or reuses) the candidate secret to bootstrap
/// `target_desktop_id` with, before anything is sent over the network. A
/// retry after a lost acknowledgement calls this again and gets the same
/// secret back, because the record already exists - the target's own
/// `accept_inbound` upsert is itself idempotent on that value, so a
/// duplicate delivery converges rather than accumulating.
fn prepare_bootstrap_request(
    store_path: &std::path::Path,
    target_desktop_id: &str,
    account_uid: &str,
    environment: &str,
    now_ms: u64,
) -> Result<String, String> {
    let _guard = crate::machine_trust::persistence_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = crate::machine_trust::MachineTrustStore::load_fail_closed(store_path)?;
    let pending = store.pending_or_create(
        target_desktop_id,
        account_uid,
        environment,
        crate::pairing::generate_device_secret,
        now_ms,
    )?;
    let candidate_secret = pending.candidate_secret.clone();
    store.save(store_path)?;
    Ok(candidate_secret)
}

/// Consumes a target's bootstrap acknowledgement, moving the pending
/// candidate into a confirmed outbound grant pinned to the target's
/// attested CA certificate and expiry. `candidate_secret` must be the exact
/// value this request sent (see `MachineTrustStore::confirm_outbound`'s own
/// doc comment for why matching by target alone is not safe against a
/// stale/late ack).
fn confirm_bootstrap_response(
    store_path: &std::path::Path,
    target_desktop_id: &str,
    candidate_secret: &str,
    response: LanBootstrapResponse,
) -> Result<crate::machine_trust::OutboundGrant, String> {
    let _guard = crate::machine_trust::persistence_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = crate::machine_trust::MachineTrustStore::load_fail_closed(store_path)?;
    let grant = store.confirm_outbound(
        target_desktop_id,
        candidate_secret,
        Some(response.ca_certificate_pem),
        response.expires_at_unix_ms,
    )?;
    store.save(store_path)?;
    Ok(grant)
}

/// Initiates outbound trust with `target_desktop_id`, over the unchanged
/// *relay-only* `invoke_relay_desktop` primitive - deliberately never
/// `invoke_desktop`, which will attempt LAN once that path is real.
/// Bootstrap and renewal are the control exchange that establishes LAN
/// trust in the first place; they must not themselves risk depending on an
/// as-yet-unestablished (or stale) LAN route, which is what routing them
/// through the general LAN-preferring seam would eventually do. Nothing
/// decides *when* to call this yet (that is Bonjour discovery's job, still
/// to come); this is the mechanism a future caller drives.
pub(crate) async fn request_bootstrap(
    state: Arc<AppState>,
    target_desktop_id: String,
    account_uid: &str,
) -> Result<crate::machine_trust::OutboundGrant, String> {
    let store_path = state
        .config()
        .machine_trust_store_path()
        .ok_or_else(|| "machine trust store is not configured".to_string())?;
    let environment = state.config().environment.clone();
    let now_ms = crate::machine_trust::unix_time_ms()?;

    let candidate_secret = prepare_bootstrap_request(
        &store_path,
        &target_desktop_id,
        account_uid,
        &environment,
        now_ms,
    )?;

    let response = state
        .invoke_relay_desktop(
            target_desktop_id.clone(),
            "POST".to_string(),
            "/v1/lan-routing/bootstrap".to_string(),
            serde_json::json!({ "candidateSecret": candidate_secret }),
        )
        .await?;

    if response.status != 200 {
        return Err(response.error.unwrap_or_else(|| {
            format!(
                "LAN bootstrap of {target_desktop_id} failed with HTTP {}",
                response.status
            )
        }));
    }
    let body: LanBootstrapResponse = serde_json::from_value(
        response
            .body
            .ok_or_else(|| "bootstrap acknowledgement had no body".to_string())?,
    )
    .map_err(|error| format!("invalid bootstrap acknowledgement: {error}"))?;

    confirm_bootstrap_response(&store_path, &target_desktop_id, &candidate_secret, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_api::test_support::test_state_with_seed;

    /// The handler side of the round trip, exercised exactly as a genuine
    /// relay forward would reach it: through the same
    /// `dispatch_authenticated_relay_http_invoke` a real relay-forwarded
    /// "invoke" uses, complete with the real `RelayAttestedSource` extractor
    /// gate and real `machine_trust`/`lan_tls_identity` persistence.
    #[tokio::test(flavor = "current_thread")]
    async fn the_handler_accepts_a_relay_attested_bootstrap_and_the_target_then_verifies_it() {
        let target_state = test_state_with_seed("desktop-target", "Target Mac", |_db| {});

        let response = crate::http_api::dispatch_authenticated_relay_http_invoke(
            Arc::clone(&target_state),
            "uid-1".to_string(),
            Some("desktop-source".to_string()),
            "POST",
            "/v1/lan-routing/bootstrap",
            serde_json::json!({ "candidateSecret": "the-candidate-secret" }),
        )
        .await;

        assert_eq!(response.status, 200, "{response:?}");
        let body = response.body.expect("bootstrap response body");
        let ca_certificate_pem = body["caCertificatePem"]
            .as_str()
            .expect("caCertificatePem in response");
        assert!(ca_certificate_pem.starts_with("-----BEGIN CERTIFICATE-----"));

        // The target now verifies the source's secret, but only under the
        // account it was actually bootstrapped under.
        let store_path = target_state.config().machine_trust_store_path().unwrap();
        let store =
            crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path).unwrap();
        let now_ms = crate::machine_trust::unix_time_ms().unwrap();
        assert!(store.verify_inbound(
            "desktop-source",
            "the-candidate-secret",
            Some("uid-1"),
            now_ms
        ));
        assert!(!store.verify_inbound(
            "desktop-source",
            "wrong-secret",
            Some("uid-1"),
            now_ms
        ));
        assert!(
            !store.verify_inbound("desktop-source", "the-candidate-secret", Some("uid-2"), now_ms),
            "a grant minted under one account must not verify under another"
        );
        assert!(
            !store.verify_inbound("desktop-source", "the-candidate-secret", None, now_ms),
            "signed out (no current account) must never verify anything"
        );
    }

    /// The source side's two halves, independent of any network call:
    /// preparing a request durably records a candidate secret (and a retry
    /// reuses it rather than minting a second one), and confirming a
    /// response moves it into a usable outbound grant pinned to the
    /// attested CA.
    #[test]
    fn prepare_then_confirm_produces_a_usable_outbound_grant() {
        let store_path = crate::test_paths::unique_test_path("lan-bootstrap-source-store");
        let now_ms = 1_000;

        let first_secret =
            prepare_bootstrap_request(&store_path, "desktop-target", "uid-1", "development", now_ms)
                .expect("first prepare");
        let retried_secret =
            prepare_bootstrap_request(&store_path, "desktop-target", "uid-1", "development", now_ms)
                .expect("retried prepare reuses the pending record");
        assert_eq!(
            first_secret, retried_secret,
            "a retry before acknowledgement must resend the same candidate"
        );

        let target_attested_expiry = now_ms + 12 * 60 * 60 * 1000;
        let grant = confirm_bootstrap_response(
            &store_path,
            "desktop-target",
            &first_secret,
            LanBootstrapResponse {
                ca_certificate_pem: "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----"
                    .to_string(),
                expires_at_unix_ms: target_attested_expiry,
            },
        )
        .expect("confirm");

        assert_eq!(grant.bearer_secret, first_secret);
        assert_eq!(
            grant.trust_anchor_pem.as_deref(),
            Some("-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----")
        );
        assert_eq!(
            grant.expires_at_unix_ms, target_attested_expiry,
            "the target's own attested expiry must be used verbatim, not recomputed at receipt"
        );

        let store = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            .expect("reload store");
        assert!(store
            .outbound_grant_for("desktop-target", Some("uid-1"), now_ms)
            .is_some());
    }

    /// A stale/late acknowledgement carrying an old secret must never
    /// confirm a newer pending request for the same target - proven
    /// directly against the store, matching `confirm_outbound`'s own
    /// contract.
    #[test]
    fn a_stale_ack_with_the_wrong_secret_does_not_confirm_a_newer_pending_request() {
        let store_path = crate::test_paths::unique_test_path("lan-bootstrap-stale-ack-store");
        let now_ms = 1_000;

        prepare_bootstrap_request(&store_path, "desktop-target", "uid-1", "development", now_ms)
            .expect("prepare");

        let error = confirm_bootstrap_response(
            &store_path,
            "desktop-target",
            "an-old-secret-from-a-previous-request",
            LanBootstrapResponse {
                ca_certificate_pem: "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----"
                    .to_string(),
                expires_at_unix_ms: now_ms + 1000,
            },
        )
        .expect_err("a mismatched secret must refuse to confirm");
        assert!(error.contains("no matching pending bootstrap"), "{error}");

        // The genuinely pending request must be untouched by the rejected ack.
        let store = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            .expect("reload store");
        assert_eq!(store.pending.len(), 1);
        assert!(store.outbound.is_empty());
    }

    /// The extractor gate itself: a dispatch with no relay-attested source
    /// desktop identity at all (the local/loopback dispatch path) must never
    /// reach the handler's own logic.
    #[tokio::test(flavor = "current_thread")]
    async fn bootstrap_is_refused_without_a_relay_attested_source() {
        let target_state = test_state_with_seed("desktop-target", "Target Mac", |_db| {});

        let response = crate::http_api::dispatch_authenticated_http_invoke(
            Arc::clone(&target_state),
            "POST",
            "/v1/lan-routing/bootstrap",
            serde_json::json!({ "candidateSecret": "the-candidate-secret" }),
        )
        .await;

        assert_eq!(response.status, StatusCode::UNAUTHORIZED.as_u16());
    }

    /// A relay connection authenticated only by account (no source desktop
    /// id - a v1 relay, or a caller relay never attested at all) must also
    /// be refused: `AuthenticatedHttpInvoke` present is not sufficient on
    /// its own, only both fields together are.
    #[tokio::test(flavor = "current_thread")]
    async fn bootstrap_is_refused_with_an_account_but_no_source_desktop_id() {
        let target_state = test_state_with_seed("desktop-target", "Target Mac", |_db| {});

        let response = crate::http_api::dispatch_authenticated_relay_http_invoke(
            Arc::clone(&target_state),
            "uid-1".to_string(),
            None,
            "POST",
            "/v1/lan-routing/bootstrap",
            serde_json::json!({ "candidateSecret": "the-candidate-secret" }),
        )
        .await;

        assert_eq!(response.status, StatusCode::UNAUTHORIZED.as_u16());
    }
}
