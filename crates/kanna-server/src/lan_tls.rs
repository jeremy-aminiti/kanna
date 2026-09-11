//! Builds standard rustls configs for the authenticated LAN machine-invoke
//! transport, from a `lan_tls_identity::LanTlsIdentity`.
//!
//! Deliberately no custom certificate verifier anywhere in this module. The
//! accepted architecture is standard rustls verification against a
//! target-specific `RootCertStore` loaded with only the relay-attested
//! target's CA certificate: ordinary WebPKI chain validation, ordinary
//! `ServerName`/hostname verification against the leaf's SAN, no OS/public
//! roots, and no `dangerous()`/bypass API anywhere. Discovery (Bonjour) may
//! supply the address to *connect* to; it must never supply, or substitute
//! for, the identity that address is verified against - the caller connects
//! the TCP socket wherever the candidate says, then hands this module the
//! desktop_id the bootstrap actually attested, never anything read off the
//! discovered address itself.

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::sync::Arc;

/// A standard rustls server config presenting this desktop's leaf
/// certificate and private key. Carries no client-auth requirement: the
/// caller is authenticated by the bearer secret the gateway checks after the
/// handshake (see `machine_trust::MachineTrustStore::verify_inbound`), not
/// by a client certificate.
pub fn server_config(
    identity: &crate::lan_tls_identity::LanTlsIdentity,
) -> Result<Arc<ServerConfig>, String> {
    let leaf = parse_certificate(&identity.leaf_certificate_pem, "leaf certificate")?;
    let key = parse_private_key(&identity.leaf_private_key_pem)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![leaf], key)
        .map_err(|error| format!("failed to build LAN TLS server config: {error}"))?;
    Ok(Arc::new(config))
}

/// A standard rustls client config trusting exactly one CA - the one a relay
/// bootstrap attested for the target this connection is about to dial.
/// Nothing else is trusted: no system roots, no other same-account
/// sibling's CA, nothing discovered on the LAN itself.
pub fn client_config_pinned_to_ca(ca_certificate_pem: &str) -> Result<Arc<ClientConfig>, String> {
    let ca = parse_certificate(ca_certificate_pem, "CA certificate")?;
    let mut roots = RootCertStore::empty();
    roots
        .add(ca)
        .map_err(|error| format!("failed to trust pinned LAN TLS CA certificate: {error}"))?;
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn parse_certificate(pem: &str, label: &str) -> Result<CertificateDer<'static>, String> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let mut certs = rustls_pemfile::certs(&mut reader);
    let first = certs
        .next()
        .ok_or_else(|| format!("no {label} found in PEM"))?
        .map_err(|error| format!("failed to parse {label}: {error}"))?;
    if certs.next().is_some() {
        return Err(format!(
            "expected exactly one {label} in PEM, found more than one"
        ));
    }
    Ok(first)
}

fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .map_err(|error| format!("failed to parse LAN TLS private key: {error}"))?
        .ok_or_else(|| "no private key found in PEM".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan_tls_identity::LanTlsIdentity;
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    /// The `ServerName` a client must present for the handshake's standard
    /// hostname verification to succeed - the target desktop_id the
    /// bootstrap attested, never anything read off the discovered candidate
    /// address. Production dials via `reqwest` instead (see
    /// `invoke_desktop::dial_lan_invoke`), which derives the same `ServerName`
    /// from the request URL's host (`target_desktop_id`) plus its
    /// `.resolve()` override; this helper exists to prove that same
    /// hostname-verification contract directly against the raw rustls
    /// primitives this module builds, one level below reqwest's own
    /// abstraction.
    fn server_name_for_desktop(desktop_id: &str) -> Result<ServerName<'static>, String> {
        ServerName::try_from(desktop_id.to_string()).map_err(|error| {
            format!("desktop id {desktop_id} is not a valid TLS server name: {error}")
        })
    }

    fn generate_identity(desktop_id: &str) -> LanTlsIdentity {
        let path = crate::test_paths::unique_test_path("lan-tls-handshake-identity");
        crate::lan_tls_identity::load_or_create(&path, desktop_id, "development")
            .expect("generate identity")
    }

    /// Binds a real loopback listener presenting `server_identity`, attempts
    /// a real TLS handshake from a client configured with `client_ca_pem`
    /// (the CA a client's RootCertStore trusts) verifying against
    /// `client_server_name`, and returns whether the handshake - and nothing
    /// past it - succeeded. No application byte is ever sent; a positive
    /// case proves the handshake alone, a negative case proves rejection
    /// happens before any byte a caller would consider a request or a
    /// bearer secret could be written.
    async fn attempt_handshake(
        server_identity: &LanTlsIdentity,
        client_ca_pem: &str,
        client_server_name: ServerName<'static>,
    ) -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind loopback listener: {error}"))?;
        let addr = listener
            .local_addr()
            .map_err(|error| format!("read loopback addr: {error}"))?;

        let acceptor = TlsAcceptor::from(server_config(server_identity).expect("server config"));
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept tcp");
            acceptor.accept(stream).await
        });

        let connector =
            TlsConnector::from(client_config_pinned_to_ca(client_ca_pem).expect("client config"));
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|error| format!("connect tcp: {error}"))?;
        let client_result = connector.connect(client_server_name, tcp).await;

        let server_result = server_task.await.expect("server task panicked");

        match (client_result, server_result) {
            (Ok(_), Ok(_)) => Ok(()),
            (Err(client_error), _) => Err(format!("client handshake failed: {client_error}")),
            (_, Err(server_error)) => Err(format!("server handshake failed: {server_error}")),
        }
    }

    #[tokio::test]
    async fn a_client_pinned_to_the_real_ca_and_the_right_name_completes_the_handshake() {
        let identity = generate_identity("desktop-target");
        let name = server_name_for_desktop("desktop-target").expect("server name");

        attempt_handshake(&identity, &identity.ca_certificate_pem, name)
            .await
            .expect("handshake between a genuine pinned pair must succeed");
    }

    #[tokio::test]
    async fn a_client_pinned_to_an_unrelated_ca_is_rejected_before_any_application_byte() {
        let real_identity = generate_identity("desktop-target");
        let impostor_identity = generate_identity("desktop-impostor");
        let name = server_name_for_desktop("desktop-target").expect("server name");

        let error = attempt_handshake(
            &real_identity,
            // The client trusts a CA that never issued the server's presented
            // leaf - simulating an on-path attacker or a spoofed Bonjour
            // candidate the client happened to dial.
            &impostor_identity.ca_certificate_pem,
            name,
        )
        .await
        .expect_err("a client trusting the wrong CA must never complete the handshake");
        assert!(error.contains("client handshake failed"), "{error}");
    }

    #[tokio::test]
    async fn a_client_expecting_the_wrong_server_name_is_rejected_even_with_the_right_ca() {
        let identity = generate_identity("desktop-target");
        // Correct CA, but verifying against a name the leaf's SAN does not
        // carry - simulating a client that resolved the right trust anchor
        // for the wrong target (e.g. a stale/attacker-controlled candidate
        // answering on the address this desktop_id used to have).
        let wrong_name = server_name_for_desktop("desktop-someone-else").expect("server name");

        let error = attempt_handshake(&identity, &identity.ca_certificate_pem, wrong_name)
            .await
            .expect_err("a server name mismatch must never complete the handshake");
        assert!(error.contains("client handshake failed"), "{error}");
    }

    #[tokio::test]
    async fn a_real_handshake_can_carry_application_bytes_once_both_sides_verify() {
        let identity = generate_identity("desktop-target");
        let name = server_name_for_desktop("desktop-target").expect("server name");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor = TlsAcceptor::from(server_config(&identity).expect("server config"));
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept tcp");
            let mut tls = acceptor.accept(stream).await.expect("server handshake");
            let mut buf = [0u8; 5];
            tls.read_exact(&mut buf).await.expect("read from client");
            assert_eq!(&buf, b"hello");
            tls.write_all(b"world").await.expect("write to client");
        });

        let connector =
            TlsConnector::from(client_config_pinned_to_ca(&identity.ca_certificate_pem).unwrap());
        let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut tls = connector
            .connect(name, tcp)
            .await
            .expect("client handshake");
        tls.write_all(b"hello").await.expect("write to server");
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await.expect("read from server");
        assert_eq!(&buf, b"world");

        server_task.await.expect("server task panicked");
    }
}
