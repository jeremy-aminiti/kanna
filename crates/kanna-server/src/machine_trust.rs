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

/// This module's own record-shape version, independent of any relay
/// `desktopRouting` capability version. Bumped only if a grant/pending
/// record's own persisted shape changes incompatibly. A record whose
/// `protocol_version` does not match the running build's is treated exactly
/// like an account/environment/local-identity mismatch: not found, not
/// verified - forcing a fresh bootstrap rather than trusting a record this
/// build may not fully understand. `#[serde(default)]` on the field itself
/// makes an already-persisted record from before this version existed
/// deserialize as `0`, which never matches and so safely invalidates it.
pub const MACHINE_TRUST_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboundGrant {
    pub source_desktop_id: String,
    pub secret_hash: String,
    pub account_uid: String,
    pub environment: String,
    /// This desktop's own `desktop_id` at the moment it accepted the grant.
    /// Checked at every use against the *current* `desktop_id`: a config
    /// directory reused after a changed desktop identity must not keep
    /// answering to inbound secrets bootstrapped for the old one.
    #[serde(default)]
    pub local_desktop_id: String,
    #[serde(default)]
    pub protocol_version: u32,
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
    /// This desktop's own `desktop_id` at the moment it minted the request
    /// this grant confirmed. See [`InboundGrant::local_desktop_id`].
    #[serde(default)]
    pub local_desktop_id: String,
    #[serde(default)]
    pub protocol_version: u32,
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
    /// This desktop's own `desktop_id` when the request was prepared.
    /// [`MachineTrustStore::confirm_outbound`] refuses an acknowledgement
    /// whose pending record no longer matches the *current* local identity -
    /// a desktop id changing mid-flight must force a fresh bootstrap rather
    /// than confirm a grant under a stale local identity.
    #[serde(default)]
    pub local_desktop_id: String,
    #[serde(default)]
    pub protocol_version: u32,
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
    /// The account uid this store's on-disk contents are known-consistent
    /// with, as of the last reconciliation that actually *persisted*
    /// successfully - see [`MachineTrustStore::retain_account`]'s own doc
    /// comment for why this, not merely re-running `retain_account`, is what
    /// makes a failed purge durably retry instead of silently reactivating
    /// on a later sign-in to the very same account.
    #[serde(default)]
    pub reconciled_account_uid: Option<String>,
}

