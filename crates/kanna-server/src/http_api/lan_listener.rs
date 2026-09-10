//! The dedicated, sidecar-independent LAN machine-invoke listener: a
//! standard-rustls TLS server, on its own port, serving exactly one
//! endpoint gated by [`LanMachineInvokeAuthenticated`]'s bearer-secret
//! check. This is deliberately not the general HTTP router's own port or
//! trust model - a caller here has proved nothing but possession of an
//! automatic same-account bearer secret, so it is a strictly smaller
//! surface than the loopback/relay-authenticated general API.

use super::lan_trust::LanMachineInvokeAuthenticated;
use super::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LanInvokeRequest {
    method: String,
    path: String,
    #[serde(default)]
    body: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LanInvokeResponse {
    status: u16,
    body: Option<serde_json::Value>,
    error: Option<String>,
}

/// Mirrors `cloud_desktops::validate_invoke_request`'s shape/method/
/// recursive-proxy checks - duplicated rather than shared because that
/// function's signature is tied to a `desktop_id` addressing concern this
/// single-target listener does not have.
fn validate_lan_invoke_request(request: &LanInvokeRequest) -> Result<(), String> {
    if !matches!(request.method.as_str(), "GET" | "POST" | "PATCH") {
        return Err("LAN invoke method must be GET, POST, or PATCH".to_string());
    }
    if !request.path.starts_with("/v1/")
        || request.path.contains("://")
        || request.path.chars().any(char::is_control)
        || request.path.starts_with("/v1/cloud/desktops")
        || request.path.starts_with("/v1/lan-routing")
    {
        return Err("LAN invoke path must be a non-recursive /v1/ API path".to_string());
    }
    Ok(())
}

async fn handle_invoke(
    source: LanMachineInvokeAuthenticated,
    State(state): State<Arc<AppState>>,
    Json(request): Json<LanInvokeRequest>,
) -> Result<Json<LanInvokeResponse>, (StatusCode, String)> {
    if let Err(error) = validate_lan_invoke_request(&request) {
        return Err((StatusCode::BAD_REQUEST, error));
    }
    let response = super::routes::dispatch_authenticated_lan_http_invoke(
        state,
        source.source_desktop_id,
        &request.method,
        &request.path,
        request.body,
    )
    .await;
    Ok(Json(LanInvokeResponse {
        status: response.status,
        body: response.body,
        error: response.error,
    }))
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/invoke", post(handle_invoke))
        .with_state(state)
}

/// A `axum::serve`-compatible [`axum::serve::Listener`] that pairs a plain
/// TCP accept with the standard-rustls TLS handshake before handing the
/// resulting stream to axum - so `axum::serve`'s own well-tested connection
/// serving is reused unchanged; nothing here reimplements HTTP.
struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.tcp.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    log::warn!("LAN machine-invoke listener accept failed: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls_stream) => return (tls_stream, addr),
                Err(error) => {
                    // A failed handshake (wrong CA, wrong name, a port scan,
                    // a stray TCP connection) must not take the listener
                    // down - only the one connection is dropped.
                    log::warn!("LAN machine-invoke TLS handshake failed from {addr}: {error}");
                    continue;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Resolves this desktop's identity and binds the socket without serving
/// yet, so a caller (production startup, or a test standing in for a real
/// candidate address) can learn the bound address first. Never falls back
/// to plaintext: a failure to load the identity or bind the port is
/// returned as an error rather than silently degrading.
async fn bind(state: &Arc<AppState>, port: u16) -> Result<(TlsListener, SocketAddr), String> {
    let identity_path = state
        .config()
        .lan_tls_identity_path()
        .ok_or_else(|| "LAN TLS identity is not configured".to_string())?;
    let identity =
        crate::lan_tls_identity::load_or_create(&identity_path, &state.config().desktop_id)?;
    let server_config = crate::lan_tls::server_config(&identity)?;
    let bind_addr = format!("{}:{port}", state.config().lan_host);
    let tcp = TcpListener::bind(&bind_addr).await.map_err(|error| {
        format!("failed to bind LAN machine-invoke listener on {bind_addr}: {error}")
    })?;
    let addr = tcp.local_addr().map_err(|error| {
        format!("failed to read LAN machine-invoke listener address: {error}")
    })?;
    Ok((
        TlsListener {
            tcp,
            acceptor: TlsAcceptor::from(server_config),
        },
        addr,
    ))
}

/// Binds and serves the LAN machine-invoke listener on `port` until this
/// desktop's own persisted TLS identity or the port itself is unavailable.
pub(crate) async fn serve(state: Arc<AppState>, port: u16) -> Result<(), String> {
    let (listener, addr) = bind(&state, port).await?;
    log::info!("LAN machine-invoke listener on {addr}");
    axum::serve(listener, router(state))
        .await
        .map_err(|error| format!("LAN machine-invoke listener failed: {error}"))
}

/// Test-only: binds on an ephemeral port, spawns the serve loop in the
/// background, and returns the real bound address - so a test can point a
/// real client at a real listener without production code needing to know
/// about this at all.
#[cfg(test)]
pub(super) async fn spawn_for_test(state: Arc<AppState>) -> SocketAddr {
    let (listener, addr) = bind(&state, 0).await.expect("bind LAN listener for test");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    addr
}
