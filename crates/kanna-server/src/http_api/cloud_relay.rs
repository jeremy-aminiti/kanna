use super::lan_trust::PrivilegedTaskAccess;
use super::state::AppState;
use crate::db::Db;
use axum::extract::State;
use axum::http::StatusCode;
use std::sync::Arc;

pub(super) async fn reconnect_cloud_relay(
    _access: PrivilegedTaskAccess,
    State(state): State<Arc<AppState>>,
) -> StatusCode {
    state.request_cloud_relay_reconnect();
    StatusCode::NO_CONTENT
}

/// Set when an explicit sign-out's machine-trust cleanup could not be
/// durably persisted, and cleared once it has been - see
/// `sign_out_desktop_cloud_account` and `retry_pending_account_sign_out`.
/// The value is the observed `account_state_generation` at the moment the
/// pending sign-out was recorded; it is read back only for logging, since a
/// process restart resets the generation counter and the retry always
/// re-observes the current one fresh.
pub(crate) const PENDING_ACCOUNT_SIGN_OUT_SETTING: &str = "pending_account_sign_out_cleanup_v1";

/// The actual desktop-to-server explicit sign-out producer. Unlike
/// `reconnect_cloud_relay` (a generic "please reconnect" nudge that the
/// relay loop resolves later, using whatever credential is still
/// configured), this disables this desktop's in-memory LAN authority
/// immediately and atomically clears its automatic same-account trust -
/// before requesting the reconnect, and independent of whether the caller's
/// own cloud credential revoke succeeded or the relay round-trip ever
/// completes. See `relay::reconcile_machine_trust_for_account` for the
/// generation-guarded persistence and how it survives racing a concurrent
/// re-authentication.
///
/// A cleanup persistence failure is reported to the caller (rather than only
/// logged) and left recorded under [`PENDING_ACCOUNT_SIGN_OUT_SETTING`], so a
/// desktop that restarts before the cleanup ever durably lands still retries
/// it before doing anything else with account state - see
/// `retry_pending_account_sign_out`.
pub(super) async fn sign_out_desktop_cloud_account(
    _access: PrivilegedTaskAccess,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let generation = state.set_authenticated_account_uid(None);
    let db = Db::open(&state.config.db_path).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db error: {error}"),
        )
    })?;
    db.set_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING, &generation.to_string())
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error: {error}"),
            )
        })?;
    // Tear down and re-probe any live relay connection *after* local
    // authority is already cleared and the retry marker is already durable -
    // so even a reconnect that lands using the not-yet-revoked credential
    // races the generation guard below rather than a bare in-memory flag.
    state.request_cloud_relay_reconnect();
    match crate::relay::reconcile_machine_trust_for_account(&state, None, generation) {
        Ok(()) => {
            if let Err(error) = db.delete_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING) {
                log::warn!("Failed to clear pending sign-out marker after cleanup: {error}");
            }
            Ok(StatusCode::NO_CONTENT)
        }
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "signed out locally, but LAN trust cleanup could not be persisted yet and will \
                 retry at next startup: {error}"
            ),
        )),
    }
}

