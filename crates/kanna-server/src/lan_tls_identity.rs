//! This desktop's own stable TLS identity for the authenticated LAN
//! machine-invoke listener.
//!
//! Distinct from every other identity in this crate: `pairing::PairingStore`
//! holds mobile/manual pairing secrets, `machine_trust::MachineTrustStore`
//! holds automatic same-account bearer grants, and this module holds neither
//! a secret nor a grant - it holds a private CA certificate and the leaf
//! server certificate it issues, per the accepted architecture: standard
//! rustls verification against a target-specific `RootCertStore`, never a
//! custom certificate verifier. A relay-authenticated bootstrap attests the
//! CA certificate to a same-account sibling as this desktop's trust anchor;
//! that sibling's outbound TLS client trusts only that one CA (never system
//! roots, never TOFU, never the Bonjour-discovered address itself) and does
//! ordinary WebPKI chain validation plus standard hostname verification
//! against the leaf's SAN. A single self-signed certificate reused as both
//! its own trust anchor and the presented leaf was deliberately rejected:
//! whether a length-0 "leaf is the anchor" chain validates under standard
//! WebPKI rules without a CA basic-constraint depends on library internals
//! this module has no business depending on. A CA cert with
//! `IsCa::Ca(BasicConstraints::Unconstrained)` signing a separate leaf with
//! `desktop_id` as its SAN is the textbook case every standard X.509
//! validator is built to handle - see `lan_tls`, which is the only module
//! that reads this identity to build rustls configs.
//!
//! The identity is generated once and persisted; it must not regenerate on
//! an ordinary restart - a previously-bootstrapped sibling's pinned trust
//! anchor would silently stop matching, and every outbound grant pointing at
//! it would need re-bootstrapping for no reason connected to any actual
//! compromise or rotation. It regenerates only when the persisted identity
//! is missing or fails to parse - the same fail-closed-on-corruption stance
//! `machine_trust::MachineTrustStore::load_fail_closed` takes, not a silent
//! repair.

use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
use std::path::Path;

/// This desktop's LAN TLS identity: the private CA certificate a sibling
/// pins as its trust anchor, and the leaf server certificate/key this
/// desktop's own listener presents on the handshake. All three are
/// PEM-encoded exactly as `rcgen`/`rustls-pemfile` already expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanTlsIdentity {
    /// Attested to a sibling via the relay bootstrap; never presented on the
    /// wire during an ordinary TLS handshake itself.
    pub ca_certificate_pem: String,
    /// Presented by this desktop's LAN listener as its leaf certificate.
    pub leaf_certificate_pem: String,
    pub leaf_private_key_pem: String,
}

/// Loads the persisted identity, generating and persisting a new one if none
/// exists yet, the persisted identity was minted for a different
/// `desktop_id`/`environment`, or (unlike those two, which are ordinary
/// recovery, not tampering) it fails outright: an unparseable file, a
/// symlink, or a file that grants more than owner access. `desktop_id`
/// becomes the leaf certificate's subject alternative name, so a client
/// pinning to the CA can also assert *which* desktop it expected to reach
/// via standard hostname verification against that SAN - the identity binds
/// a specific desktop_id, not just "some same-account sibling holding some
/// CA-issued cert"; `environment` keeps a staging and a production identity
/// minted under the same reused config directory from ever being reused for
/// each other.
///
/// A desktop_id/environment mismatch regenerates rather than fails closed:
/// it is the expected shape of a config directory reused for a fresh
/// identity, and every `machine_trust` grant that depended on the old
/// identity is separately bound to `local_desktop_id`/`environment` there,
/// so it already stops verifying the moment this identity changes -
/// re-bootstrapping is required, not silently skipped.
pub fn load_or_create(
    path: &Path,
    desktop_id: &str,
    environment: &str,
) -> Result<LanTlsIdentity, String> {
    match try_load(path, desktop_id, environment)? {
        LoadedIdentity::Reusable(identity) => return Ok(identity),
        LoadedIdentity::Missing | LoadedIdentity::IdentityChanged => {}
    }
    let identity = generate(desktop_id)?;
    save(path, desktop_id, environment, &identity)?;
    Ok(identity)
}

enum LoadedIdentity {
    Reusable(LanTlsIdentity),
    Missing,
    IdentityChanged,
}