impl MachineTrustStore {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read machine trust store {}: {e}", path.display()))?;
        serde_json::from_str(&content).map_err(|e| {
            format!(
                "failed to parse machine trust store {}: {e}",
                path.display()
            )
        })
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
        local_desktop_id: &str,
        candidate_secret: impl FnOnce() -> Result<String, String>,
        now_ms: u64,
    ) -> Result<PendingBootstrap, String> {
        if let Some(existing) = self.pending.iter().find(|pending| {
            pending.target_desktop_id == target_desktop_id
                && pending.account_uid == account_uid
                && pending.environment == environment
                && pending.local_desktop_id == local_desktop_id
        }) {
            return Ok(existing.clone());
        }
        let pending = PendingBootstrap {
            target_desktop_id: target_desktop_id.to_string(),
            candidate_secret: candidate_secret()?,
            account_uid: account_uid.to_string(),
            environment: environment.to_string(),
            local_desktop_id: local_desktop_id.to_string(),
            protocol_version: MACHINE_TRUST_PROTOCOL_VERSION,
            created_at_unix_ms: now_ms,
        };
        self.pending.push(pending.clone());
        Ok(pending)
    }

    /// Moves a pending bootstrap to a confirmed outbound grant once the
    /// target acknowledges it. Matches on the *exact* `candidate_secret`
    /// this specific request sent, not merely `target_desktop_id`: an
    /// account transition between preparing a request and its acknowledgement
    /// arriving could otherwise let a stale/late ack wrongly confirm an
    /// unrelated, newer pending request for the same target - the secret is
    /// this request's own nonce and a stale ack simply will not match the
    /// pending record a newer request created. Takes the target's own
    /// attested `expires_at_unix_ms` (returned by its `accept_inbound`
    /// call) rather than computing a fresh one from receipt time, so a late
    /// ack cannot silently extend the effective lease beyond what the
    /// target actually granted.
    pub fn confirm_outbound(
        &mut self,
        target_desktop_id: &str,
        candidate_secret: &str,
        current_local_desktop_id: &str,
        trust_anchor_pem: Option<String>,
        expires_at_unix_ms: u64,
    ) -> Result<OutboundGrant, String> {
        let index = self
            .pending
            .iter()
            .position(|pending| {
                pending.target_desktop_id == target_desktop_id
                    && pending.candidate_secret == candidate_secret
                    && pending.local_desktop_id == current_local_desktop_id
            })
            .ok_or_else(|| {
                format!(
                    "no matching pending bootstrap for target {target_desktop_id} - this \
                     acknowledgement does not match the current request for it"
                )
            })?;
        let pending = self.pending.remove(index);
        let grant = OutboundGrant {
            target_desktop_id: pending.target_desktop_id,
            bearer_secret: pending.candidate_secret,
            trust_anchor_pem,
            account_uid: pending.account_uid,
            environment: pending.environment,
            local_desktop_id: pending.local_desktop_id,
            protocol_version: MACHINE_TRUST_PROTOCOL_VERSION,
            issued_at_unix_ms: pending.created_at_unix_ms,
            expires_at_unix_ms,
        };
        self.outbound
            .retain(|existing| existing.target_desktop_id != grant.target_desktop_id);
        self.outbound.push(grant.clone());
        Ok(grant)
    }

    /// The unexpired outbound grant for a target under the *currently*
    /// authenticated account, or `None` if there is none, it has expired, or
    /// `current_account_uid` is `None` (signed out) or does not match the
    /// grant's own account. Checked fresh on every lookup rather than relied
    /// on solely via periodic reconciliation - a record `retain_account`
    /// has not yet pruned (a missed reconciliation pass, a save that failed,
    /// a restart before the first one ran) must still never be usable once
    /// the account it was minted under is no longer the current one.
    pub fn outbound_grant_for(
        &self,
        target_desktop_id: &str,
        current_account_uid: Option<&str>,
        current_environment: &str,
        current_local_desktop_id: &str,
        now_ms: u64,
    ) -> Option<&OutboundGrant> {
        let current_account_uid = current_account_uid?;
        self.outbound.iter().find(|grant| {
            grant.target_desktop_id == target_desktop_id
                && grant.account_uid == current_account_uid
                && grant.environment == current_environment
                && grant.local_desktop_id == current_local_desktop_id
                && grant.protocol_version == MACHINE_TRUST_PROTOCOL_VERSION
                && grant.expires_at_unix_ms > now_ms
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
        local_desktop_id: &str,
        now_ms: u64,
    ) -> InboundGrant {
        self.inbound
            .retain(|existing| existing.source_desktop_id != source_desktop_id);
        let grant = InboundGrant {
            source_desktop_id: source_desktop_id.to_string(),
            secret_hash: secret_hash.to_string(),
            account_uid: account_uid.to_string(),
            environment: environment.to_string(),
            local_desktop_id: local_desktop_id.to_string(),
            protocol_version: MACHINE_TRUST_PROTOCOL_VERSION,
            issued_at_unix_ms: now_ms,
            expires_at_unix_ms: now_ms.saturating_add(LEASE_MS),
        };
        self.inbound.push(grant.clone());
        grant
    }

    /// Verifies a caller-presented secret against an unexpired inbound grant
    /// bound to the *currently* authenticated account, in constant time.
    /// Reuses `pairing::hash_device_secret` so the two stores can never
    /// diverge on hash algorithm. `current_account_uid: None` (signed out)
    /// never verifies anything, and a grant whose own account no longer
    /// matches the current one is treated exactly like an absent grant -
    /// the same at-point-of-use enforcement as `outbound_grant_for`.
    pub fn verify_inbound(
        &self,
        source_desktop_id: &str,
        secret: &str,
        current_account_uid: Option<&str>,
        current_environment: &str,
        current_local_desktop_id: &str,
        now_ms: u64,
    ) -> bool {
        let Some(current_account_uid) = current_account_uid else {
            return false;
        };
        let Some(grant) = self.inbound.iter().find(|grant| {
            grant.source_desktop_id == source_desktop_id
                && grant.account_uid == current_account_uid
                && grant.environment == current_environment
                && grant.local_desktop_id == current_local_desktop_id
                && grant.protocol_version == MACHINE_TRUST_PROTOCOL_VERSION
                && grant.expires_at_unix_ms > now_ms
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
    /// previous one managed.
    ///
    /// Returns whether the caller must persist the result - which is not
    /// simply "did any record change." A transition *into* `current_account_uid`
    /// that finds nothing left to remove (every remaining record already
    /// belongs to it) must still be saved so `reconciled_account_uid`
    /// advances: if a *previous* purge for a *different* target uid failed to
    /// persist, its stale records can otherwise reappear as trusted the
    /// moment that same uid signs back in, purely because `retain_account`
    /// found nothing to filter that time. Persisting the marker atomically
    /// with the grants themselves is what makes a failed purge keep
    /// re-attempting on every later reconciliation - including one for the
    /// exact uid that failed to purge - until it actually succeeds, rather
    /// than being silently treated as done because nothing needed removing.
    pub fn retain_account(&mut self, current_account_uid: Option<&str>) -> bool {
        let transitioning = self.reconciled_account_uid.as_deref() != current_account_uid;
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
        let purged = before != (self.inbound.len(), self.outbound.len(), self.pending.len());
        if transitioning {
            self.reconciled_account_uid = current_account_uid.map(str::to_string);
        }
        purged || transitioning
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

    /// This crate's own desktop_id for every test below, standing in for
    /// `state.config().desktop_id` at every call site that binds to it.
    const LOCAL: &str = "local-desktop";
    const ENV: &str = "development";

    fn temp_store_path() -> std::path::PathBuf {
        crate::test_paths::unique_test_path("machine-trust-store")
    }

    #[test]
    fn persists_and_reloads_all_three_record_kinds() {
        let path = temp_store_path();
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", ENV, LOCAL, 1_000);
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
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

        let error = MachineTrustStore::load_fail_closed(&path)
            .expect_err("group-readable must fail closed");
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
                ENV,
                LOCAL,
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
                ENV,
                LOCAL,
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
    fn pending_or_create_mints_a_fresh_candidate_when_the_local_identity_changed() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                "old-local-desktop",
                || Ok("first-candidate".to_string()),
                1_000,
            )
            .expect("first pending");

        let second = store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                "new-local-desktop",
                || Ok("second-candidate".to_string()),
                2_000,
            )
            .expect("pending under the new local identity");

        assert_eq!(second.candidate_secret, "second-candidate");
        assert_eq!(
            store.pending.len(),
            2,
            "the old identity's pending record is orphaned, not silently reused"
        );
    }

    #[test]
    fn confirm_outbound_moves_pending_to_outbound_and_consumes_it() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");

        let confirmed = store
            .confirm_outbound(
                "desktop-b",
                "secret",
                LOCAL,
                Some("pem-1".to_string()),
                50_000,
            )
            .expect("first confirm");
        assert!(store.pending.is_empty());
        assert_eq!(store.outbound.len(), 1);
        assert_eq!(confirmed.bearer_secret, "secret");
        assert_eq!(
            confirmed.expires_at_unix_ms, 50_000,
            "the target's attested expiry must be used verbatim"
        );

        // The pending record is gone once confirmed, so a duplicated
        // acknowledgement fails rather than fabricating a second grant from
        // nothing; the caller treats this as "already confirmed" and leaves
        // the existing outbound grant alone instead of erroring the request.
        assert!(
            store
                .confirm_outbound("desktop-b", "secret", LOCAL, None, 2_000)
                .is_err(),
            "confirming with no matching pending record must fail rather than fabricate one"
        );
        assert_eq!(store.outbound.len(), 1);
    }

    #[test]
    fn confirm_outbound_refuses_a_secret_that_does_not_match_the_pending_request() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("real-secret".to_string()),
                1_000,
            )
            .expect("pending create");

        let error = store
            .confirm_outbound(
                "desktop-b",
                "stale-secret-from-another-request",
                LOCAL,
                None,
                2_000,
            )
            .expect_err("a mismatched secret must not confirm an unrelated pending request");
        assert!(error.contains("no matching pending bootstrap"), "{error}");
        assert_eq!(
            store.pending.len(),
            1,
            "the real pending request must survive"
        );
        assert!(store.outbound.is_empty());
    }

    #[test]
    fn confirm_outbound_refuses_an_ack_whose_pending_record_has_a_different_local_identity() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                "old-local-desktop",
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create under the old identity");

        // The local desktop identity changed between preparing the request
        // and its acknowledgement arriving - confirming under the new
        // identity must not silently carry the old pending record forward.
        let error = store
            .confirm_outbound("desktop-b", "secret", "new-local-desktop", None, 2_000)
            .expect_err("an ack must not confirm a pending record from a different local identity");
        assert!(error.contains("no matching pending bootstrap"), "{error}");
        assert_eq!(
            store.pending.len(),
            1,
            "the original pending record must survive untouched"
        );
    }

    #[test]
    fn confirming_replaces_a_stale_grant_for_the_same_target() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("first-secret".to_string()),
                1_000,
            )
            .expect("first pending");
        store
            .confirm_outbound("desktop-b", "first-secret", LOCAL, None, 1_000 + LEASE_MS)
            .expect("first confirm");

        // A fresh bootstrap for the same target (e.g. after the first grant
        // expired and was renewed) must replace, not accumulate alongside,
        // the old grant.
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("second-secret".to_string()),
                5_000,
            )
            .expect("second pending");
        let renewed = store
            .confirm_outbound("desktop-b", "second-secret", LOCAL, None, 5_000 + LEASE_MS)
            .expect("second confirm");

        assert_eq!(
            store.outbound.len(),
            1,
            "must not accumulate duplicate grants"
        );
        assert_eq!(renewed.bearer_secret, "second-secret");
    }

    #[test]
    fn outbound_grant_for_hides_an_expired_grant() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");
        store
            .confirm_outbound("desktop-b", "secret", LOCAL, None, 1_000 + LEASE_MS)
            .expect("confirm");

        assert!(store
            .outbound_grant_for("desktop-b", Some("uid-1"), ENV, LOCAL, 1_000)
            .is_some());
        let just_before_expiry = 1_000 + LEASE_MS - 1;
        assert!(store
            .outbound_grant_for("desktop-b", Some("uid-1"), ENV, LOCAL, just_before_expiry)
            .is_some());
        let after_expiry = 1_000 + LEASE_MS;
        assert!(
            store
                .outbound_grant_for("desktop-b", Some("uid-1"), ENV, LOCAL, after_expiry)
                .is_none(),
            "an expired grant must not be returned as usable"
        );
        assert!(
            store
                .outbound_grant_for("desktop-b", Some("uid-2"), ENV, LOCAL, 1_000)
                .is_none(),
            "a grant minted under one account must not be returned as usable under another"
        );
        assert!(
            store
                .outbound_grant_for("desktop-b", None, ENV, LOCAL, 1_000)
                .is_none(),
            "signed out (no current account) must never see an outbound grant"
        );
        assert!(
            store
                .outbound_grant_for("desktop-b", Some("uid-1"), "staging", LOCAL, 1_000)
                .is_none(),
            "a grant minted under one environment must not be usable under another"
        );
        assert!(
            store
                .outbound_grant_for(
                    "desktop-b",
                    Some("uid-1"),
                    ENV,
                    "a-different-local-desktop",
                    1_000
                )
                .is_none(),
            "a grant minted under one local desktop identity must not be usable under another"
        );
    }

    #[test]
    fn outbound_grant_for_hides_a_grant_from_an_unrecognized_protocol_version() {
        let mut store = MachineTrustStore::default();
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");
        store
            .confirm_outbound("desktop-b", "secret", LOCAL, None, 1_000 + LEASE_MS)
            .expect("confirm");
        // Simulate a record persisted under a build with a different (or
        // absent, per `#[serde(default)]`) protocol version.
        store.outbound[0].protocol_version = 0;

        assert!(
            store
                .outbound_grant_for("desktop-b", Some("uid-1"), ENV, LOCAL, 1_000)
                .is_none(),
            "a grant from an unrecognized protocol version must not be treated as usable"
        );
    }

    #[test]
    fn verify_inbound_checks_hash_and_expiry() {
        let mut store = MachineTrustStore::default();
        let hash = pairing::hash_device_secret("real-secret");
        store.accept_inbound("desktop-a", &hash, "uid-1", ENV, LOCAL, 1_000);

        assert!(store.verify_inbound("desktop-a", "real-secret", Some("uid-1"), ENV, LOCAL, 1_000));
        assert!(!store.verify_inbound(
            "desktop-a",
            "wrong-secret",
            Some("uid-1"),
            ENV,
            LOCAL,
            1_000
        ));
        assert!(!store.verify_inbound(
            "desktop-unknown",
            "real-secret",
            Some("uid-1"),
            ENV,
            LOCAL,
            1_000
        ));
        assert!(
            !store.verify_inbound(
                "desktop-a",
                "real-secret",
                Some("uid-1"),
                ENV,
                LOCAL,
                1_000 + LEASE_MS + 1
            ),
            "an expired inbound grant must stop verifying"
        );
        assert!(
            !store.verify_inbound("desktop-a", "real-secret", Some("uid-2"), ENV, LOCAL, 1_000),
            "a grant minted under one account must not verify under another"
        );
        assert!(
            !store.verify_inbound("desktop-a", "real-secret", None, ENV, LOCAL, 1_000),
            "signed out (no current account) must never verify anything"
        );
        assert!(
            !store.verify_inbound(
                "desktop-a",
                "real-secret",
                Some("uid-1"),
                "staging",
                LOCAL,
                1_000
            ),
            "a grant accepted under one environment must not verify under another"
        );
        assert!(
            !store.verify_inbound(
                "desktop-a",
                "real-secret",
                Some("uid-1"),
                ENV,
                "a-different-local-desktop",
                1_000
            ),
            "a grant accepted under one local desktop identity must not verify under another - \
             a config directory reused after a changed desktop id must not keep answering"
        );
    }

    #[test]
    fn retain_account_drops_every_record_from_another_account_including_pending() {
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", ENV, LOCAL, 1_000);
        store.accept_inbound("desktop-c", "hash-c", "uid-2", ENV, LOCAL, 1_000);
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("secret-1".to_string()),
                1_000,
            )
            .expect("pending for uid-1");
        store
            .pending_or_create(
                "desktop-d",
                "uid-2",
                ENV,
                LOCAL,
                || Ok("secret-2".to_string()),
                1_000,
            )
            .expect("pending for uid-2");
        store
            .confirm_outbound("desktop-b", "secret-1", LOCAL, None, 1_000 + LEASE_MS)
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
        assert_eq!(store.reconciled_account_uid.as_deref(), Some("uid-1"));
    }

    #[test]
    fn retain_account_none_clears_everything_on_sign_out() {
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", ENV, LOCAL, 1_000);
        store
            .pending_or_create(
                "desktop-b",
                "uid-1",
                ENV,
                LOCAL,
                || Ok("secret".to_string()),
                1_000,
            )
            .expect("pending create");

        let changed = store.retain_account(None);

        assert!(changed);
        assert!(store.inbound.is_empty());
        assert!(store.outbound.is_empty());
        assert!(store.pending.is_empty());
        assert_eq!(store.reconciled_account_uid, None);
    }

    #[test]
    fn retain_account_reports_a_change_when_transitioning_even_with_nothing_to_purge() {
        // A store that already holds only uid-1's records (e.g. freshly
        // reconciled once before) still must be saved when told to
        // reconcile to uid-1 again for the *first time this instance knows
        // of* - `reconciled_account_uid` starts `None`, so this is a real
        // transition even though nothing needs filtering.
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", ENV, LOCAL, 1_000);

        let changed = store.retain_account(Some("uid-1"));

        assert!(
            changed,
            "the marker must advance (and so be saved) even when nothing was purged"
        );
        assert_eq!(store.reconciled_account_uid.as_deref(), Some("uid-1"));

        // A second reconciliation to the *same* uid, with the marker already
        // caught up, has genuinely nothing new to report.
        assert!(!store.retain_account(Some("uid-1")));
    }

    #[test]
    fn a_purge_that_never_persisted_is_retried_by_a_later_reconciliation() {
        // Simulates relay.rs's reconcile_machine_trust_for_account: it
        // mutates a freshly-loaded store in memory, then calls `save`, and
        // on a save error only logs a warning and returns - the mutation is
        // discarded along with the function's local `store` value, and the
        // file on disk is untouched.
        let path = temp_store_path();
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", ENV, LOCAL, 1_000);
        assert!(store.retain_account(Some("uid-1")));
        store
            .save(&path)
            .expect("initial save records reconciled_account_uid = uid-1");

        // A sign-out purge (retain_account(None)) succeeds only in memory;
        // its save fails, so nothing persists.
        let mut failed_purge = MachineTrustStore::load(&path).expect("reload");
        assert!(
            failed_purge.retain_account(None),
            "the purge itself succeeds in memory"
        );
        drop(failed_purge); // the save that would persist this never happens

        // A later reconciliation - triggered by anything: a retry, a
        // restart's own startup reconciliation, or this exact account
        // signing back in - reloads from disk and must see this as still
        // needing to purge and persist, because the disk state never
        // actually recorded the sign-out in the first place.
        let mut retried = MachineTrustStore::load(&path).expect("reload after the failed persist");
        assert_eq!(
            retried.inbound.len(),
            1,
            "the failed purge never reached disk"
        );
        assert_eq!(retried.reconciled_account_uid.as_deref(), Some("uid-1"));
        assert!(
            retried.retain_account(None),
            "a purge that never persisted must be retried, not treated as already done"
        );
        assert!(retried.inbound.is_empty());
    }

    #[test]
    fn remove_expired_prunes_only_what_has_actually_expired() {
        let mut store = MachineTrustStore::default();
        store.accept_inbound("desktop-a", "hash-a", "uid-1", ENV, LOCAL, 1_000);
        store.accept_inbound("desktop-b", "hash-b", "uid-1", ENV, LOCAL, 1_000);

        // Manually age one record past its lease without waiting real time.
        store.inbound[0].expires_at_unix_ms = 1_500;

        let changed = store.remove_expired(2_000);

        assert!(changed);
        assert_eq!(store.inbound.len(), 1);
        assert_eq!(store.inbound[0].source_desktop_id, "desktop-b");
    }
}