/// Runs once at relay-loop startup, before the reconnection loop's first
/// iteration: if a prior explicit sign-out's cleanup never durably landed
/// (the process crashed or was killed before it could), retry it now, using
/// the current (freshly-initialized) generation rather than the one
/// recorded before restart - nothing else can have raced it yet at this
/// point. `signed_out_or_rejected`'s own reconciliation only fires once
/// relay definitively confirms this desktop is unauthenticated, so without
/// this a lost cleanup could otherwise sit unresolved for as long as relay
/// stays unreachable or the same still-valid device credential keeps
/// reconnecting.
pub(crate) async fn retry_pending_account_sign_out(http_state: &Arc<AppState>, db: &Db) {
    let has_pending = match db.get_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING) {
        Ok(value) => value.is_some(),
        Err(error) => {
            log::warn!("Failed to read pending sign-out marker at startup: {error}");
            return;
        }
    };
    if !has_pending {
        return;
    }
    let generation = http_state.account_state_generation();
    match crate::relay::reconcile_machine_trust_for_account(http_state, None, generation) {
        Ok(()) => {
            if let Err(error) = db.delete_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING) {
                log::warn!(
                    "Failed to clear pending sign-out marker after startup retry: {error}"
                );
            } else {
                log::info!(
                    "Retried a machine-trust cleanup left pending by an explicit sign-out \
                     before the previous shutdown."
                );
            }
        }
        Err(error) => {
            log::warn!("Startup retry of pending sign-out cleanup failed again: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_api::test_support::test_state_with_seed;

    fn seed_trust_for_uid(state: &Arc<AppState>, uid: &str) {
        let now_ms = crate::machine_trust::unix_time_ms().expect("clock");
        let store_path = state
            .config()
            .machine_trust_store_path()
            .expect("machine trust store path");
        let mut store = crate::machine_trust::MachineTrustStore::default();
        store.accept_inbound(
            "desktop-peer",
            "hash-peer",
            uid,
            "development",
            &state.config().desktop_id,
            now_ms,
        );
        store.save(&store_path).expect("seed machine trust store");
    }

    /// The explicit sign-out producer itself: in-memory authority clears
    /// immediately, the automatic trust store is atomically wiped, and the
    /// retry marker is left clean because this call's own cleanup succeeded.
    #[tokio::test]
    async fn sign_out_desktop_cloud_account_clears_authority_trust_and_marker() {
        let state = test_state_with_seed("desktop-signout", "Sign-out Mac", |_db| {});
        state.set_authenticated_account_uid(Some("uid-1".to_string()));
        seed_trust_for_uid(&state, "uid-1");

        let result = sign_out_desktop_cloud_account(PrivilegedTaskAccess, State(Arc::clone(&state)))
            .await
            .expect("sign-out succeeds");
        assert_eq!(result, StatusCode::NO_CONTENT);

        assert_eq!(state.authenticated_account_uid(), None);
        let store_path = state
            .config()
            .machine_trust_store_path()
            .expect("machine trust store path");
        let reloaded = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            .expect("reload after sign-out");
        assert!(
            reloaded.inbound.is_empty(),
            "sign-out must clear every automatic trust record"
        );

        let db = Db::open(&state.config.db_path).expect("open db");
        assert_eq!(
            db.get_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING)
                .expect("read marker"),
            None,
            "a fully persisted cleanup must not leave a retry marker behind"
        );
    }

    /// A cleanup left pending by a prior crash (the marker is set, but
    /// nothing since has changed the account state) must be retried and
    /// resolved at the next startup, without waiting for relay to
    /// independently observe this desktop as unauthenticated.
    #[tokio::test]
    async fn retry_pending_account_sign_out_finishes_a_cleanup_left_by_a_prior_crash() {
        let state = test_state_with_seed("desktop-retry-signout", "Retry Mac", |_db| {});
        seed_trust_for_uid(&state, "uid-1");
        let db = Db::open(&state.config.db_path).expect("open db");
        db.set_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING, "0")
            .expect("seed pending marker");

        retry_pending_account_sign_out(&state, &db).await;

        let store_path = state
            .config()
            .machine_trust_store_path()
            .expect("machine trust store path");
        let reloaded = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            .expect("reload after retry");
        assert!(
            reloaded.inbound.is_empty(),
            "the retried cleanup must still clear every automatic trust record"
        );
        assert_eq!(
            db.get_setting(PENDING_ACCOUNT_SIGN_OUT_SETTING)
                .expect("read marker"),
            None,
            "a successful retry must clear its own marker"
        );
    }

    /// Nothing was pending - the common case at every ordinary startup - so
    /// this must not touch the trust store at all.
    #[tokio::test]
    async fn retry_pending_account_sign_out_is_a_no_op_when_nothing_is_pending() {
        let state = test_state_with_seed("desktop-retry-noop", "Retry Noop Mac", |_db| {});
        seed_trust_for_uid(&state, "uid-1");
        let db = Db::open(&state.config.db_path).expect("open db");

        retry_pending_account_sign_out(&state, &db).await;

        let store_path = state
            .config()
            .machine_trust_store_path()
            .expect("machine trust store path");
        let reloaded = crate::machine_trust::MachineTrustStore::load_fail_closed(&store_path)
            .expect("reload after retry");
        assert_eq!(
            reloaded.inbound.len(),
            1,
            "with no pending marker, existing trust must be left untouched"
        );
    }
}
