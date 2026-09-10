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
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
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

/// Loads the persisted identity, generating and persisting a new one only if
/// none exists yet or the persisted file fails to parse as a well-formed
/// identity. `desktop_id` becomes the leaf certificate's subject alternative
/// name, so a client pinning to the CA can also assert *which* desktop it
/// expected to reach via standard hostname verification against that SAN -
/// the identity binds a specific desktop_id, not just "some same-account
/// sibling holding some CA-issued cert".
pub fn load_or_create(path: &Path, desktop_id: &str) -> Result<LanTlsIdentity, String> {
    if let Some(identity) = try_load(path)? {
        return Ok(identity);
    }
    let identity = generate(desktop_id)?;
    save(path, &identity)?;
    Ok(identity)
}

/// `Ok(None)` for a missing file (the ordinary first-run case); `Err` for a
/// file that exists but fails to parse - fail closed rather than silently
/// discarding and regenerating over what might be an operator's own copy or
/// a partially-written file from a crashed prior run.
fn try_load(path: &Path) -> Result<Option<LanTlsIdentity>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read LAN TLS identity {}: {error}", path.display()))?;
    let identity: PersistedIdentity = serde_json::from_str(&content).map_err(|error| {
        format!(
            "failed to parse LAN TLS identity {}: {error}",
            path.display()
        )
    })?;
    Ok(Some(LanTlsIdentity {
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
    let key_pair =
        KeyPair::generate().map_err(|error| format!("failed to generate LAN TLS CA key: {error}"))?;
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
    ca_certificate_pem: String,
    leaf_certificate_pem: String,
    leaf_private_key_pem: String,
}

/// Atomically persists the identity as 0600 - it contains private keys,
/// never group/other-readable, matching `machine_trust::MachineTrustStore`'s
/// persistence stance for the same reason.
fn save(path: &Path, identity: &LanTlsIdentity) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&PersistedIdentity {
        ca_certificate_pem: identity.ca_certificate_pem.clone(),
        leaf_certificate_pem: identity.leaf_certificate_pem.clone(),
        leaf_private_key_pem: identity.leaf_private_key_pem.clone(),
    })
    .map_err(|error| format!("failed to serialize LAN TLS identity: {error}"))?;
    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let write_result = options
        .open(&temp_path)
        .and_then(|mut file| file.write_all(body.as_bytes()).and_then(|_| file.sync_all()));
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "failed to write LAN TLS identity temp file {}: {error}",
            temp_path.display()
        ));
    }
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "failed to replace LAN TLS identity {} from {}: {error}",
            path.display(),
            temp_path.display()
        )
    })
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
        let first = load_or_create(&path, "desktop-1").expect("first load creates");
        let second = load_or_create(&path, "desktop-1").expect("second load reuses");
        assert_eq!(
            first, second,
            "an ordinary restart must not regenerate the identity"
        );
    }

    #[test]
    fn save_reasserts_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_identity_path();
        load_or_create(&path, "desktop-1").expect("create identity");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "LAN TLS identity file must be owner-read-write only"
        );
    }

    #[test]
    fn a_corrupt_identity_file_fails_closed_instead_of_silently_regenerating() {
        let path = temp_identity_path();
        std::fs::write(&path, b"not valid json").expect("write corrupt file");
        let error = load_or_create(&path, "desktop-1")
            .expect_err("a corrupt identity file must not be silently replaced");
        assert!(error.contains("failed to parse"), "{error}");
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
