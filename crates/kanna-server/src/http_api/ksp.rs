use super::lan_trust::{BrowserOriginatedRequest, LocalControlCredential, TrustedLanDeviceAccess};
use super::state::AppState;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, State};
use axum::Extension;
use std::net::SocketAddr;
use std::sync::Arc;

pub(super) async fn legacy_ksp_stream(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    trusted_lan_device: Option<Extension<TrustedLanDeviceAccess>>,
    browser: Option<Extension<BrowserOriginatedRequest>>,
    local_credential: Option<Extension<LocalControlCredential>>,
) -> axum::response::Response {
    let auth_mode = direct_stream_auth_mode(
        peer,
        true,
        trusted_lan_device.is_some(),
        browser.is_some(),
        local_credential.is_some(),
    );
    let companion_access = direct_stream_companion_access(trusted_lan_device, auth_mode);
    ws.on_upgrade(move |socket| {
        crate::ksp::handle_stream(socket, state, auth_mode, companion_access)
    })
}

pub(super) async fn ksp_stream(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    trusted_lan_device: Option<Extension<TrustedLanDeviceAccess>>,
    browser: Option<Extension<BrowserOriginatedRequest>>,
    local_credential: Option<Extension<LocalControlCredential>>,
) -> axum::response::Response {
    let auth_mode = direct_stream_auth_mode(
        peer,
        false,
        trusted_lan_device.is_some(),
        browser.is_some(),
        local_credential.is_some(),
    );
    let companion_access = direct_stream_companion_access(trusted_lan_device, auth_mode);
    ws.on_upgrade(move |socket| {
        crate::ksp::handle_stream(socket, state, auth_mode, companion_access)
    })
}

fn direct_stream_auth_mode(
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    legacy_v1: bool,
    paired_at_upgrade: bool,
    browser_originated: bool,
    local_credential_at_upgrade: bool,
) -> crate::ksp::AuthMode {
    let peer_is_loopback = peer.is_some_and(|Extension(ConnectInfo(peer))| peer.ip().is_loopback());
    if browser_originated && peer_is_loopback && !local_credential_at_upgrade {
        // A browser reaches loopback as easily as the desktop webview does,
        // and a WebSocket upgrade is not a CORS request — nothing in the
        // browser stops a hostile page from opening this stream. It cannot
        // attach a header to the handshake either, so the credential the
        // header middleware would have demanded is proved in the first `auth`
        // frame instead. Android emulator NAT also presents the paired mobile
        // app as a loopback peer, though, and React Native supplies an Origin.
        // A verified paired-device header at upgrade is authority to ask for
        // the same paired secret in-band; it must not be reclassified as a
        // desktop webview that could possess the local control token.
        if paired_at_upgrade {
            if legacy_v1 {
                crate::ksp::AuthMode::LegacyReadOnlyOrPaired
            } else {
                crate::ksp::AuthMode::RequirePairedDevice
            }
        } else {
            crate::ksp::AuthMode::RequireLocalControlToken
        }
    } else if peer_is_loopback {
        crate::ksp::AuthMode::AllowEmpty
    } else if legacy_v1 && paired_at_upgrade {
        // Preserve legacy header/cookie-authenticated readers. They have
        // already proved pairing; empty in-band auth still grants only the
        // existing read-only access, not privileged frames.
        crate::ksp::AuthMode::LegacyReadOnlyOrPaired
    } else {
        // An upgrade alone grants no data access, including on v1. A caller
        // without upgrade-time pairing must prove it in the first Auth frame.
        crate::ksp::AuthMode::RequirePairedDevice
    }
}

/// Companion streams need a verified paired device: upgrade-time device
/// headers or the stream cookie. An in-band paired credential can still earn
/// companion access during `Auth`; a bare loopback stream cannot — the local
/// desktop uses the companion bridge, not KSP, for its own server.
fn direct_stream_companion_access(
    trusted_lan_device: Option<Extension<TrustedLanDeviceAccess>>,
    _auth_mode: crate::ksp::AuthMode,
) -> bool {
    trusted_lan_device.is_some()
}

#[cfg(test)]
mod tests {
    use super::direct_stream_auth_mode;
    use crate::ksp::AuthMode;
    use axum::extract::ConnectInfo;
    use axum::Extension;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn loopback_peer() -> Option<Extension<ConnectInfo<SocketAddr>>> {
        Some(Extension(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            49152,
        ))))
    }

    #[test]
    fn paired_browser_originated_loopback_v2_still_requires_the_paired_secret() {
        assert_eq!(
            direct_stream_auth_mode(loopback_peer(), false, true, true, false),
            AuthMode::RequirePairedDevice
        );
    }

    #[test]
    fn paired_browser_originated_loopback_v1_preserves_legacy_paired_access() {
        assert_eq!(
            direct_stream_auth_mode(loopback_peer(), true, true, true, false),
            AuthMode::LegacyReadOnlyOrPaired
        );
    }

    #[test]
    fn unpaired_browser_originated_loopback_still_requires_local_control() {
        assert_eq!(
            direct_stream_auth_mode(loopback_peer(), false, false, true, false),
            AuthMode::RequireLocalControlToken
        );
    }

    #[test]
    fn native_loopback_client_keeps_local_process_authority() {
        assert_eq!(
            direct_stream_auth_mode(loopback_peer(), false, false, false, false),
            AuthMode::AllowEmpty
        );
    }
}
