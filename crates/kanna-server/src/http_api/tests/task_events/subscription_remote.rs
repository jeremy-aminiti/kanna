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

// Bounded scheduling with a paused Tokio clock. Advancing one millisecond
// lets peer handlers and observers settle without sleeping for 240 real seconds.
async fn until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1_000 {
        if condition() {
            return;
        }
        tokio::time::advance(Duration::from_millis(1)).await;
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
        json!({"taskId":"manager", "repoId":"repo-pending-source", "delivery":"poll"})
    }

    async fn new(exhaust_budget: bool) -> (Self, Option<tokio::sync::OwnedSemaphorePermit>) {
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
            subscription_request(&app, "POST", "/v1/event-subscriptions", Self::request()).await;
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
            json!({"acknowledgeBatchId":row.batch_id}),
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
    assert_eq!(
        event_pairs(recovered.pending.as_ref().unwrap()),
        vec![("pending-peer-child".into(), "task.pr_created".into())]
    );
    assert!(recovered
        .pending
        .as_ref()
        .unwrap()
        .get("watchError")
        .is_none());
    assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(watch.relay.counts.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.busy.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
}
