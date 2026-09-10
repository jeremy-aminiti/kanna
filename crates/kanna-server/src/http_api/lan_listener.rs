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
    let identity = crate::lan_tls_identity::load_or_create(
        &identity_path,
        &state.config().desktop_id,
        &state.config().environment,
    )?;
    let server_config = crate::lan_tls::server_config(&identity)?;
    let bind_addr = format!("{}:{port}", state.config().lan_host);
    let tcp = TcpListener::bind(&bind_addr).await.map_err(|error| {
        format!("failed to bind LAN machine-invoke listener on {bind_addr}: {error}")
    })?;
    let addr = tcp
        .local_addr()
        .map_err(|error| format!("failed to read LAN machine-invoke listener address: {error}"))?;
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
/// `on_bound` runs exactly once, only after the bind actually succeeds, with
/// the real bound address - the caller uses it to start advertising the
/// listener only once there is something real to advertise, never before
/// (see `runtime::run_lan_machine_invoke_listener`).
pub(crate) async fn serve(
    state: Arc<AppState>,
    port: u16,
    on_bound: impl FnOnce(SocketAddr),
) -> Result<(), String> {
    let (listener, addr) = bind(&state, port).await?;
    on_bound(addr);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(desktop_id: &str) -> crate::config::Config {
        let dir = crate::test_paths::unique_test_dir(&format!("lan-listener-{desktop_id}"));
        crate::config::Config {
            relay_url: String::new(),
            device_token: "device-token".to_string(),
            firebase_project_id: "kanna-local".to_string(),
            firebase_auth_emulator_url: None,
            firebase_firestore_emulator_host: None,
            daemon_dir: dir.join("daemon").to_string_lossy().into_owned(),
            db_path: crate::db::Db::test_db_path(&format!("lan-listener-{desktop_id}")),
            kanna_cli_path: None,
            desktop_id: desktop_id.to_string(),
            desktop_secret: Some("desktop-secret".to_string()),
            desktop_name: format!("{desktop_id} Mac"),
            version: "test-version".to_string(),
            environment: "development".to_string(),
            lan_host: "127.0.0.1".to_string(),
            lan_port: 48120,
            transfer_port: 4455,
            lan_routing_port: 4460,
            activity_event_debounce_seconds: 300,
            pairing_store_path: dir.join("pairings.json").to_string_lossy().into_owned(),
        }
    }

    /// The production ordering contract this whole `on_bound` parameter
    /// exists for: a caller learns the real bound address (and so only
    /// starts advertising) exactly when, and only when, the bind actually
    /// succeeded.
    #[tokio::test]
    async fn on_bound_fires_once_with_the_real_bound_address_after_a_successful_bind() {
        let state = Arc::new(AppState::new(test_config("desktop-bind-success")));
        let observed = Arc::new(std::sync::Mutex::new(Vec::<SocketAddr>::new()));
        let observed_in_callback = Arc::clone(&observed);

        let serving = tokio::spawn(async move {
            serve(state, 0, move |addr| {
                observed_in_callback.lock().unwrap().push(addr);
            })
            .await
        });
        // Let the bind complete and on_bound run before inspecting it or
        // tearing the task down - it never actually serves a request.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        serving.abort();

        let calls = observed.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "on_bound must fire exactly once");
        assert_ne!(
            calls[0].port(),
            0,
            "on_bound must report the real bound port, not the requested ephemeral 0"
        );
    }

    /// The other half of the same contract: a failed bind (here, a port
    /// already held by another listener) must never advertise anything -
    /// on_bound must not fire at all.
    #[tokio::test]
    async fn on_bound_never_fires_when_the_port_is_already_taken() {
        let holder_state = Arc::new(AppState::new(test_config("desktop-bind-holder")));
        let (holder_listener, holder_addr) = bind(&holder_state, 0)
            .await
            .expect("bind the port-holding listener");
        // Keep the holder's TCP socket alive (but never serving) for the
        // duration of this test, so the port stays genuinely occupied.
        let _holder_task = tokio::spawn(async move {
            let _ = axum::serve(holder_listener, router(holder_state)).await;
        });

        let contending_state = Arc::new(AppState::new(test_config("desktop-bind-contender")));
        let on_bound_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let on_bound_called_in_callback = Arc::clone(&on_bound_called);

        let result = serve(contending_state, holder_addr.port(), move |_addr| {
            on_bound_called_in_callback.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;

        assert!(
            result.is_err(),
            "binding an already-occupied port must fail"
        );
        assert!(
            !on_bound_called.load(std::sync::atomic::Ordering::SeqCst),
            "on_bound must never fire for a bind that failed"
        );
    }
}
