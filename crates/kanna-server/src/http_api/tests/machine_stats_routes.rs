use super::*;
use crate::http_api::state::DesktopRelayRequest;
use serde_json::{json, Value};

// Real listener -> auth middleware -> aggregate handler -> relay invoke boundary
// -> sibling router -> native two-point sampler. A broken local collector must
// not erase the sibling's CPU sample.
#[tokio::test]
async fn machine_stats_http_relay_keeps_native_peer_when_local_collection_fails() {
    let remote = test_state_with_seed("stats-native-peer", "Native peer", |_| {});
    let mut local = (*test_state_with_seed("stats-broken-local", "Broken local", |_| {})).clone();
    let invalid_db = tempfile::tempdir().unwrap();
    local.config.db_path = invalid_db.path().to_string_lossy().into_owned(); // a directory, not SQLite
    let local = Arc::new(local);
    let mut requests = local.take_desktop_relay_requests().unwrap();
    local.set_desktop_routing_available(true);
    let relay = tokio::spawn(async move {
        let DesktopRelayRequest::ListActive { response, .. } = requests.recv().await.unwrap()
        else {
            panic!("expected listing")
        };
        response.send(Ok(vec!["stats-native-peer".into()])).unwrap();
        let DesktopRelayRequest::Invoke {
            desktop_id,
            method,
            path,
            body,
            response,
            ..
        } = requests.recv().await.unwrap()
        else {
            panic!("expected invoke")
        };
        assert_eq!(desktop_id, "stats-native-peer");
        assert_eq!(path, "/v1/machine-stats?localOnly=true");
        response
            .send(Ok(crate::http_api::dispatch_authenticated_http_invoke(
                remote, &method, &path, body,
            )
            .await))
            .unwrap();
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(local).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let response = reqwest::Client::new()
        .get(format!("http://{address}/v1/machine-stats"))
        .send()
        .await;
    let response = match response {
        Ok(response) => response.json::<Value>().await,
        Err(error) => {
            server.abort();
            relay.abort();
            panic!("HTTP request failed: {error}")
        }
    };
    server.abort();
    let _ = server.await;
    relay.await.unwrap();
    let body = response.unwrap();
    assert_eq!(body["machines"].as_array().unwrap().len(), 1, "{body}");
    let peer = &body["machines"][0];
    assert_eq!(peer["machineId"], "stats-native-peer");
    assert!(peer["sampledAt"].is_u64(), "{body}");
    assert!(
        peer["cpu"]["sampleWindowMs"].as_u64().unwrap() >= 500,
        "{body}"
    );
    assert!(peer["cpu"]["busyPercent"].as_f64().unwrap() <= 100.0);
    assert!(peer["processes"]["topProcesses"].as_array().unwrap().len() <= 10);
    assert_eq!(body["machineErrors"][0]["machineId"], "stats-broken-local");
    assert!(body["machineErrors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("database"));
}

#[tokio::test]
async fn machine_stats_browser_requests_still_require_credentials() {
    let state = test_state_with_seed("stats-auth", "Stats Auth", |_| {});
    let token = state.local_task_events_token.clone().unwrap();
    let app = router(state);
    let request =
        |credential: Option<&str>| {
            let mut request = Request::get("/v1/machine-stats?localOnly=true")
                .header("host", "localhost")
                .header("origin", "https://example.invalid");
            if let Some(token) = credential {
                request = request.header("x-kanna-local-token", token);
            }
            let mut request = request.body(Body::empty()).unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
            ));
            request
        };
    assert_eq!(
        app.clone().oneshot(request(None)).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.oneshot(request(Some(&token))).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn machine_stats_concurrent_http_requests_share_sample_provenance() {
    let app = test_router("stats-shared", "Shared stats");
    let request = || {
        Request::get("/v1/machine-stats?localOnly=true")
            .body(Body::empty())
            .unwrap()
    };
    let (a, b) = tokio::join!(app.clone().oneshot(request()), app.oneshot(request()));
    let a: Value = from_slice(
        &axum::body::to_bytes(a.unwrap().into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let b: Value = from_slice(
        &axum::body::to_bytes(b.unwrap().into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(a["machineErrors"], json!([]));
    assert_eq!(b["machineErrors"], json!([]));
    assert!(a["machines"][0]["cpu"].is_object(), "{a}");
    assert_eq!(a["machines"][0]["cpu"], b["machines"][0]["cpu"]);
    assert_eq!(a["machines"][0]["sampledAt"], b["machines"][0]["sampledAt"]);
}
