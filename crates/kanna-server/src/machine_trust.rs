//! Server-owned automatic same-account LAN trust.
//!
//! Distinct from `pairing::PairingStore`, which is the human/mobile QR
//! pairing ceremony and only ever holds an *inbound* secret hash. This store
//! holds both directions of automatic desktop-to-desktop trust:
//! - `inbound`: hashes of secrets other same-account desktops present to
//!   *this* desktop's machine-invoke gateway (this desktop is the target).
//! - `outbound`: plaintext bearer secrets *this* desktop presents when
//!   calling another same-account desktop (this desktop is the source),
//!   plus that target's TLS trust anchor once one exists.
//! - `pending`: a durably-recorded, not-yet-acknowledged outbound bootstrap
//!   in flight, keyed by target so a lost acknowledgement and a retry
//!   resend the same candidate secret instead of minting a second one.
//!
//! Every record is bound to the account UID and environment it was minted
//! under, on a lease no longer than [`LEASE_MS`], so it fails closed on
//! expiry, on a restart that finds a corrupted or over-permissive file, and
//! on an account change - with no unbounded credential and no dependency on
//! an edge-triggered sign-out callback succeeding. `kanna-server` is the
//! lifecycle owner of this store; nothing outside this crate reads or writes
//! it directly.

use crate::pairing;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Guards every load-modify-save cycle against this store's own file, so two
/// concurrent bootstrap requests (or a bootstrap racing an account-change
/// reconciliation) cannot interleave and drop one writer's update. A
/// process-wide static rather than a new `AppState` field: `AppState` has no
/// builder, and a new required field would touch every literal `AppState`
/// construction site across this crate's tests for something that is purely
/// an implementation detail of this module's own persistence.
pub(crate) fn persistence_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Maximum lifetime of an automatic trust record. Renewed only by another
/// authenticated relay bootstrap; there is deliberately no background
/// renewal timer.
pub const LEASE_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboundGrant {
    pub source_desktop_id: String,
    pub secret_hash: String,
    pub account_uid: String,
    pub environment: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OutboundGrant {
    pub target_desktop_id: String,
    /// Plaintext bearer this desktop presents to the target. Never derived
    /// from, or written into, `pairing::PairingStore`, which stores hashes
    /// only.
    pub bearer_secret: String,
    /// PEM-encoded trust anchor the target's relay bootstrap ack attested
    /// for it. `None` until the TLS transport exists; an outbound grant with
    /// no trust anchor is not yet usable for a LAN attempt and callers must
    /// treat it the same as having no grant at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_anchor_pem: Option<String>,
    pub account_uid: String,
    pub environment: String,
    pub issued_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

/// A durably-recorded outbound bootstrap awaiting its relay acknowledgement.
/// Recording the candidate secret *before* the relay round trip, and
/// resending the same one on retry, is what makes a lost acknowledgement
/// converge to exactly one usable credential instead of leaking an orphaned
/// one on the target for every retry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingBootstrap {
    pub target_desktop_id: String,
    pub candidate_secret: String,
    pub account_uid: String,
    pub environment: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MachineTrustStore {
    #[serde(default)]
    pub inbound: Vec<InboundGrant>,
    #[serde(default)]
    pub outbound: Vec<OutboundGrant>,
    #[serde(default)]
    pub pending: Vec<PendingBootstrap>,
}

impl MachineTrustStore {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read machine trust store {}: {e}", path.display()))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("failed to parse machine trust store {}: {e}", path.display()))
    }

    /// Loads, failing closed on a store file that grants readability beyond
    /// its owner. The file can only have become that permissive outside this
    /// module's own [`save`](Self::save), which never writes it that way, so
    /// treating it as untrustworthy rather than repairing it in place is the
    /// safe default for a file that can hold live plaintext bearer secrets.
    pub fn load_fail_closed(path: &Path) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt;

        if !path.exists() {
            return Ok(Self::default());
        }
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("failed to stat machine trust store {}: {e}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "machine trust store {} is not a regular file",
                path.display()
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "machine trust store {} must not grant group or other permissions",
                path.display()
            ));
        }
        Self::load(path)
    }

    /// Atomically replaces the store on disk, reasserting owner-only
    /// permissions and refusing to write through a pre-existing temp path
    /// (a stale leftover or a symlink) on every write - see
    /// `secure_file::atomic_write_0600`. Unlike `pairing::PairingStore`
    /// (hashes only, no explicit permission enforcement today), this file
    /// can hold live plaintext outbound bearer secrets, so it must never
    /// inherit a permissive umask, and it must never write through a path
    /// this process did not itself just create.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize machine trust store: {e}"))?;
        crate::secure_file::atomic_write_0600(path, &body)
    }

    /// The candidate secret to bootstrap `target_desktop_id` with. Reuses an
    /// existing pending record for the same target/account/environment so a
    /// retry after a lost acknowledgement resends the same candidate instead
    /// of minting a second one; the target's own [`accept_inbound`] upsert is
    /// idempotent on that same value.
    pub fn pending_or_create(
        &mut self,
        target_desktop_id: &str,
        account_uid: &str,
        environment: &str,
        candidate_secret: impl FnOnce() -> Result<String, String>,
        now_ms: u64,
    ) -> Result<PendingBootstrap, String> {
        if let Some(existing) = self.pending.iter().find(|pending| {
            pending.target_desktop_id == target_desktop_id
                && pending.account_uid == account_uid
                && pending.environment == environment
        }) {
            return Ok(existing.clone());
        }
        let pending = PendingBootstrap {
            target_desktop_id: target_desktop_id.to_string(),
            candidate_secret: candidate_secret()?,
            account_uid: account_uid.to_string(),
            environment: environment.to_string(),
            created_at_unix_ms: now_ms,
        };
        self.pending.push(pending.clone());
        Ok(pending)
    }

    /// Moves a pending bootstrap to a confirmed outbound grant once the
    /// relay attests the addressed target accepted it. Idempotent:
    /// acknowledging the same pending target twice just re-confirms the same
    /// grant rather than erroring or duplicating it.
    pub fn confirm_outbound(
        &mut self,
        target_desktop_id: &str,
        trust_anchor_pem: Option<String>,
        now_ms: u64,
    ) -> Result<OutboundGrant, String> {
        let index = self
            .pending
            .iter()
            .position(|pending| pending.target_desktop_id == target_desktop_id)
            .ok_or_else(|| format!("no pending bootstrap for target {target_desktop_id}"))?;
        let pending = self.pending.remove(index);
        let grant = OutboundGrant {
            target_desktop_id: pending.target_desktop_id,
            bearer_secret: pending.candidate_secret,
            trust_anchor_pem,
            account_uid: pending.account_uid,
            environment: pending.environment,
            issued_at_unix_ms: pending.created_at_unix_ms,
            expires_at_unix_ms: now_ms.saturating_add(LEASE_MS),
        };
        self.outbound
            .retain(|existing| existing.target_desktop_id != grant.target_desktop_id);
        self.outbound.push(grant.clone());
        Ok(grant)
    }

    /// The unexpired outbound grant for a target, or `None` if there is none
    /// or it has expired. Expiry is checked on every lookup rather than
    /// pruned on a timer, so a store nobody has written to in days still
    /// fails closed correctly.
    pub fn outbound_grant_for(
        &self,
        target_desktop_id: &str,
        now_ms: u64,
    ) -> Option<&OutboundGrant> {
        self.outbound.iter().find(|grant| {
            grant.target_desktop_id == target_desktop_id && grant.expires_at_unix_ms > now_ms
        })
    }

    /// Idempotently upserts an inbound grant for a caller that authenticated
    /// itself over the relay bootstrap. Distinct from
    /// `pairing::PairingStore::add_trusted_device`: this never touches the
    /// human/mobile pairing session or push-identity material, and every
    /// record carries an expiry the mobile/manual model does not.
    pub fn accept_inbound(
        &mut self,
        source_desktop_id: &str,
        secret_hash: &str,
        account_uid: &str,
        environment: &str,
        now_ms: u64,
    ) {
        self.inbound
            .retain(|existing| existing.source_desktop_id != source_desktop_id);
        self.inbound.push(InboundGrant {
            source_desktop_id: source_desktop_id.to_string(),
            secret_hash: secret_hash.to_string(),
            account_uid: account_uid.to_string(),
            environment: environment.to_string(),
            issued_at_unix_ms: now_ms,
            expires_at_unix_ms: now_ms.saturating_add(LEASE_MS),
        });
    }

    /// Verifies a caller-presented secret against an unexpired inbound
    /// grant, in constant time. Reuses `pairing::hash_device_secret` so the
    /// two stores can never diverge on hash algorithm.
    pub fn verify_inbound(&self, source_desktop_id: &str, secret: &str, now_ms: u64) -> bool {
        let Some(grant) = self.inbound.iter().find(|grant| {
            grant.source_desktop_id == source_desktop_id && grant.expires_at_unix_ms > now_ms
        }) else {
            return false;
        };
        pairing::constant_time_eq(
            grant.secret_hash.as_bytes(),
            pairing::hash_device_secret(secret).as_bytes(),
        )
    }

    /// Removes every record - inbound, outbound, and pending - not bound to
    /// `current_account_uid`. Passing `None` (signed out) clears everything.
    ///
    /// This is the durable, server-owned account-transition cleanup: it is
    /// meant to run on every reconciliation this module is asked to do,
    /// including one right after restart, so it is not defeated by a
    /// crashed frontend, a failed one-shot purge, or a missed sign-out
    /// event - the next reconciliation converges regardless of what the
    /// previous one managed. Returns whether anything changed, so a caller
    /// only pays for a `save` when needed.
    pub fn retain_account(&mut self, current_account_uid: Option<&str>) -> bool {
        let before = (self.inbound.len(), self.outbound.len(), self.pending.len());
        match current_account_uid {
            Some(uid) => {
                self.inbound.retain(|grant| grant.account_uid == uid);
                self.outbound.retain(|grant| grant.account_uid == uid);
                self.pending.retain(|pending| pending.account_uid == uid);
            }
            None => {
                self.inbound.clear();
                self.outbound.clear();
                self.pending.clear();
            }
        }
        before != (self.inbound.len(), self.outbound.len(), self.pending.len())
    }

    /// Drops every expired inbound and outbound record. Expiry is already
    /// enforced on every lookup ([`outbound_grant_for`](Self::outbound_grant_for),
    /// [`verify_inbound`](Self::verify_inbound)); this only reclaims space
    /// and is safe to skip entirely.
    pub fn remove_expired(&mut self, now_ms: u64) -> bool {
        let before = (self.inbound.len(), self.outbound.len());
        self.inbound
            .retain(|grant| grant.expires_at_unix_ms > now_ms);
        self.outbound
            .retain(|grant| grant.expires_at_unix_ms > now_ms);
        before != (self.inbound.len(), self.outbound.len())
    }
}

