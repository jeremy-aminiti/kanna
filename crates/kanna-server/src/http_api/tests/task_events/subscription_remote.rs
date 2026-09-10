//! Subscription worker -> aggregate registry -> relay queue -> peer HTTP wait.
//! The transport is a fixture, with the production long-poll semaphore and
//! peer handler. Receiver work deliberately survives loss of its caller, just
//! like relay.rs; a fake transport that cancels it would hide this regression.
use super::*;
use crate::http_api::{DesktopRelayRequest, HttpInvokeResponse};
use tokio::sync::Semaphore;

#[derive(Default)]
struct Counts {
    attempts: AtomicUsize,
    admitted: AtomicUsize,
    abandoned: AtomicUsize,
    released: AtomicUsize,
    busy: AtomicUsize,
}

struct RelayFixture {
    task: tokio::task::JoinHandle<()>,
    budget: Arc<Semaphore>,
    counts: Arc<Counts>,
}

impl Drop for RelayFixture {
    fn drop(&mut self) {
        // Its JoinSet owns every fake receiver, including abandoned waits.
        self.task.abort();
    }
}

fn connect(source: &Arc<AppState>, peer: Arc<AppState>) -> RelayFixture {
    let mut requests = source.take_desktop_relay_requests().unwrap();
    source.set_desktop_routing_available(true);
    let permits = Arc::new(crate::relay::RelayHttpInvokePermits::new(1));
    let budget = permits.for_path("/v1/task-events");
    let counts = Arc::new(Counts::default());
    let observed = counts.clone();
    let task = tokio::spawn(async move {
        let mut receivers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(result) = receivers.join_next(), if !receivers.is_empty() => {
                    result.expect("peer receiver panicked");
                }
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    match request {
                        DesktopRelayRequest::PublishTaskSnapshot { response, .. } => {
                            let _ = response.send(Ok(()));
                        }
                        DesktopRelayRequest::ListActive { response, .. } => {
                            let _ = response.send(Ok(vec![peer.config().desktop_id.clone()]));
                        }
                        DesktopRelayRequest::Invoke { method, path, body, mut response, .. } => {
                            assert!(path.starts_with("/v1/task-events?"));
                            // Bootstrap uses timeout zero; count the long polls
                            // whose lifetime is at issue separately from it.
                            let long = !path.contains("timeoutSecs=0&");
                            if long { observed.attempts.fetch_add(1, Ordering::SeqCst); }
                            let permit = match permits.for_path(&path).try_acquire_owned() {
                                Ok(permit) => permit,
                                Err(_) => {
                                    observed.busy.fetch_add(1, Ordering::SeqCst);
                                    let _ = response.send(Ok(HttpInvokeResponse {
                                        status: 503, body: None,
                                        error: Some("desktop is busy; too many concurrent requests".into()),
                                    }));
                                    continue;
                                }
                            };
                            if long { observed.admitted.fetch_add(1, Ordering::SeqCst); }
                            let peer = peer.clone();
                            let observed = observed.clone();
                            receivers.spawn(async move {
                                let wait = crate::http_api::dispatch_authenticated_http_invoke(
                                    peer, &method, &path, body,
                                );
                                tokio::pin!(wait);
                                let result = tokio::select! {
                                    result = &mut wait => result,
                                    _ = response.closed() => {
                                        if long { observed.abandoned.fetch_add(1, Ordering::SeqCst); }
                                        // No cancellation message crosses the relay. Keep
                                        // the permit through the actual peer deadline.
                                        wait.await
                                    }
                                };
                                drop(permit);
                                if long { observed.released.fetch_add(1, Ordering::SeqCst); }
                                let _ = response.send(Ok(result));
                            });
                        }
                        _ => panic!("unexpected subscription relay operation"),
                    }
                }
            }
        }
    });
    RelayFixture {
        task,
        budget,
        counts,
    }
}

// Bounded scheduling with a paused Tokio clock. Advancing virtual time lets
// peer handlers and observers settle without sleeping for 240 real seconds.
async fn until(mut condition: impl FnMut() -> bool) {
    // An ordinary (non-urgent) batch's collection window is bounded by the
    // 240s receiver deadline (quiet/max_hold are both 300s, so the receiver
    // wins). 400s of virtual time comfortably covers that plus scheduler
    // turns; the 1s step is coarse because this is a readiness gate, not a
    // timing measurement — precise elapsed time is asserted elsewhere.
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert!(
        condition(),
        "subscription fixture did not reach the expected state"
    );
}

