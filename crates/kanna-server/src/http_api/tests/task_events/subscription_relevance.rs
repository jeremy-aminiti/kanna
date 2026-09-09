//! Notification selection through the real durable wait and mailbox paths.
//! These fixtures do not launch a provider or mutate a live subscription.
use super::*;
use crate::db::TaskEventKind;

fn run_with_policy(db: &Db, id: &str, task: &str, stage: &str, policy: &str) {
    start_run(db, id, task, stage);
    db.connection_for_e2e_tests()
        .execute(
            "UPDATE stage_run SET completion_transition = ?, agent_provider = 'codex' WHERE id = ?",
            [policy, id],
        )
        .unwrap();
}

async fn selected(state: Arc<AppState>, query: Value) -> Value {
    super::super::super::task_events::wait_subscription_events(state, query)
        .await
        .unwrap()
}

fn query(limit: i64) -> Value {
    json!({"taskIds":"child-a,child-b", "localOnly":true,
        "includeCurrentActivity":false, "timeoutSecs":0, "limit":limit})
}

fn noise(db: &Db, count: usize) {
    for _ in 0..count {
        db.append_task_event(
            "child-a",
            TaskEventKind::RuntimeChanged,
            json!({"runtimeState":"busy"}),
        )
        .unwrap();
        db.append_task_event(
            "child-a",
            TaskEventKind::ActivityChanged,
            json!({"activity":"unread"}),
        )
        .unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn automatic_review_to_pr_and_noise_drain_before_limit_without_changing_raw_history() {
    let state = test_state_with_seed("selection-auto", "Selection", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    run_with_policy(&db, "review", "child-a", "review", "auto");
    db.finish_stage_run("review", "succeeded", Some("success"), None)
        .unwrap();
    db.update_pipeline_item_stage("child-a", "pr").unwrap();
    run_with_policy(&db, "pr", "child-a", "pr", "manual");
    noise(&db, 12);
    let tail = db.latest_task_event_seq().unwrap().to_string();
    let page = selected(state.clone(), query(2)).await;
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["cursor"], tail);
    assert_eq!(page["waitOutcome"], "timeout");
    assert_eq!(page["hasMore"], false);
    db.append_task_event("child-b", TaskEventKind::AwaitingInput, json!({}))
        .unwrap();
    let mut next = query(2);
    next["cursor"] = page["cursor"].clone();
    let attention = selected(state.clone(), next).await;
    assert_eq!(
        event_pairs(&attention),
        vec![("child-b".into(), "task.awaiting_input".into())]
    );
    let raw = get_json_body(
        &router(state),
        "/v1/task-events?taskIds=child-a&localOnly=true&includeCurrentActivity=false&timeoutSecs=0",
    )
    .await;
    assert!(event_pairs(&raw)
        .iter()
        .any(|(_, kind)| kind == "run.finished"));
    assert!(event_pairs(&raw)
        .iter()
        .any(|(_, kind)| kind == "task.activity_changed"));
}

#[tokio::test(start_paused = true)]
async fn mixed_filtered_pages_preserve_failure_after_stage_change_and_exact_continuation() {
    let state = test_state_with_seed("selection-failure", "Selection", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    run_with_policy(&db, "failed-review", "child-a", "review", "auto");
    db.finish_stage_run("failed-review", "failed", Some("failure"), None)
        .unwrap();
    db.update_pipeline_item_stage("child-a", "pr").unwrap();
    run_with_policy(&db, "replacement", "child-a", "pr", "manual");
    noise(&db, 5);
    db.insert_task_blocker("child-b", "child-a").unwrap();
    db.close_pipeline_item("child-a").unwrap();
    noise(&db, 5);
    db.append_task_event("child-b", TaskEventKind::AwaitingInput, json!({}))
        .unwrap();
    let mut q = query(1);
    let mut observed = Vec::new();
    for _ in 0..10 {
        let page = selected(state.clone(), q.clone()).await;
        observed.extend(event_pairs(&page));
        q["cursor"] = page["cursor"].clone();
        if page["events"] == json!([]) {
            break;
        }
    }
    assert_eq!(
        observed,
        vec![
            ("child-a".into(), "run.finished".into()),
            ("child-b".into(), "task.blocked".into()),
            ("child-a".into(), "task.closed".into()),
            ("child-b".into(), "task.unblocked".into()),
            ("child-b".into(), "task.awaiting_input".into()),
        ]
    );
    assert_eq!(q["cursor"], db.latest_task_event_seq().unwrap().to_string());
}

#[tokio::test(start_paused = true)]
async fn excluded_events_neither_fill_batch_nor_start_its_debounce() {
    let state = test_state_with_seed("selection-debounce", "Selection", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    noise(&db, 8);
    let mut q = query(3);
    q["timeoutSecs"] = json!(30);
    q["minEvents"] = json!(2);
    q["debounceMs"] = json!(1000);
    let wait = tokio::spawn(selected(state, q));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!wait.is_finished(), "noise cannot fill an actionable batch");
    db.append_task_event("child-a", TaskEventKind::PrCreated, json!({}))
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(200)).await;
    db.append_task_event("child-b", TaskEventKind::TaskClosed, json!({}))
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        !wait.is_finished(),
        "the hold starts at the first relevant event, not earlier noise"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    let page = wait.await.unwrap();
    assert_eq!(page["waitOutcome"], "events");
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn both_delivery_adapters_share_manual_bootstrap_and_acknowledgement_selection() {
    for delivery in ["input", "codex_app_server"] {
        let state = test_state_with_seed(
            &format!("selection-{delivery}"),
            "Selection",
            seed_orchestration,
        );
        let db = Db::open(&state.config().db_path).unwrap();
        run_with_policy(&db, "manager", "child-c", "in progress", "manual");
        run_with_policy(&db, "manual", "child-a", "in progress", "manual");
        run_with_policy(&db, "automatic", "child-b", "in progress", "auto");
        settle_runtime_tasks(&db, &["child-a", "child-b"]);
        let before = db.get_pipeline_item("child-a").unwrap().unwrap().activity;
        let app = router(state.clone());
        let (status, initial) = subscription_request(
            &app,
            "POST",
            "/v1/event-subscriptions",
            json!({"taskId":"child-c", "localOnly":true, "delivery":delivery}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{initial}");
        assert_eq!(
            event_pairs(&initial["pending"]),
            vec![("child-a".into(), "task.runtime_changed".into())]
        );
        let id = initial["id"].as_str().unwrap();
        let path = format!("/v1/event-subscriptions/{id}/read");
        let (_, read) = subscription_request(&app, "POST", &path, json!({})).await;
        assert_eq!(read["pending"], initial["pending"]);
        let (status, ack) = subscription_request(
            &app,
            "POST",
            &path,
            json!({"acknowledgeBatchId":initial["batchId"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(ack["pending"].is_null());
        assert_eq!(
            db.get_pipeline_item("child-a").unwrap().unwrap().activity,
            before
        );
        let mut q = json!({"taskIds":"child-a,child-b", "localOnly":true, "timeoutSecs":0});
        q["cursor"] = ack["cursor"].clone();
        let continued = selected(state, q).await;
        assert_eq!(
            continued["events"],
            json!([]),
            "initial manual scan is acknowledged once"
        );
    }
}

// Exercise an independently shipped peer by removing the optional selection
// parameter. Its raw handler still owns native cursors and settled pagination.
fn old_peer(source: &Arc<AppState>, peer: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let mut requests = source.take_desktop_relay_requests().unwrap();
    source.set_desktop_routing_available(true);
    tokio::spawn(async move {
        let mut receivers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(result) = receivers.join_next(), if !receivers.is_empty() => { result.unwrap(); }
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    match request {
                        crate::http_api::DesktopRelayRequest::ListActive { response, .. } => {
                            let _ = response.send(Ok(vec![peer.config().desktop_id.clone()]));
                        }
                        crate::http_api::DesktopRelayRequest::Invoke { method, path, body, response, .. } => {
                            assert!(path.contains("orchestrationNotifications=true"));
                            let path = path.replace("&orchestrationNotifications=true", "");
                            let peer = peer.clone();
                            receivers.spawn(async move {
                                let result = crate::http_api::dispatch_authenticated_http_invoke(peer, &method, &path, body).await;
                                let _ = response.send(Ok(result));
                            });
                        }
                        _ => panic!("unexpected event-watch relay operation"),
                    }
                }
            }
        }
    })
}

#[tokio::test]
async fn old_remote_peer_noise_is_filtered_before_aggregate_limit_and_keeps_its_checkpoint() {
    let source = test_state_with_seed("selection-source", "Source", seed_orchestration);
    let peer = test_state_with_seed("selection-peer", "Peer", seed_orchestration);
    let local_db = Db::open(&source.config().db_path).unwrap();
    let peer_db = Db::open(&peer.config().db_path).unwrap();
    noise(&local_db, 3);
    noise(&peer_db, 3);
    run_with_policy(&peer_db, "old-review", "child-a", "review", "auto");
    peer_db
        .finish_stage_run("old-review", "succeeded", Some("success"), None)
        .unwrap();
    peer_db.update_pipeline_item_stage("child-a", "pr").unwrap();
    run_with_policy(&peer_db, "old-pr", "child-a", "pr", "manual");
    for _ in 0..3 {
        peer_db
            .append_task_event("child-b", TaskEventKind::AwaitingInput, json!({}))
            .unwrap();
        noise(&peer_db, 3);
    }
    local_db
        .append_task_event(
            "child-a",
            TaskEventKind::LifecycleFailed,
            json!({"error":"stage spawn failed"}),
        )
        .unwrap();
    let relay = old_peer(&source, peer);
    let mut q = query(2);
    q["localOnly"] = json!(false);
    let mut observed = Vec::new();
    for _ in 0..20 {
        let page = selected(source.clone(), q.clone()).await;
        assert_eq!(page["machineErrors"], json!([]), "{page}");
        observed.extend(page["events"].as_array().unwrap().iter().cloned());
        q["cursor"] = page["cursor"].clone();
        if observed.len() == 4 {
            break;
        }
    }
    assert_eq!(observed.len(), 4);
    assert_eq!(
        observed
            .iter()
            .filter(|e| e["machineId"] == "selection-peer")
            .count(),
        3
    );
    let unique = observed
        .iter()
        .map(|e| (e["machineId"].to_string(), e["seq"].to_string()))
        .collect::<HashSet<_>>();
    assert_eq!(unique.len(), 4, "truncation must not replay relevant rows");
    for _ in 0..2 {
        let page = selected(source.clone(), q.clone()).await;
        assert_eq!(
            page["events"],
            json!([]),
            "excluded remote tails cannot create empty event wakes"
        );
        assert_eq!(page["waitOutcome"], "timeout");
        q["cursor"] = page["cursor"].clone();
    }
    relay.abort();
    let _ = relay.await;
}

#[tokio::test]
async fn subscription_worker_publishes_only_attention_and_ack_resumes_after_filtered_progress() {
    let state = test_state_with_seed("selection-worker", "Selection", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    run_with_policy(&db, "manager", "child-c", "in progress", "manual");
    run_with_policy(&db, "automatic", "child-a", "in progress", "auto");
    let app = router(state.clone());
    let (_, initial) = subscription_request(
        &app,
        "POST",
        "/v1/event-subscriptions",
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll"}),
    )
    .await;
    assert!(initial["pending"].is_null());
    let id = initial["id"].as_str().unwrap();
    let service = tokio::spawn(super::super::super::event_subscriptions::run(state.clone()));
    db.finish_stage_run("automatic", "succeeded", Some("success"), None)
        .unwrap();
    settle_runtime_tasks(&db, &["child-a"]);
    noise(&db, 60); // More raw rows than the subscription page limit.
    db.append_task_event("child-b", TaskEventKind::AwaitingInput, json!({}))
        .unwrap();
    let first = await_subscription(&state, id, |row| row.wake_state == "ready").await;
    assert_eq!(
        first.batch_id, 1,
        "routine pages must never occupy the mailbox"
    );
    assert_eq!(
        event_pairs(first.pending.as_ref().unwrap()),
        vec![("child-b".into(), "task.awaiting_input".into())]
    );
    noise(&db, 3);
    db.append_task_event(
        "child-b",
        TaskEventKind::LifecycleFailed,
        json!({"error":"handoff failed"}),
    )
    .unwrap();
    let (_, read) = subscription_request(
        &app,
        "POST",
        &format!("/v1/event-subscriptions/{id}/read"),
        json!({}),
    )
    .await;
    assert_eq!(
        read["pending"],
        first.pending.unwrap(),
        "read does not release backpressure"
    );
    let (status, _) = subscription_request(
        &app,
        "POST",
        &format!("/v1/event-subscriptions/{id}/read"),
        json!({"acknowledgeBatchId":first.batch_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second = await_subscription(&state, id, |row| {
        row.batch_id == 2 && row.wake_state == "ready"
    })
    .await;
    assert_eq!(
        event_pairs(second.pending.as_ref().unwrap()),
        vec![("child-b".into(), "task.lifecycle_failed".into())]
    );
    service.abort();
    let _ = service.await;
}

#[tokio::test(start_paused = true)]
async fn excluded_backlog_cannot_extend_deadline_and_resumes_without_cursor_loss() {
    for timeout_secs in [0, 1] {
        let state = test_state_with_seed(
            &format!("selection-deadline-{timeout_secs}"),
            "Selection",
            seed_orchestration,
        );
        let db = Db::open(&state.config().db_path).unwrap();
        noise(&db, 20);
        let tail = db.latest_task_event_seq().unwrap();
        let mut q = query(1);
        q["timeoutSecs"] = json!(timeout_secs);
        let wait = tokio::spawn(selected(state.clone(), q));
        // Let the wait consume a page and yield to drain the filtered backlog.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let page = wait.await.unwrap();
        assert_eq!(page["waitOutcome"], "timeout");
        assert_eq!(page["events"], json!([]));
        assert_eq!(page["hasMore"], true);
        let checkpoint = page["cursor"].as_str().unwrap().parse::<i64>().unwrap();
        assert!(
            checkpoint > 0 && checkpoint < tail,
            "expired wait must stop draining before the tail: {page}"
        );
        db.append_task_event("child-b", TaskEventKind::AwaitingInput, json!({}))
            .unwrap();
        let mut resume = query(1);
        resume["cursor"] = page["cursor"].clone();
        let attention = selected(state.clone(), resume.clone()).await;
        assert_eq!(
            event_pairs(&attention),
            vec![("child-b".into(), "task.awaiting_input".into())]
        );
        resume["cursor"] = attention["cursor"].clone();
        assert_eq!(selected(state, resume).await["events"], json!([]));
    }
}
