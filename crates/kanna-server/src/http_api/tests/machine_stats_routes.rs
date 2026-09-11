use super::*;
use crate::http_api::state::DesktopRelayRequest;
use serde_json::{json, Value};

#[tokio::test]
async fn compact_local_disk_unavailability_is_concise_and_hides_database_path() {
    let marker = "compact-private-database-path";
    let mut state = (*test_state_with_seed("stats-disk-failure", "Disk failure", |_| {})).clone();
    let invalid_db = tempfile::Builder::new().prefix(marker).tempdir().unwrap();
    state.config.db_path = invalid_db.path().to_string_lossy().into_owned(); // directory, not SQLite
    let app = router(Arc::new(state));

    let response = app
        .oneshot(
            Request::get("/v1/machine-stats?localOnly=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value = from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(body["machines"][0]["freeDiskBytes"].is_null(), "{body}");
    assert_eq!(
        body["machines"][0]["errors"],
        json!(["disk unavailable: database could not be opened"])
    );
    assert!(!body.to_string().contains(marker), "{body}");
}

#[tokio::test]
async fn compact_local_stats_do_not_expose_failed_storage_paths() {
    let marker = "compact-private-storage-path";
    let state = test_state_with_seed("stats-storage-filter", "Storage filter", |db| {
        db.insert_test_repo_with_path("broken-storage", &format!("/{marker}\0/repo"), "Broken")
            .unwrap();
    });
    let app = router(state);

    let compact = app
        .clone()
        .oneshot(
            Request::get("/v1/machine-stats?localOnly=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let compact: Value = from_slice(
        &axum::body::to_bytes(compact.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        compact["machines"][0]["freeDiskBytes"].is_u64(),
        "{compact}"
    );
    assert!(compact["machines"][0].get("errors").is_none(), "{compact}");
    assert!(!compact.to_string().contains(marker), "{compact}");

    let detailed = app
        .oneshot(
            Request::get("/v1/machine-stats?localOnly=true&detailed=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detailed: Value = from_slice(
        &axum::body::to_bytes(detailed.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        detailed["machines"][0]["collectionErrors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().is_some_and(|error| error.contains(marker))),
        "{detailed}"
    );
}

#[tokio::test]
async fn compact_legacy_remote_filters_detailed_diagnostics_but_detailed_retains_them() {
    let state = test_state_with_seed("stats-aggregate", "Aggregate", |_| {});
    let mut requests = state.take_desktop_relay_requests().unwrap();
    state.set_desktop_routing_available(true);
    let remote_body = json!({
        "machines": [{
            "machineId": "stats-legacy-peer",
            "loadAverages": {"one": 1.0, "five": 2.0, "fifteen": 3.0},
            "cpuCoreCount": 8,
            "memory": {
                "totalBytes": 1000, "usedBytes": 600, "freeBytes": 300,
                "availableBytes": 400,
                "collectionErrors": ["memory pressure probe returned an unsupported diagnostic"]
            },
            "heavyProcessCount": 1,
            "heavyProcesses": {"cargo": 1},
            "busyTaskCount": 0,
            "cpu": {
                "busyPercent": 50.0, "idlePercent": 50.0, "userPercent": 30.0,
                "systemPercent": 20.0, "sampleStartedAt": 1, "sampledAt": 501,
                "sampleWindowMs": 500, "source": "legacy-cpu"
            },
            "processes": {
                "topProcesses": [{
                    "pid": 42, "parentPid": 1, "name": "cargo",
                    "cpuPercent": 50.0, "sampleWindowMs": 500, "residentBytes": 1234
                }],
                "observedProcessCount": 1, "sampledProcessCount": 1,
                "unavailableProcessCount": 0, "truncated": false
            },
            "storage": [],
            "collectionErrors": [
                "CPU topology unavailable",
                "process enumeration failed",
                "storage repo /private/work/repo: permission denied"
            ]
        }],
        "machineErrors": []
    });
    let relay_body = remote_body.clone();
    let relay = tokio::spawn(async move {
        for expected_path in [
            "/v1/machine-stats?localOnly=true",
            "/v1/machine-stats?localOnly=true&detailed=true",
        ] {
            let DesktopRelayRequest::ListActive { response, .. } = requests.recv().await.unwrap()
            else {
                panic!("expected listing")
            };
            response.send(Ok(vec!["stats-legacy-peer".into()])).unwrap();
            let DesktopRelayRequest::Invoke { path, response, .. } = requests.recv().await.unwrap()
            else {
                panic!("expected invoke")
            };
            assert_eq!(path, expected_path);
            response
                .send(Ok(crate::http_api::HttpInvokeResponse {
                    status: 200,
                    body: Some(relay_body.clone()),
                    error: None,
                }))
                .unwrap();
        }
    });
    let app = router(state);

    let compact = app
        .clone()
        .oneshot(
            Request::get("/v1/machine-stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let compact: Value = from_slice(
        &axum::body::to_bytes(compact.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let compact_peer = compact["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|machine| machine["machineId"] == "stats-legacy-peer")
        .unwrap();
    assert!(compact_peer["freeDiskBytes"].is_null(), "{compact}");
    assert_eq!(
        compact_peer["errors"],
        json!(["disk unavailable: peer returned no storage measurement"])
    );
    assert!(compact_peer.get("cpu").is_none(), "{compact}");
    assert!(compact_peer.get("processes").is_none(), "{compact}");
    assert!(
        !compact.to_string().contains("/private/work/repo"),
        "{compact}"
    );

    let detailed = app
        .oneshot(
            Request::get("/v1/machine-stats?detailed=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detailed: Value = from_slice(
        &axum::body::to_bytes(detailed.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    relay.await.unwrap();
    let detailed_peer = detailed["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|machine| machine["machineId"] == "stats-legacy-peer")
        .unwrap();
    assert_eq!(detailed_peer["cpu"]["source"], "legacy-cpu");
    assert_eq!(
        detailed_peer["processes"]["topProcesses"][0]["name"],
        "cargo"
    );
    assert!(
        detailed_peer["collectionErrors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error == "storage repo /private/work/repo: permission denied"),
        "{detailed}"
    );
}

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
        assert_eq!(path, "/v1/machine-stats?localOnly=true&detailed=true");
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
        .get(format!("http://{address}/v1/machine-stats?detailed=true"))
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
    let cpu_error = || {
        peer["collectionErrors"].as_array().and_then(|errors| {
            errors.iter().filter_map(Value::as_str).find(|error| {
                error.starts_with("host_statistics CPU unavailable")
                    || error.starts_with("hw.logicalcpu unavailable")
                    || error.starts_with("/proc/stat:")
                    || error.starts_with("missing /proc/stat CPU row")
                    || error.starts_with("missing aggregate CPU row")
                    || error.starts_with("incomplete CPU counters")
                    || error.starts_with("invalid CPU counter")
                    || error.starts_with("CPU sample window outside")
                    || error.starts_with("CPU topology or counter source changed/unavailable")
                    || error.starts_with("CPU counter reset/wrap")
                    || error.starts_with("CPU counters did not advance")
                    || error.starts_with("CPU counter overflow")
            })
        })
    };
    match peer.get("cpu") {
        Some(Value::Object(cpu)) => {
            assert!(
                cpu["sampleWindowMs"]
                    .as_u64()
                    .is_some_and(|ms| (500..=5_000).contains(&ms)),
                "{body}"
            );
            assert!(
                cpu["busyPercent"]
                    .as_f64()
                    .is_some_and(|percent| (0.0..=100.0).contains(&percent)),
                "{body}"
            );
        }
        None => {
            let cpu_error = cpu_error();
            assert!(
                cpu_error.is_some(),
                "native peer omitted CPU without an availability error: {body}"
            );
            eprintln!("native peer reported unavailable CPU: {cpu_error:?}; response: {body}");
        }
        Some(cpu) => panic!("native peer returned invalid CPU shape {cpu}: {body}"),
    }
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
        Request::get("/v1/machine-stats?localOnly=true&detailed=true")
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
