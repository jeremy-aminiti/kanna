//! This desktop's own stable TLS identity for the authenticated LAN
//! machine-invoke listener.
//!
//! Distinct from every other identity in this crate: `pairing::PairingStore`
//! holds mobile/manual pairing secrets, `machine_trust::MachineTrustStore`
//! holds automatic same-account bearer grants, and this module holds neither
//! a secret nor a grant - it holds the keypair and self-signed certificate a
//! relay-authenticated bootstrap attests to a same-account sibling as this
//! desktop's trust anchor. A sibling that later dials this desktop over LAN
//! pins its outbound connection to exactly this certificate (never system
//! roots, never TOFU, never the Bonjour-discovered address itself) - see
//! `http_api::invoke_desktop`'s documented TLS-client stub for where that
//! pinning eventually happens.
//!
//! The identity is generated once and persisted; it must not regenerate on
//! an ordinary restart; a previously-bootstrapped sibling's pinned trust
//! anchor would silently stop matching, and every outbound grant pointing at
//! it would need re-bootstrapping for no reason connected to any actual
//! compromise or rotation. It regenerates only when the persisted identity
//! is missing or fails to parse - the same fail-closed-on-corruption stance
//! `machine_trust::MachineTrustStore::load_fail_closed` takes, not a
//! silent repair.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// This desktop's LAN TLS identity: a self-signed certificate and the
/// private key that signed it, both PEM-encoded exactly as `rcgen`/
/// `rustls-pemfile` already expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanTlsIdentity {
    pub certificate_pem: String,
    pub private_key_pem: String,
}

/// Loads the persisted identity, generating and persisting a new one only if
/// none exists yet or the persisted file fails to parse as a well-formed
/// identity. `desktop_id` becomes the certificate's subject alternative name,
/// so a caller pinning to this certificate can also assert *which* desktop
/// it expected to reach - the identity binds a specific desktop_id, not just
/// "some same-account sibling".
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
        certificate_pem: identity.certificate_pem,
        private_key_pem: identity.private_key_pem,
    }))
}

fn generate(desktop_id: &str) -> Result<LanTlsIdentity, String> {
    let key_pair =
        rcgen::KeyPair::generate().map_err(|error| format!("failed to generate LAN TLS key: {error}"))?;
    let mut params = rcgen::CertificateParams::new(vec![sanitize_san(desktop_id)])
        .map_err(|error| format!("failed to build LAN TLS certificate params: {error}"))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, desktop_id);
    let certificate = params
        .self_signed(&key_pair)
        .map_err(|error| format!("failed to self-sign LAN TLS certificate: {error}"))?;
    Ok(LanTlsIdentity {
        certificate_pem: certificate.pem(),
        private_key_pem: key_pair.serialize_pem(),
    })
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
    certificate_pem: String,
    private_key_pem: String,
}

/// Atomically persists the identity as 0600 - it contains a private key,
/// never group/other-readable, matching `machine_trust::MachineTrustStore`'s
/// persistence stance for the same reason.
fn save(path: &Path, identity: &LanTlsIdentity) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&PersistedIdentity {
        certificate_pem: identity.certificate_pem.clone(),
        private_key_pem: identity.private_key_pem.clone(),
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
    fn generates_a_parseable_pem_certificate_and_key() {
        let identity = generate("desktop-1").expect("generate identity");
        assert!(identity.certificate_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(identity
            .private_key_pem
            .starts_with("-----BEGIN PRIVATE KEY-----"));
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
        assert!(identity.certificate_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }
}