/// `Missing` for a missing file (the ordinary first-run case) or one minted
/// for a different desktop_id/environment (ordinary recovery, not
/// tampering); `Err` for a file that exists but fails to parse, is a
/// symlink, or grants more than owner access - fail closed rather than
/// silently discarding and regenerating over what might be an operator's own
/// copy, a partially-written file from a crashed prior run, or a private key
/// left readable by another account on the machine.
fn try_load(path: &Path, desktop_id: &str, environment: &str) -> Result<LoadedIdentity, String> {
    use std::os::unix::fs::PermissionsExt;

    if !path.exists() {
        return Ok(LoadedIdentity::Missing);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to stat LAN TLS identity {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "LAN TLS identity {} is not a regular file",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "LAN TLS identity {} must not grant group or other permissions",
            path.display()
        ));
    }
    let content = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "failed to read LAN TLS identity {}: {error}",
            path.display()
        )
    })?;
    let identity: PersistedIdentity = serde_json::from_str(&content).map_err(|error| {
        format!(
            "failed to parse LAN TLS identity {}: {error}",
            path.display()
        )
    })?;
    if identity.desktop_id != desktop_id || identity.environment != environment {
        return Ok(LoadedIdentity::IdentityChanged);
    }
    Ok(LoadedIdentity::Reusable(LanTlsIdentity {
        ca_certificate_pem: identity.ca_certificate_pem,
        leaf_certificate_pem: identity.leaf_certificate_pem,
        leaf_private_key_pem: identity.leaf_private_key_pem,
    }))
}

fn generate(desktop_id: &str) -> Result<LanTlsIdentity, String> {
    let (ca_certificate_pem, ca_cert, ca_key) = generate_ca()?;
    let (leaf_certificate_pem, leaf_private_key_pem) =
        generate_leaf(desktop_id, &ca_cert, &ca_key)?;
    Ok(LanTlsIdentity {
        ca_certificate_pem,
        leaf_certificate_pem,
        leaf_private_key_pem,
    })
}

fn generate_ca() -> Result<(String, rcgen::Certificate, KeyPair), String> {
    let mut params = CertificateParams::new(Vec::new())
        .map_err(|error| format!("failed to build LAN TLS CA params: {error}"))?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "Kanna LAN Machine-Invoke CA");
    let key_pair = KeyPair::generate()
        .map_err(|error| format!("failed to generate LAN TLS CA key: {error}"))?;
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|error| format!("failed to self-sign LAN TLS CA certificate: {error}"))?;
    let pem = certificate.pem();
    Ok((pem, certificate, key_pair))
}

fn generate_leaf(
    desktop_id: &str,
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
) -> Result<(String, String), String> {
    let mut params = CertificateParams::new(vec![sanitize_san(desktop_id)])
        .map_err(|error| format!("failed to build LAN TLS leaf params: {error}"))?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, desktop_id);
    params
        .extended_key_usages
        .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
    let key_pair = KeyPair::generate()
        .map_err(|error| format!("failed to generate LAN TLS leaf key: {error}"))?;
    let certificate = params
        .signed_by(&key_pair, ca_cert, ca_key)
        .map_err(|error| format!("failed to sign LAN TLS leaf certificate: {error}"))?;
    Ok((certificate.pem(), key_pair.serialize_pem()))
}