async fn notifications(state: &AppState) {
    for _ in 0..8 {
        state.event_subscriptions_changed.notify_waiters();
        state.publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
    }
}

struct WatchFixture {
    source: Arc<AppState>,
    peer: Arc<AppState>,
    relay: RelayFixture,
    app: Router,
    id: String,
    service: tokio::task::JoinHandle<()>,
}

impl Drop for WatchFixture {
    fn drop(&mut self) {
        self.service.abort();
    }
}

impl WatchFixture {
    fn request() -> Value {
        Self::request_with(json!({"quietMs": 2_000, "maxHoldMs": 10_000}))
    }

    fn request_with(overrides: Value) -> Value {
        // Diagnostic mode: these fixtures assert on the durable internal
        // cursor directly, which the default compact response omits.
        let mut body = json!({"taskId":"manager", "repoId":"repo-pending-source", "delivery":"poll", "diagnostic":true});
        if let Some(extra) = overrides.as_object() {
            for (key, value) in extra {
                body[key] = value.clone();
            }
        }
        body
    }

    async fn new(exhaust_budget: bool) -> (Self, Option<tokio::sync::OwnedSemaphorePermit>) {
        Self::new_with(exhaust_budget, Self::request()).await
    }

    async fn new_with(
        exhaust_budget: bool,
        request: Value,
    ) -> (Self, Option<tokio::sync::OwnedSemaphorePermit>) {
        let (source, peer) = aggregate_pending_leg_states();
        for (state, repo) in [
            (&source, "repo-pending-source"),
            (&peer, "repo-pending-peer"),
        ] {
            Db::open(&state.config().db_path)
                .unwrap()
                .patch_repo(
                    repo,
                    crate::db::RepoPatch {
                        remote_url_hash: Some(Some("sha256:subscription-fixture")),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let db = Db::open(&source.config().db_path).unwrap();
        db.insert_test_pipeline_item(
            "manager",
            "repo-pending-source",
            "manage",
            Some("Manager"),
            "in progress",
            "2026-09-09 00:00:00",
        )
        .unwrap();
        start_run(&db, "manager-run", "manager", "in progress");
        let relay = connect(&source, peer.clone());
        let app = router(source.clone());
        let (status, initial) =
            subscription_request(&app, "POST", "/v1/event-subscriptions", request).await;
        assert_eq!(status, StatusCode::OK, "{initial}");
        assert!(initial["pending"].is_null(), "{initial}");
        assert!(initial["cursor"].as_str().unwrap().starts_with("ks1."));
        // Both bootstrap legs must have established their checkpoint before
        // admitting a 240-second wait (or deliberately exhausting its budget).
        let decoded = decode_cursor(initial["cursor"].as_str().unwrap());
        assert!(decoded["cursorsByMachine"]["desktop-pending-peer"]
            .as_str()
            .unwrap()
            .starts_with("ke1."));
        let held = exhaust_budget.then(|| relay.budget.clone().try_acquire_owned().unwrap());
        let service = tokio::spawn(super::super::super::event_subscriptions::run(
            source.clone(),
        ));
        let fixture = Self {
            source,
            peer,
            relay,
            app,
            id: initial["id"].as_str().unwrap().into(),
            service,
        };
        until(|| fixture.relay.counts.attempts.load(Ordering::SeqCst) == 1).await;
        (fixture, held)
    }

    fn row(&self) -> crate::db::EventSubscription {
        Db::open(&self.source.config().db_path)
            .unwrap()
            .event_subscription(&self.id)
            .unwrap()
            .unwrap()
    }

    async fn page(&self) -> crate::db::EventSubscription {
        until(|| self.row().wake_state == "ready").await;
        self.row()
    }

    async fn ack(&self, row: &crate::db::EventSubscription) -> Value {
        let (status, body) = subscription_request(
            &self.app,
            "POST",
            &format!("/v1/event-subscriptions/{}/read", self.id),
            json!({"acknowledgeBatchId":row.batch_id, "diagnostic":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn register(&self) -> Value {
        let (status, body) = subscription_request(
            &self.app,
            "POST",
            "/v1/event-subscriptions",
            Self::request(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["id"], self.id);
        body
    }
}

fn decode_cursor(cursor: &str) -> Value {
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor.strip_prefix("ks1.").unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn subscription_notifications_and_ack_retain_one_remote_wait() {
    let (watch, _) = WatchFixture::new(false).await;
    let initial = watch.row().cursor;
    notifications(&watch.source).await;
    assert_eq!(watch.row().cursor, initial);
    for pr_number in [101, 102, 103] {
        let before = watch.row().cursor;
        let retry = watch.register().await;
        assert_eq!(retry["cursor"], json!(before));
        notifications(&watch.source).await;
        Db::open(&watch.source.config().db_path)
            .unwrap()
            .update_pipeline_item_pr(
                "pending-local-child",
                Some(pr_number),
                &format!("https://example.test/pull/{pr_number}"),
            )
            .unwrap();
        watch
            .source
            .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
        let page = watch.page().await;
        let batch = page.pending.as_ref().unwrap();
        assert!(
            batch.get("watchError").is_none(),
            "peer errors: {}; attempts={}, abandoned={}",
            batch["machineErrors"],
            watch.relay.counts.attempts.load(Ordering::SeqCst),
            watch.relay.counts.abandoned.load(Ordering::SeqCst),
        );
        assert_eq!(
            event_pairs(batch),
            vec![("pending-local-child".into(), "task.pr_created".into())]
        );
        assert_eq!(
            page.cursor, before,
            "delivery must not acknowledge the page"
        );
        assert_eq!(watch.register().await["pending"], *batch);
        let acked = watch.ack(&page).await;
        assert_eq!(acked["cursor"], batch["cursor"]);
        assert_ne!(acked["cursor"], json!(before));
    }
    notifications(&watch.source).await;
    assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
    assert_eq!(watch.relay.counts.busy.load(Ordering::SeqCst), 0);
    Db::open(&watch.peer.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-peer-child",
            Some(201),
            "https://example.test/pull/201",
        )
        .unwrap();
    let remote = watch.page().await;
    assert_eq!(
        event_pairs(remote.pending.as_ref().unwrap()),
        vec![("pending-peer-child".into(), "task.pr_created".into())]
    );
    assert_eq!(watch.relay.counts.released.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn subscription_retirement_abandons_one_leg_until_peer_deadline() {
    for replacement in [false, true] {
        let (watch, _) = WatchFixture::new(false).await;
        let checkpoint = watch.row().cursor;
        if replacement {
            start_run(
                &Db::open(&watch.source.config().db_path).unwrap(),
                "replacement-run",
                "manager",
                "in progress",
            );
            watch
                .source
                .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
        } else {
            let (status, _) = subscription_request(
                &watch.app,
                "POST",
                &format!("/v1/event-subscriptions/{}/unsubscribe", watch.id),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        // Exactly one lifecycle notification suffices to interrupt collect.
        until(|| watch.relay.counts.abandoned.load(Ordering::SeqCst) == 1).await;
        assert!(!watch.row().active);
        assert_eq!(watch.row().cursor, checkpoint);
        assert_eq!(watch.relay.budget.available_permits(), 0);
        notifications(&watch.source).await;
        assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(watch.relay.counts.released.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(
            kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS,
        ))
        .await;
        until(|| watch.relay.counts.released.load(Ordering::SeqCst) == 1).await;
        assert_eq!(watch.relay.budget.available_permits(), 1);
        assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(watch.row().cursor, checkpoint);
    }
}

#[tokio::test(start_paused = true)]
async fn subscription_busy_peer_pause_and_same_id_recovery_preserve_checkpoint() {
    let (watch, held) = WatchFixture::new(true).await;
    let checkpoint = watch.row().cursor.unwrap();
    notifications(&watch.source).await;
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(101),
            "https://example.test/pull/101",
        )
        .unwrap();
    let failed = watch.page().await;
    let batch = failed.pending.as_ref().unwrap();
    assert!(batch["watchError"].is_string());
    assert!(batch["machineErrors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("too many concurrent requests"));
    let peer_before =
        decode_cursor(&checkpoint)["cursorsByMachine"]["desktop-pending-peer"].clone();
    assert_eq!(
        decode_cursor(batch["cursor"].as_str().unwrap())["cursorsByMachine"]
            ["desktop-pending-peer"],
        peer_before
    );
    let paused = watch.ack(&failed).await;
    assert_eq!(paused["active"], false);
    assert_eq!(paused["cursor"], batch["cursor"]);
    notifications(&watch.source).await;
    assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 1);
    drop(held);
    // This event must be replayed from the retained checkpoint, not skipped
    // by a recovery that silently starts over at from=now.
    Db::open(&watch.peer.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-peer-child",
            Some(201),
            "https://example.test/pull/201",
        )
        .unwrap();
    let resumed = watch.register().await;
    assert_eq!(resumed["cursor"], paused["cursor"]);
    assert_eq!(resumed["active"], true);
    assert!(resumed["error"].is_null());
    let recovered = watch.page().await;
    let mut delivered = event_pairs(batch);
    delivered.extend(event_pairs(recovered.pending.as_ref().unwrap()));
    delivered.sort();
    assert_eq!(delivered, vec![
        ("pending-local-child".into(), "task.pr_created".into()),
        ("pending-peer-child".into(), "task.pr_created".into()),
    ], "an urgent peer fault may return before the local PR is observed; recovery must retain both facts");
    assert!(recovered
        .pending
        .as_ref()
        .unwrap()
        .get("watchError")
        .is_none());
    // Recovery consumes the PR leg, then rearms it during the ordinary quiet
    // window. That new silent leg survives the normal batch return and ack.
    assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(watch.relay.counts.admitted.load(Ordering::SeqCst), 2);
    assert_eq!(watch.relay.counts.released.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.budget.available_permits(), 0);
    assert_eq!(watch.relay.counts.busy.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
    watch.ack(&recovered).await;
    notifications(&watch.source).await;
    assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn ordinary_quiet_deadlines_and_notification_storms_keep_the_remote_leg() {
    let (watch, _) = WatchFixture::new(false).await;
    for pr in [401, 402] {
        Db::open(&watch.source.config().db_path)
            .unwrap()
            .update_pipeline_item_pr(
                "pending-local-child",
                Some(pr),
                &format!("https://example.test/pull/{pr}"),
            )
            .unwrap();
        notifications(&watch.source).await;
        let page = watch.page().await;
        assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
        watch.ack(&page).await;
    }
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(403),
            "https://example.test/pull/403",
        )
        .unwrap();
    notifications(&watch.source).await;
    Db::open(&watch.peer.config().db_path)
        .unwrap()
        .append_task_event(
            "pending-peer-child",
            crate::db::TaskEventKind::LifecycleFailed,
            json!({"error":"remote failure"}),
        )
        .unwrap();
    let urgent = watch.page().await;
    let pairs = event_pairs(urgent.pending.as_ref().unwrap());
    assert!(pairs.contains(&("pending-peer-child".into(), "task.lifecycle_failed".into())));
    assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
    assert_eq!(watch.relay.counts.busy.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn an_aggregate_event_observed_in_one_native_leg_survives_into_a_later_leg() {
    // A 250s quiet/max-hold window exceeds the fixed 240s native receiver, so
    // the aggregate wait must chain a second call — issuing a fresh peer long
    // poll — before the subscription's own deadline is reached. The local
    // event is only ever observed inside the first (240s) leg; if the chain
    // did not retain it across that leg's own timeout, the eventual page
    // would either come back empty or, at best, only ever ack past it.
    let (watch, _) = WatchFixture::new_with(
        false,
        WatchFixture::request_with(json!({"quietMs": 250_000, "maxHoldMs": 250_000})),
    )
    .await;
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(301),
            "https://example.test/pull/301",
        )
        .unwrap();
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    let page = watch.page().await;
    // At least one additional long poll to the peer was issued beyond the
    // first, proving a native receiver boundary was actually crossed here
    // rather than the page merely settling within a single 240s call.
    assert!(watch.relay.counts.attempts.load(Ordering::SeqCst) >= 2);
    let batch = page.pending.as_ref().unwrap();
    assert!(
        batch.get("watchError").is_none(),
        "peer errors: {}",
        batch["machineErrors"]
    );
    assert_eq!(
        event_pairs(batch),
        vec![("pending-local-child".into(), "task.pr_created".into())]
    );
    let acked = watch.ack(&page).await;
    assert_eq!(acked["cursor"], batch["cursor"]);
}

#[tokio::test(start_paused = true)]
async fn an_aggregate_later_event_extends_the_live_deadline_mid_leg_and_is_not_sealed_early() {
    // quiet (300s) < max_hold (600s): quiet, anchored to the LATEST
    // observation, actually controls the deadline. The first event's own leg
    // (240s ceiling) times out well short of the initial 300s deadline; a
    // second event lands only after that re-issued leg has been dispatched
    // (sized to the now-stale 300s point), which must extend the live
    // deadline rather than let the stale leg's own receiver seal the page.
    let (watch, _) = WatchFixture::new_with(
        false,
        WatchFixture::request_with(json!({"quietMs": 300_000, "maxHoldMs": 600_000})),
    )
    .await;
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(501),
            "https://example.test/pull/501",
        )
        .unwrap();
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    // Wait for the re-issued (second) long poll, proving leg 1's own 240s
    // receiver was crossed before the page could settle.
    until(|| watch.relay.counts.attempts.load(Ordering::SeqCst) >= 2).await;
    // Not yet sealed: still short of even the original (soon-to-be-stale)
    // 300s deadline, let alone the extended one.
    assert_ne!(watch.row().wake_state, "ready");
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .append_task_event(
            "pending-local-child",
            crate::db::TaskEventKind::TaskClosed,
            json!({}),
        )
        .unwrap();
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    let page = watch.page().await;
    let batch = page.pending.as_ref().unwrap();
    assert!(
        batch.get("watchError").is_none(),
        "peer errors: {}",
        batch["machineErrors"]
    );
    assert_eq!(
        event_pairs(batch),
        vec![
            ("pending-local-child".into(), "task.pr_created".into()),
            ("pending-local-child".into(), "task.closed".into()),
        ]
    );
    let acked = watch.ack(&page).await;
    assert_eq!(acked["cursor"], batch["cursor"]);
}

#[tokio::test(start_paused = true)]
async fn initial_discovery_fault_pins_local_tail_before_recovery() {
    let state = test_state_with_seed("subscription-discovery", "Discovery", seed_orchestration);
    state.set_desktop_routing_available(true);
    let mut requests = state.take_desktop_relay_requests().unwrap();
    let relay = tokio::spawn(async move {
        let mut first = true;
        while let Some(request) = requests.recv().await {
            match request {
                DesktopRelayRequest::ListActive { response, .. } => {
                    let result = if first {
                        first = false;
                        Err("discovery unavailable".into())
                    } else {
                        Ok(vec![])
                    };
                    let _ = response.send(result);
                }
                DesktopRelayRequest::PublishTaskSnapshot { response, .. } => {
                    let _ = response.send(Ok(()));
                }
                _ => panic!("unexpected discovery fixture request"),
            }
        }
    });
    let query = json!({"taskIds":"child-a,child-b", "from":"now",
        "includeCurrentActivity":false, "timeoutSecs":240, "limit":100});
    let started = tokio::time::Instant::now();
    let fresh_collection = || {
        Arc::new(std::sync::Mutex::new(
            super::super::super::subscription_timing::Collection::default(),
        ))
    };
    let fault = super::super::super::task_events::wait_subscription_events(
        state.clone(),
        query.clone(),
        fresh_collection(),
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::Instant::now(),
        started,
        "known fault must not await a silent leg"
    );
    assert_eq!(fault["machineErrors"].as_array().unwrap().len(), 1);
    assert_eq!(fault["events"], json!([]));
    let db = Db::open(&state.config().db_path).unwrap();
    db.append_task_event(
        "child-b",
        crate::db::TaskEventKind::AwaitingInput,
        json!({}),
    )
    .unwrap();
    let mut resumed = query;
    resumed["cursor"] = fault["cursor"].clone();
    let page = super::super::super::task_events::wait_subscription_events(
        state,
        resumed,
        fresh_collection(),
    )
    .await
    .unwrap();
    assert_eq!(
        event_pairs(&page),
        vec![("child-b".into(), "task.awaiting_input".into())]
    );
    assert_eq!(page["machineErrors"], json!([]));
    relay.abort();
    let _ = relay.await;
}
