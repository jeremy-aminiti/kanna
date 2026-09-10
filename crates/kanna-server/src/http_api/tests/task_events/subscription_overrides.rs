//! Bounded per-subscription knobs: validated quiet/max-hold/admission
//! overrides and the event_types/exclude_event_types filter passthrough.
//! These reuse the subscription's own query/timing ownership — no new
//! scheduler, no runtime retry loop.
use super::*;
use crate::db::TaskEventKind;

async fn subscribe(app: &Router, body: Value) -> (StatusCode, Value) {
    subscription_request(app, "POST", "/v1/event-subscriptions", body).await
}

#[tokio::test]
async fn invalid_timing_overrides_are_rejected_before_any_registration() {
    let state = test_state_with_seed("overrides-invalid", "Overrides", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    start_run(&db, "manager", "child-c", "in progress");
    let app = router(state.clone());
    let base = json!({"taskId":"child-c", "localOnly":true, "delivery":"poll"});
    for (field, value, expectation) in [
        ("quietMs", json!(999), "below the 1000ms floor"),
        ("maxHoldMs", json!(0), "below the 1000ms floor"),
        (
            "minAdmissionIntervalMs",
            json!(500),
            "below the 1000ms floor",
        ),
    ] {
        let mut body = base.clone();
        body[field] = value;
        let (status, response) = subscribe(&app, body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{field} {expectation}: {response}"
        );
    }
    // max_hold_ms below quiet_ms is a degenerate pair even though both clear
    // the floor individually.
    let mut inverted = base.clone();
    inverted["quietMs"] = json!(5_000);
    inverted["maxHoldMs"] = json!(2_000);
    let (status, response) = subscribe(&app, inverted).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    // Nothing was left half-registered by a rejected request.
    assert!(db.event_subscriptions().unwrap().is_empty());
}

#[tokio::test]
async fn omitted_overrides_keep_the_exact_prior_query_shape() {
    let state = test_state_with_seed("overrides-omitted", "Overrides", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    start_run(&db, "manager", "child-c", "in progress");
    let app = router(state.clone());
    let (status, initial) = subscribe(
        &app,
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    // An untouched request never persists the override keys, so a
    // pre-existing row's `existing.query != query` retry comparison is
    // unaffected by this feature's addition.
    for key in [
        "quietMs",
        "maxHoldMs",
        "minAdmissionIntervalMs",
        "eventTypes",
    ] {
        assert!(initial["query"][key].is_null(), "{key}: {initial}");
    }
    assert_eq!(
        initial["query"]["excludeEventTypes"],
        "task.activity_changed,task.runtime_settled,task.input_delivered"
    );
    let retry = subscribe(
        &app,
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll"}),
    )
    .await
    .1;
    assert_eq!(retry["id"], initial["id"], "retry must reuse the mailbox");
}

#[tokio::test]
async fn explicit_overrides_are_persisted_and_validated_as_a_pair() {
    let state = test_state_with_seed("overrides-persisted", "Overrides", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    start_run(&db, "manager", "child-c", "in progress");
    let app = router(state.clone());
    let (status, initial) = subscribe(
        &app,
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll",
            "quietMs": 2_000, "maxHoldMs": 4_000, "minAdmissionIntervalMs": 1_000,
            "eventTypes": ["task.pr_created"], "excludeEventTypes": ["task.blocked"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    assert_eq!(initial["query"]["quietMs"], 2_000);
    assert_eq!(initial["query"]["maxHoldMs"], 4_000);
    assert_eq!(initial["query"]["minAdmissionIntervalMs"], 1_000);
    assert_eq!(initial["query"]["eventTypes"], "task.pr_created");
    // Additive to the fixed baseline, not a replacement for it.
    let excluded = initial["query"]["excludeEventTypes"].as_str().unwrap();
    for baseline in [
        "task.activity_changed",
        "task.runtime_settled",
        "task.input_delivered",
        "task.blocked",
    ] {
        assert!(excluded.contains(baseline), "{excluded}");
    }
}

#[tokio::test]
async fn quiet_and_max_hold_overrides_seal_an_ordinary_batch_at_the_overridden_window() {
    let state = test_state_with_seed("overrides-quiet", "Overrides", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    start_run(&db, "manager", "child-c", "in progress");
    let app = router(state.clone());
    let (status, initial) = subscribe(
        &app,
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll",
            "quietMs": 1_000, "maxHoldMs": 1_000}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    assert!(initial["pending"].is_null());
    let id = initial["id"].as_str().unwrap().to_string();
    let service = tokio::spawn(super::super::event_subscriptions::run(state.clone()));
    // Ordinary (non-urgent) event: without the override this would need the
    // full 300000ms default to seal, which this real-time test cannot afford
    // to wait out. The floor (1000ms) is the smallest legal override.
    db.append_task_event("child-a", TaskEventKind::PrCreated, json!({}))
        .unwrap();
    let row = await_subscription(&state, &id, |row| row.wake_state == "ready").await;
    assert_eq!(
        event_pairs(row.pending.as_ref().unwrap()),
        vec![("child-a".into(), "task.pr_created".into())]
    );
    service.abort();
    let _ = service.await;
}

#[tokio::test]
async fn event_types_allowlist_admits_only_the_named_types() {
    let state = test_state_with_seed("overrides-allowlist", "Overrides", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    start_run(&db, "manager", "child-c", "in progress");
    let app = router(state.clone());
    let (status, initial) = subscribe(
        &app,
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll",
            "eventTypes": ["task.awaiting_input"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    assert!(initial["pending"].is_null());
    let id = initial["id"].as_str().unwrap().to_string();
    let service = tokio::spawn(super::super::event_subscriptions::run(state.clone()));
    // Urgent on its own, but excluded by the allow-list.
    db.append_task_event("child-a", TaskEventKind::LifecycleFailed, json!({}))
        .unwrap();
    // Named by the allow-list; also urgent, so it seals immediately.
    db.append_task_event("child-b", TaskEventKind::AwaitingInput, json!({}))
        .unwrap();
    let row = await_subscription(&state, &id, |row| row.pending.is_some()).await;
    assert_eq!(
        event_pairs(row.pending.as_ref().unwrap()),
        vec![("child-b".into(), "task.awaiting_input".into())]
    );
    service.abort();
    let _ = service.await;
}

#[tokio::test]
async fn exclude_event_types_is_additive_to_the_fixed_baseline() {
    let state = test_state_with_seed("overrides-exclude", "Overrides", seed_orchestration);
    let db = Db::open(&state.config().db_path).unwrap();
    start_run(&db, "manager", "child-c", "in progress");
    let app = router(state.clone());
    let (status, initial) = subscribe(
        &app,
        json!({"taskId":"child-c", "localOnly":true, "delivery":"poll",
            "excludeEventTypes": ["task.awaiting_input"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    let id = initial["id"].as_str().unwrap().to_string();
    let service = tokio::spawn(super::super::event_subscriptions::run(state.clone()));
    // Excluded by the caller's own addition, on top of the baseline.
    db.append_task_event("child-a", TaskEventKind::AwaitingInput, json!({}))
        .unwrap();
    // Not excluded; urgent, so it seals the same collection.
    db.append_task_event("child-a", TaskEventKind::LifecycleFailed, json!({}))
        .unwrap();
    let row = await_subscription(&state, &id, |row| row.pending.is_some()).await;
    assert_eq!(
        event_pairs(row.pending.as_ref().unwrap()),
        vec![("child-a".into(), "task.lifecycle_failed".into())]
    );
    service.abort();
    let _ = service.await;
}