/// A certificate SAN must be a valid DNS name; a desktop_id is an opaque
/// application identifier that may not already be one. Reusing it byte for
/// byte when it happens to qualify keeps the common case readable; this
/// never affects which secret authenticates a request, only the label a
/// human reads off the certificate.
fn sanitize_san(desktop_id: &str) -> String {
    let is_valid_dns_label = !desktop_id.is_empty()
        && desktop_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !desktop_id.starts_with('-')
        && !desktop_id.ends_with('-');
    if is_valid_dns_label {
        desktop_id.to_string()
    } else {
        "kanna-lan-desktop".to_string()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedIdentity {
    /// Bound at generation time; checked against the *current* desktop_id
    /// and environment on every load - see `try_load`. `#[serde(default)]`
    /// makes a file persisted before this field existed deserialize as an
    /// empty string, which never matches any real desktop_id and so safely
    /// triggers a regeneration rather than a parse failure.
    #[serde(default)]
    desktop_id: String,
    #[serde(default)]
    environment: String,
    ca_certificate_pem: String,
    leaf_certificate_pem: String,
    leaf_private_key_pem: String,
}

/// Atomically persists the identity as 0600 and refuses to write through a
/// pre-existing temp path - see `secure_file::atomic_write_0600`. It
/// contains private keys, never group/other-readable, matching
/// `machine_trust::MachineTrustStore`'s persistence stance for the same
/// reason.
fn save(
    path: &Path,
    desktop_id: &str,
    environment: &str,
    identity: &LanTlsIdentity,
) -> Result<(), String> {
    let body = serde_json::to_string_pretty(&PersistedIdentity {
        desktop_id: desktop_id.to_string(),
        environment: environment.to_string(),
        ca_certificate_pem: identity.ca_certificate_pem.clone(),
        leaf_certificate_pem: identity.leaf_certificate_pem.clone(),
        leaf_private_key_pem: identity.leaf_private_key_pem.clone(),
    })
    .map_err(|error| format!("failed to serialize LAN TLS identity: {error}"))?;
    crate::secure_file::atomic_write_0600(path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_identity_path() -> std::path::PathBuf {
        crate::test_paths::unique_test_path("lan-tls-identity")
    }

    #[test]
    fn generates_a_parseable_pem_ca_and_leaf() {
        let identity = generate("desktop-1").expect("generate identity");
        assert!(identity
            .ca_certificate_pem
            .starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(identity
            .leaf_certificate_pem
            .starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(identity
            .leaf_private_key_pem
            .starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_ne!(
            identity.ca_certificate_pem, identity.leaf_certificate_pem,
            "the CA and the leaf it issues must be distinct certificates"
        );
    }

    #[test]
    fn load_or_create_persists_and_reloads_the_same_identity() {
        let path = temp_identity_path();
        let first = load_or_create(&path, "desktop-1", "development").expect("first load creates");
        let second = load_or_create(&path, "desktop-1", "development").expect("second load reuses");
        assert_eq!(
            first, second,
            "an ordinary restart must not regenerate the identity"
        );
    }

    #[test]
    fn save_reasserts_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_identity_path();
        load_or_create(&path, "desktop-1", "development").expect("create identity");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "LAN TLS identity file must be owner-read-write only"
        );
    }

    #[test]
    fn a_corrupt_identity_file_fails_closed_instead_of_silently_regenerating() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_identity_path();
        std::fs::write(&path, b"not valid json").expect("write corrupt file");
        // Owner-only, so this exercises the parse failure specifically, not
        // the separate (and separately tested) permission check.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("set owner-only permissions");
        let error = load_or_create(&path, "desktop-1", "development")
            .expect_err("a corrupt identity file must not be silently replaced");
        assert!(error.contains("failed to parse"), "{error}");
    }

    #[test]
    fn a_group_readable_identity_file_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_identity_path();
        load_or_create(&path, "desktop-1", "development").expect("create identity");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("widen permissions to simulate tampering");

        let error = load_or_create(&path, "desktop-1", "development")
            .expect_err("group-readable must fail closed, not silently regenerate");
        assert!(error.contains("group or other permissions"), "{error}");
    }

    #[test]
    fn a_symlinked_identity_path_fails_closed() {
        let target = temp_identity_path();
        load_or_create(&target, "desktop-1", "development").expect("create identity");
        let link = temp_identity_path();
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let error = load_or_create(&link, "desktop-1", "development")
            .expect_err("a symlink must fail closed, not be followed and reused or replaced");
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_changed_desktop_id_regenerates_rather_than_reusing_the_old_identity() {
        let path = temp_identity_path();
        let old = load_or_create(&path, "desktop-old", "development").expect("create old");

        let new = load_or_create(&path, "desktop-new", "development")
            .expect("a changed desktop id regenerates rather than failing closed");

        assert_ne!(
            old, new,
            "the identity must not be reused across a changed desktop_id"
        );
        // Reloading again under the new id must now reuse what was just
        // generated, not regenerate a third time.
        let reloaded = load_or_create(&path, "desktop-new", "development")
            .expect("reload under the new id reuses it");
        assert_eq!(new, reloaded);
    }

    #[test]
    fn a_changed_environment_regenerates_rather_than_reusing_the_old_identity() {
        let path = temp_identity_path();
        let old = load_or_create(&path, "desktop-1", "development").expect("create old");

        let new = load_or_create(&path, "desktop-1", "staging")
            .expect("a changed environment regenerates rather than failing closed");

        assert_ne!(
            old, new,
            "the identity must not be reused across a changed environment - a staging and a \
             production identity minted under the same reused config directory must never be \
             interchangeable"
        );
    }

    #[test]
    fn an_invalid_dns_desktop_id_still_produces_a_usable_certificate() {
        // desktop ids are opaque application identifiers (e.g. often include
        // characters SAN DNS names cannot), and generation must not fail on
        // one just because it does not already look like a hostname.
        let identity = generate("desktop_with_underscore!").expect("generate identity");
        assert!(identity
            .leaf_certificate_pem
            .starts_with("-----BEGIN CERTIFICATE-----"));
    }
}