pub fn unix_time_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock error: {e}"))
        .map(|duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store_path() -> std::path::PathBuf {
        crate::test_paths::unique_test_path("machine-trust-store")
    }

    #[test]
    fn persists_and_reloads_all_three_record_kinds() {
        let path = temp_store_path();
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", "development", 1_000);
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("candidate-secret".to_string()),
                1_000,
            )
            .expect("pending create");
        store.save(&path).expect("save");

        let reloaded = MachineTrustStore::load(&path).expect("load");
        assert_eq!(reloaded.inbound.len(), 1);
        assert_eq!(reloaded.pending.len(), 1);
        assert!(reloaded.outbound.is_empty());
        assert_eq!(reloaded.inbound[0].source_desktop_id, "desktop-a");
        assert_eq!(reloaded.pending[0].candidate_secret, "candidate-secret");
    }

    #[test]
    fn missing_store_loads_empty_rather_than_erroring() {
        let path = temp_store_path();
        let store = MachineTrustStore::load(&path).expect("missing file loads empty");
        assert!(store.inbound.is_empty());
        assert!(store.outbound.is_empty());
        assert!(store.pending.is_empty());
    }

    #[test]
    fn save_reasserts_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path();
        MachineTrustStore::default().save(&path).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "machine trust store must be owner-read-write only"
        );
    }

    #[test]
    fn load_fail_closed_refuses_a_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path();
        MachineTrustStore::default().save(&path).expect("save");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("widen permissions to simulate tampering");

        let error =
            MachineTrustStore::load_fail_closed(&path).expect_err("group-readable must fail closed");
        assert!(error.contains("group or other permissions"), "{error}");
    }

    #[test]
    fn pending_or_create_is_idempotent_so_a_retry_resends_the_same_candidate() {
        let mut store = MachineTrustStore::default();
        let mut calls = 0;
        let first = store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || {
                    calls += 1;
                    Ok("first-candidate".to_string())
                },
                1_000,
            )
            .expect("first pending");
        let second = store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || {
                    calls += 1;
                    Ok("second-candidate".to_string())
                },
                2_000,
            )
            .expect("retry pending");

        assert_eq!(calls, 1, "a retry must not mint a second candidate");
        assert_eq!(first.candidate_secret, second.candidate_secret);
        assert_eq!(store.pending.len(), 1);
    }

    #[test]
    fn confirm_outbound_moves_pending_to_outbound_and_consumes_it() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");

        let confirmed = store
            .confirm_outbound("desktop-b", Some("pem-1".to_string()), 1_500)
            .expect("first confirm");
        assert!(store.pending.is_empty());
        assert_eq!(store.outbound.len(), 1);
        assert_eq!(confirmed.bearer_secret, "secret");
        assert_eq!(confirmed.expires_at_unix_ms, 1_500 + LEASE_MS);

        // The pending record is gone once confirmed, so a duplicated
        // acknowledgement fails rather than fabricating a second grant from
        // nothing; the caller treats this as "already confirmed" and leaves
        // the existing outbound grant alone instead of erroring the request.
        assert!(
            store.confirm_outbound("desktop-b", None, 2_000).is_err(),
            "confirming with no matching pending record must fail rather than fabricate one"
        );
        assert_eq!(store.outbound.len(), 1);
    }

    #[test]
    fn confirming_replaces_a_stale_grant_for_the_same_target() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("first-secret".to_string()),
                1_000,
            )
            .expect("first pending");
        store
            .confirm_outbound("desktop-b", None, 1_000)
            .expect("first confirm");

        // A fresh bootstrap for the same target (e.g. after the first grant
        // expired and was renewed) must replace, not accumulate alongside,
        // the old grant.
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("second-secret".to_string()),
                5_000,
            )
            .expect("second pending");
        let renewed = store
            .confirm_outbound("desktop-b", None, 5_000)
            .expect("second confirm");

        assert_eq!(store.outbound.len(), 1, "must not accumulate duplicate grants");
        assert_eq!(renewed.bearer_secret, "second-secret");
    }

    #[test]
    fn outbound_grant_for_hides_an_expired_grant() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");
        store
            .confirm_outbound("desktop-b", None, 1_000)
            .expect("confirm");

        assert!(store.outbound_grant_for("desktop-b", 1_000).is_some());
        let just_before_expiry = 1_000 + LEASE_MS - 1;
        assert!(store
            .outbound_grant_for("desktop-b", just_before_expiry)
            .is_some());
        let after_expiry = 1_000 + LEASE_MS;
        assert!(
            store
                .outbound_grant_for("desktop-b", after_expiry)
                .is_none(),
            "an expired grant must not be returned as usable"
        );
    }

    #[test]
    fn verify_inbound_checks_hash_and_expiry() {
        let mut store = MachineTrustStore::default();
        let hash = pairing::hash_device_secret("real-secret");
        store.accept_inbound("desktop-a", &hash, "uid-1", "development", 1_000);

        assert!(store.verify_inbound("desktop-a", "real-secret", 1_000));
        assert!(!store.verify_inbound("desktop-a", "wrong-secret", 1_000));
        assert!(!store.verify_inbound("desktop-unknown", "real-secret", 1_000));
        assert!(
            !store.verify_inbound("desktop-a", "real-secret", 1_000 + LEASE_MS + 1),
            "an expired inbound grant must stop verifying"
        );
    }

    #[test]
    fn retain_account_drops_every_record_from_another_account_including_pending() {
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", "development", 1_000);
        store.accept_inbound("desktop-c", "hash-c", "uid-2", "development", 1_000);
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("secret-1".to_string()),
                1_000,
            )
            .expect("pending for uid-1");
        store
            .pending_or_create(
                "desktop-d",
                "uid-2",
                "development",
                || Ok("secret-2".to_string()),
                1_000,
            )
            .expect("pending for uid-2");
        store
            .confirm_outbound("desktop-b", None, 1_000)
            .expect("confirm uid-1 outbound");

        let changed = store.retain_account(Some("uid-1"));

        assert!(changed);
        assert_eq!(store.inbound.len(), 1);
        assert_eq!(store.inbound[0].account_uid, "uid-1");
        assert_eq!(store.outbound.len(), 1);
        assert_eq!(store.outbound[0].account_uid, "uid-1");
        // desktop-b's pending record was already consumed by confirm_outbound
        // above; desktop-d's belongs to uid-2 and must have been dropped by
        // retain_account even though it never had an outbound grant to
        // accompany it - pending records are trust in progress, not merely
        // metadata, so nothing uid-2-scoped should survive.
        assert!(store.pending.is_empty(), "{:?}", store.pending);
    }

    #[test]
    fn retain_account_none_clears_everything_on_sign_out() {
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", "development", 1_000);
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                "development",
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");

        let changed = store.retain_account(None);

        assert!(changed);
        assert!(store.inbound.is_empty());
        assert!(store.outbound.is_empty());
        assert!(store.pending.is_empty());
    }

    #[test]
    fn remove_expired_prunes_only_what_has_actually_expired() {
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", "development", 1_000);
        store.accept_inbound("desktop-b", "hash-b", "uid-1", "development", 1_000);

        // Manually age one record past its lease without waiting real time.
        store.inbound[0].expires_at_unix_ms = 1_500;

        let changed = store.remove_expired(2_000);

        assert!(changed);
        assert_eq!(store.inbound.len(), 1);
        assert_eq!(store.inbound[0].source_desktop_id, "desktop-b");
    }
}
