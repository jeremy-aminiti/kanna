//! Subscription worker -> aggregate registry -> relay queue -> peer HTTP wait.
//! The transport is a fixture, with the production long-poll semaphore and
//! peer handler. Receiver work deliberately survives loss of its caller, just
//! like relay.rs; a fake transport that cancels it would hide this regression.
use super::*;
use crate::http_api::{DesktopRelayRequest, HttpInvokeResponse};
use std::collections::HashMap;
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

/// Like `connect`, but for a repo-scoped aggregate with more than one
/// remote peer: `Invoke` is routed by its `desktop_id` to the matching
/// backing state, and `ListActive` reports whatever `active` currently
/// holds — mutable from the test, so a peer already discovered at bootstrap
/// (its checkpoint established) can be dropped out of discovery entirely,
/// the actual "peer absent from ListActive" shape, never a busy/503 leg.
/// `counts` is shared by every routed peer: only one remote leg is ever
/// admitted at a time in the fixtures that use this (an excluded peer is
/// never dispatched at all), so a single semaphore/counter set still
/// unambiguously reports that one healthy leg's own cadence.
fn connect_repo_peers(
    source: &Arc<AppState>,
    peers: &[Arc<AppState>],
    active: Arc<std::sync::Mutex<Vec<String>>>,
) -> RelayFixture {
    let mut requests = source.take_desktop_relay_requests().unwrap();
    source.set_desktop_routing_available(true);
    // One permit per peer: unlike `connect` (one remote peer, so a budget of
    // 1 also doubles as a busy/503 knob for tests that hold it), bootstrap
    // dispatches a zero-timeout call to every currently-listed peer at once
    // — a budget smaller than the peer count would starve one of them with
    // an artificial busy rejection on every registration, not just when a
    // test deliberately exhausts it. Nothing here exhausts this budget.
    let permits = Arc::new(crate::relay::RelayHttpInvokePermits::new(peers.len().max(1)));
    let budget = permits.for_path("/v1/task-events");
    let counts = Arc::new(Counts::default());
    let observed = counts.clone();
    let routes: HashMap<String, Arc<AppState>> = peers
        .iter()
        .map(|peer| (peer.config().desktop_id.clone(), peer.clone()))
        .collect();
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
                            let ids = active
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            let _ = response.send(Ok(ids));
                        }
                        DesktopRelayRequest::Invoke {
                            desktop_id,
                            method,
                            path,
                            body,
                            mut response,
                            ..
                        } => {
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
                            let target = routes
                                .get(&desktop_id)
                                .unwrap_or_else(|| {
                                    panic!("relay request for unrouted peer {desktop_id}")
                                })
                                .clone();
                            let observed = observed.clone();
                            receivers.spawn(async move {
                                let wait = crate::http_api::dispatch_authenticated_http_invoke(
                                    target, &method, &path, body,
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

/// One captured, not-yet-resolved long poll to the gated peer in
/// `connect_gated_peer`: the test decides exactly when and how it resolves
/// (a specific failure, a specific success, or simply not at all for as
/// long as it needs), so recovery evidence is never inferred, only ever
/// positively supplied. Holds its admission permit for as long as the test
/// holds the request, mirroring a real outstanding long poll's lifetime.
struct GatedInvoke {
    permit: tokio::sync::OwnedSemaphorePermit,
    response: tokio::sync::oneshot::Sender<Result<HttpInvokeResponse, String>>,
}

impl GatedInvoke {
    /// Resolves this leg and releases its admission permit at the same
    /// moment. Calling `.response.send(...)` directly is a *partial move*
    /// of only that field: `permit` survives it and stays alive until the
    /// enclosing `let`-bound leg goes out of scope, not when the response
    /// actually completes. Under `connect_gated_peer`'s one-permit budget
    /// that starves every later dispatch with a synthetic 503 until the
    /// binding's scope ends, so a second leg meant to reach the gate never
    /// does. Consuming `self` here and dropping `permit` before sending
    /// closes that gap.
    fn resolve(self, result: Result<HttpInvokeResponse, String>) {
        let GatedInvoke { permit, response } = self;
        drop(permit);
        let _ = response.send(result);
    }
}

/// Like `connect`, but every *long* (post-bootstrap) `Invoke` to `peer` is
/// captured on the returned channel instead of being dispatched: the test
/// resolves each one explicitly. The zero-timeout bootstrap call still
/// dispatches for real, so the subscription starts with a genuine
/// established checkpoint. `ListActive` always reports the peer present —
/// this fixture is about a peer's own leg outcome, not discovery.
fn connect_gated_peer(
    source: &Arc<AppState>,
    peer: Arc<AppState>,
) -> (RelayFixture, tokio::sync::mpsc::UnboundedReceiver<GatedInvoke>) {
    let mut requests = source.take_desktop_relay_requests().unwrap();
    source.set_desktop_routing_available(true);
    let permits = Arc::new(crate::relay::RelayHttpInvokePermits::new(1));
    let budget = permits.for_path("/v1/task-events");
    let counts = Arc::new(Counts::default());
    let observed = counts.clone();
    let (gate_tx, gate_rx) = tokio::sync::mpsc::unbounded_channel::<GatedInvoke>();
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
                        DesktopRelayRequest::Invoke { method, path, body, response, .. } => {
                            assert!(path.starts_with("/v1/task-events?"));
                            let long = !path.contains("timeoutSecs=0&");
                            if !long {
                                // Bootstrap: dispatch for real so the
                                // subscription starts with a genuine
                                // checkpoint, not a synthetic one.
                                let peer = peer.clone();
                                receivers.spawn(async move {
                                    let result =
                                        crate::http_api::dispatch_authenticated_http_invoke(
                                            peer, &method, &path, body,
                                        )
                                        .await;
                                    let _ = response.send(Ok(result));
                                });
                                continue;
                            }
                            observed.attempts.fetch_add(1, Ordering::SeqCst);
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
                            observed.admitted.fetch_add(1, Ordering::SeqCst);
                            let _ = gate_tx.send(GatedInvoke { permit, response });
                        }
                        _ => panic!("unexpected subscription relay operation"),
                    }
                }
            }
        }
    });
    (
        RelayFixture {
            task,
            budget,
            counts,
        },
        gate_rx,
    )
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

/// Bounds a `connect_gated_peer` dispatch wait the same way `until` bounds a
/// state poll, so a fixture regression (the permit bug this replaced, or a
/// production change that stops dispatching a leg the test expects) fails
/// this test instead of hanging the whole suite. Nothing here is actually
/// time-based in the healthy case — the channel resolves the instant the
/// production code dispatches — the budget only matters when it should fire.
async fn recv_gated(
    gate: &mut tokio::sync::mpsc::UnboundedReceiver<GatedInvoke>,
    what: &str,
) -> GatedInvoke {
    tokio::select! {
        leg = gate.recv() => {
            leg.unwrap_or_else(|| panic!("{what}: relay fixture closed before dispatching"))
        }
        _ = async {
            for _ in 0..400 {
                tokio::time::advance(Duration::from_secs(1)).await;
                tokio::task::yield_now().await;
            }
        } => panic!("{what}: expected long poll was never dispatched"),
    }
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

    /// Like `new`, but the peer's own embedded native cursor is corrupted
    /// *before* the worker ever spawns, so its very first long-poll already
    /// carries the poisoned value — not after a valid one was already
    /// admitted. A live peer leg is never cancelled, so mutating the row out
    /// from under one that already holds the fixture's one relay permit
    /// only starves a later attempt with a busy rejection instead of
    /// reaching the peer's cursor validation at all (see
    /// `subscription_retirement_abandons_one_leg_until_peer_deadline`: an
    /// abandoned leg keeps its permit until its own deadline).
    async fn new_with_poisoned_peer_cursor() -> Self {
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
        let id = initial["id"].as_str().unwrap().to_string();
        let mut poisoned = decode_cursor(initial["cursor"].as_str().unwrap());
        // Same poisoned literal already proven (in task_events.rs) to decode
        // locally as a pass-through legacy cursor and be rejected by the
        // peer itself, never by this machine's own decode.
        poisoned["cursorsByMachine"]["desktop-pending-peer"] = json!("ksh1.deadbeef");
        let mut row = db.event_subscription(&id).unwrap().unwrap();
        row.cursor = Some(encode_cursor(&poisoned));
        assert!(db.save_event_subscription(&mut row).unwrap());
        let service = tokio::spawn(super::super::super::event_subscriptions::run(
            source.clone(),
        ));
        let fixture = Self {
            source,
            peer,
            relay,
            app,
            id,
            service,
        };
        until(|| fixture.relay.counts.attempts.load(Ordering::SeqCst) == 1).await;
        fixture
    }

    /// Three machines: `source` (local), `peer` (a healthy sibling, reusing
    /// every existing `WatchFixture` method), and a third, separately
    /// returned peer that is *already discovered* — its checkpoint
    /// established during bootstrap, like the other two — and only then
    /// excluded from `ListActive` before the worker's first real
    /// collection cycle. That is the actual "MBP dropped WiFi" shape: the
    /// peer is never spawned at all once excluded (the pre-spawn
    /// `machineErrors` path in `wait_aggregate_task_events`), not admitted
    /// and then rejected. Because it is never dispatched, the healthy
    /// sibling's own relay counts (`fixture.relay.counts`) reflect that
    /// sibling's cadence alone — unaffected by the excluded peer's absence,
    /// with no extra admission or abandonment to account for.
    async fn new_with_healthy_sibling_and_excluded_peer(
    ) -> (Self, Arc<AppState>, Arc<std::sync::Mutex<Vec<String>>>) {
        let (source, sibling) = aggregate_pending_leg_states();
        let missing = test_state_with_seed("desktop-pending-missing", "Pending Missing", |db| {
            db.insert_test_repo("repo-pending-missing", "Pending Missing Repo")
                .expect("insert missing repo");
            db.insert_test_pipeline_item(
                "pending-missing-child",
                "repo-pending-missing",
                "missing child",
                Some("Missing Child"),
                "in progress",
                "2026-08-16 00:00:00",
            )
            .expect("insert missing task");
        });
        for (state, repo) in [
            (&source, "repo-pending-source"),
            (&sibling, "repo-pending-peer"),
            (&missing, "repo-pending-missing"),
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
        let sibling_id = sibling.config().desktop_id.clone();
        let missing_id = missing.config().desktop_id.clone();
        let active = Arc::new(std::sync::Mutex::new(vec![
            sibling_id.clone(),
            missing_id.clone(),
        ]));
        let relay = connect_repo_peers(
            &source,
            &[sibling.clone(), missing.clone()],
            active.clone(),
        );
        let app = router(source.clone());
        let (status, initial) =
            subscription_request(&app, "POST", "/v1/event-subscriptions", Self::request()).await;
        assert_eq!(status, StatusCode::OK, "{initial}");
        let id = initial["id"].as_str().unwrap().to_string();
        let decoded = decode_cursor(initial["cursor"].as_str().unwrap());
        for machine_id in [&sibling_id, &missing_id] {
            assert!(
                decoded["cursorsByMachine"][machine_id.as_str()]
                    .as_str()
                    .unwrap()
                    .starts_with("ke1."),
                "{decoded}"
            );
        }
        // Exclude the peer before the worker's first real cycle: an
        // already-admitted leg cannot simply be revoked (see
        // `subscription_retirement_abandons_one_leg_until_peer_deadline`),
        // so the exclusion must land before any leg to this peer is ever
        // spawned, not after.
        active
            .lock()
            .unwrap()
            .retain(|entry| *entry != missing_id);
        let service = tokio::spawn(super::super::super::event_subscriptions::run(
            source.clone(),
        ));
        let fixture = Self {
            source,
            peer: sibling,
            relay,
            app,
            id,
            service,
        };
        until(|| fixture.relay.counts.attempts.load(Ordering::SeqCst) == 1).await;
        (fixture, missing, active)
    }

    /// Simulates a server restart for the subscribing machine: the worker
    /// and relay connection are torn down (via `Drop`) and a fresh
    /// `AppState` is built from the same persisted DB path. No in-memory
    /// state survives the boundary — not the aggregate-wait registry, not
    /// the admission clock, not the relay connection — only what is
    /// durable in the DB. `peers` is reconnected through the same
    /// multi-peer relay as `new_with_healthy_sibling_and_excluded_peer`,
    /// carrying `active` through unchanged, so an excluded peer stays
    /// excluded (and a since-recovered one stays recovered) exactly as it
    /// was before the restart.
    async fn restart(
        self,
        peers: &[Arc<AppState>],
        active: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        let config = self.source.config().clone();
        let peer = self.peer.clone();
        let id = self.id.clone();
        drop(self);
        let source = Arc::new(AppState::new(config));
        let relay = connect_repo_peers(&source, peers, active);
        let app = router(source.clone());
        let service = tokio::spawn(super::super::super::event_subscriptions::run(
            source.clone(),
        ));
        let fixture = Self {
            source,
            peer,
            relay,
            app,
            id,
            service,
        };
        until(|| fixture.relay.counts.attempts.load(Ordering::SeqCst) >= 1).await;
        fixture
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

fn encode_cursor(payload: &Value) -> String {
    format!(
        "ks1.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

fn decode_cursor(cursor: &str) -> Value {
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor.strip_prefix("ks1.").unwrap())
            .unwrap(),
    )
    .unwrap()
}

/// A non-destructive read through the actual HTTP compact contract — no
/// `acknowledgeBatchId`, `diagnostic` omitted — exactly what a manager
/// consuming the response shape (not the DB struct) sees by default.
/// Asserts the compact/durable-cursor-omission contract that must hold on
/// every such response, so every call site gets that coverage for free
/// rather than needing to repeat it.
async fn compact_read(app: &Router, id: &str) -> Value {
    let (status, body) = subscription_request(
        app,
        "POST",
        &format!("/v1/event-subscriptions/{id}/read"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("cursor").is_none(),
        "a compact response must omit the durable cursor: {body}"
    );
    if let Some(query) = body.get("query") {
        assert!(
            query.get("cursor").is_none(),
            "a compact response's watched scope must omit any cursor-shaped key: {body}"
        );
    }
    if let Some(pending) = body.get("pending").filter(|p| !p.is_null()) {
        assert!(
            pending.get("cursor").is_none(),
            "a compact response's pending batch must omit the durable cursor: {body}"
        );
    }
    body
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

/// A busy/unreachable remote peer degrades only that one leg. The local
/// leg's own delivery and acknowledgement continue uninterrupted, the peer's
/// checkpoint survives untouched across several acks, an unchanged fault
/// does not re-wake the subscriber on every notification, and the peer's
/// return replays its backlog from the preserved checkpoint with no
/// unsubscribe/resubscribe dance — the subscription was never paused.
#[tokio::test(start_paused = true)]
async fn subscription_remote_outage_isolates_to_that_leg_and_recovers() {
    let (watch, held) = WatchFixture::new(true).await;
    let checkpoint = watch.row().cursor.unwrap();
    let peer_checkpoint =
        decode_cursor(&checkpoint)["cursorsByMachine"]["desktop-pending-peer"].clone();
    notifications(&watch.source).await;

    // First local event while the peer is down: still delivered normally,
    // annotated with the peer's fault, never as a whole-subscription pause.
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(101),
            "https://example.test/pull/101",
        )
        .unwrap();
    let first = watch.page().await;
    let batch = first.pending.as_ref().unwrap();
    assert!(
        batch.get("watchError").is_none(),
        "a remote-only fault must not synthesize a whole-subscription watchError: {batch}"
    );
    assert!(batch["machineErrors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("too many concurrent requests"));
    assert_eq!(
        event_pairs(batch),
        vec![("pending-local-child".into(), "task.pr_created".into())]
    );
    assert_eq!(
        decode_cursor(batch["cursor"].as_str().unwrap())["cursorsByMachine"]
            ["desktop-pending-peer"],
        peer_checkpoint,
        "the down peer's own checkpoint must not move while it cannot be observed"
    );
    // The compact HTTP contract a manager actually consumes: while this
    // batch is still pending, its own machineErrors reports the outage —
    // not just the diagnostic-mode row read above.
    let compact_pending = compact_read(&watch.app, &watch.id).await;
    assert!(!compact_pending["pending"].is_null());
    assert!(compact_pending["pending"]["machineErrors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("too many concurrent requests"));
    let acked = watch.ack(&first).await;
    assert_eq!(
        acked["active"],
        json!(true),
        "a peer outage must not pause the whole subscription"
    );
    assert!(acked["error"].is_null());
    // Compact staleMachines must be visible immediately after ack — with
    // pending now null — not only through diagnostic:true.
    let compact_after_first_ack = compact_read(&watch.app, &watch.id).await;
    assert!(compact_after_first_ack["pending"].is_null());
    assert!(compact_after_first_ack["staleMachines"]["desktop-pending-peer"]
        .as_str()
        .unwrap()
        .contains("too many concurrent requests"));

    // A pure notification storm with the peer still down and nothing new
    // locally must not manufacture a fresh wake from the already-reported,
    // unchanged fault. Synchronize on the next call's own peer dispatch
    // (rather than a fixed number of scheduler turns) before asserting
    // nothing woke the subscriber from it.
    notifications(&watch.source).await;
    until(|| watch.relay.counts.attempts.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(
        watch.row().wake_state,
        "idle",
        "an unchanged, already-reported peer fault must not re-wake the subscriber"
    );
    assert!(watch.row().active);
    assert_eq!(
        decode_cursor(watch.row().cursor.as_ref().unwrap())["cursorsByMachine"]
            ["desktop-pending-peer"],
        peer_checkpoint
    );
    // Still visible through the compact contract with nothing new pending.
    let compact_idle = compact_read(&watch.app, &watch.id).await;
    assert!(compact_idle["pending"].is_null());
    assert!(compact_idle["staleMachines"]["desktop-pending-peer"].is_string());

    // A second local event, still with the peer down: the checkpoint keeps
    // surviving across repeated acknowledgements, not just the first one.
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(102),
            "https://example.test/pull/102",
        )
        .unwrap();
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    let second = watch.page().await;
    let second_batch = second.pending.as_ref().unwrap();
    assert!(second_batch.get("watchError").is_none());
    assert_eq!(
        event_pairs(second_batch),
        vec![("pending-local-child".into(), "task.pr_created".into())]
    );
    assert_eq!(
        decode_cursor(second_batch["cursor"].as_str().unwrap())["cursorsByMachine"]
            ["desktop-pending-peer"],
        peer_checkpoint
    );
    let second_acked = watch.ack(&second).await;
    assert_eq!(second_acked["active"], json!(true));
    // Stale coverage survives across a *second* real ack too, still through
    // the compact contract, not just the first one.
    let compact_after_second_ack = compact_read(&watch.app, &watch.id).await;
    assert!(compact_after_second_ack["pending"].is_null());
    assert!(compact_after_second_ack["staleMachines"]["desktop-pending-peer"].is_string());
    // Let the next call's own peer dispatch begin (and fail, held is still
    // exhausted) before jumping the clock, so its local-only deadline is
    // anchored to the current time rather than to whatever the worker has
    // not yet gotten around to starting.
    until(|| watch.relay.counts.attempts.load(Ordering::SeqCst) >= 3).await;

    drop(held);
    // This event must be replayed from the retained checkpoint, not skipped
    // by a recovery that silently starts over at from=now — and no
    // unsubscribe/resubscribe is needed, because the subscription stayed
    // active through the whole outage. The leg already in flight marked the
    // peer failed for its own call and will not retry it mid-call; advance
    // past its own local-only deadline so the worker starts a fresh call
    // that gives the now-healthy peer a new attempt.
    Db::open(&watch.peer.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-peer-child",
            Some(201),
            "https://example.test/pull/201",
        )
        .unwrap();
    tokio::time::advance(Duration::from_secs(
        kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS,
    ))
    .await;
    let recovered = watch.page().await;
    let recovered_batch = recovered.pending.as_ref().unwrap();
    assert!(recovered_batch.get("watchError").is_none());
    assert_eq!(
        recovered_batch["machineErrors"],
        json!([]),
        "the peer's return must clear its fault annotation, not just deliver its backlog"
    );
    assert_eq!(
        event_pairs(recovered_batch),
        vec![("pending-peer-child".into(), "task.pr_created".into())]
    );
    let recovered_acked = watch.ack(&recovered).await;
    assert_eq!(recovered_acked["active"], json!(true));
    assert!(recovered_acked["error"].is_null());
    // Confirmed recovery clears staleMachines through the compact contract
    // too, not only the durable row.
    let compact_recovered = compact_read(&watch.app, &watch.id).await;
    assert!(compact_recovered["pending"].is_null());
    assert_eq!(compact_recovered["staleMachines"], json!({}));
}

/// A remote peer rejecting its own embedded cursor (a poisoned/expired
/// checkpoint) is not peer unavailability: `apply_aggregate_completion`
/// classifies it as `AggregateMachineWaitError::CursorRejected` and returns a
/// hard `Err` *before* it ever reaches `machineErrors`, so none of the
/// outage-isolation leniency in `accept_page` applies to it — by
/// construction, not by an extra check. It must keep behaving exactly like
/// the pre-existing local-cursor-corruption case: a durable pause requiring
/// reconciliation, never silently reset or retried.
#[tokio::test(start_paused = true)]
async fn subscription_remote_cursor_rejection_remains_a_hard_pause_distinct_from_outage() {
    let watch = WatchFixture::new_with_poisoned_peer_cursor().await;
    let poisoned_cursor = watch.row().cursor.unwrap();

    let failed = watch.page().await;
    let batch = failed.pending.as_ref().unwrap();
    let watch_error = batch["watchError"]
        .as_str()
        .expect("a rejected remote cursor must surface as an explicit watchError, not a per-machine annotation");
    assert!(
        watch_error.contains("rejected its embedded task-event cursor"),
        "a cursor rejection must be reported distinctly from peer unavailability: {watch_error}"
    );
    assert!(!watch_error.contains("too many concurrent requests"));
    assert!(
        batch["machineErrors"]
            .as_array()
            .map(|errors| errors.is_empty())
            .unwrap_or(true),
        "a cursor rejection is a hard failure of the whole wait, never a tolerated per-machine fault: {batch}"
    );
    assert_eq!(
        batch["cursor"],
        json!(poisoned_cursor),
        "an error cannot silently reset or advance the poisoned checkpoint"
    );

    let paused = watch.ack(&failed).await;
    assert_eq!(
        paused["active"],
        json!(false),
        "a remote cursor rejection must remain a durable, actionable pause, unlike remote unavailability"
    );
    assert_eq!(paused["cursor"], json!(poisoned_cursor));

    // Not silently retried: once paused the worker stops entirely, so no
    // amount of notifications or elapsed time produces another peer attempt.
    let attempts_at_pause = watch.relay.counts.attempts.load(Ordering::SeqCst);
    notifications(&watch.source).await;
    tokio::time::advance(Duration::from_secs(
        kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS,
    ))
    .await;
    notifications(&watch.source).await;
    assert_eq!(
        watch.relay.counts.attempts.load(Ordering::SeqCst),
        attempts_at_pause,
        "a paused, cursor-rejected subscription must not keep retrying the peer"
    );

    // A plain retry (not an explicit unsubscribe) preserves the poisoned
    // position for reconciliation rather than silently resetting it to now.
    let resumed = watch.register().await;
    assert_eq!(resumed["cursor"], json!(poisoned_cursor));
    assert_eq!(resumed["active"], json!(true));
}

/// A repo-scoped subscription with a healthy sibling peer AND a peer that
/// was already discovered — its checkpoint established at bootstrap, like
/// the sibling's — but then disappears from `ListActive` before the
/// worker's first real collection cycle. This is the actual "MBP dropped
/// WiFi" shape (relay stops reporting it at all), not a busy/503 leg, and
/// it exercises the pre-spawn `machineErrors` path this fix added: the
/// excluded peer is never even attempted, not admitted then rejected.
/// Proves the healthy sibling's own retained wait is entirely unaffected —
/// no extra admission, no abandonment — repeated local delivery keeps
/// working through several real ACKs, the missing peer's exact checkpoint
/// survives untouched and in scope, and its eventual return replays the
/// intervening event from that preserved checkpoint.
#[tokio::test(start_paused = true)]
async fn subscription_remote_outage_with_a_healthy_sibling_isolates_and_recovers() {
    let (watch, missing, active) =
        WatchFixture::new_with_healthy_sibling_and_excluded_peer().await;
    let missing_id = missing.config().desktop_id.clone();
    let checkpoint = watch.row().cursor.unwrap();
    let missing_checkpoint =
        decode_cursor(&checkpoint)["cursorsByMachine"][missing_id.as_str()].clone();

    // Round 1: a local event is delivered normally; the missing peer is
    // reported (never even attempted) without pausing anything, and its
    // checkpoint in the delivered batch is untouched.
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(101),
            "https://example.test/pull/101",
        )
        .unwrap();
    let first = watch.page().await;
    let batch = first.pending.as_ref().unwrap();
    assert!(batch.get("watchError").is_none(), "{batch}");
    assert_eq!(batch["machineErrors"].as_array().unwrap().len(), 1);
    assert_eq!(batch["machineErrors"][0]["machineId"], json!(missing_id));
    assert_eq!(
        decode_cursor(batch["cursor"].as_str().unwrap())["cursorsByMachine"][missing_id.as_str()],
        missing_checkpoint
    );
    assert_eq!(
        event_pairs(batch),
        vec![("pending-local-child".into(), "task.pr_created".into())]
    );
    watch.ack(&first).await;

    // Round 2: another local event, still with the peer missing — proves
    // repeated delivery, not a one-shot. The sibling's own leg (admitted
    // once for round 1) is simply resumed via the aggregate-wait registry,
    // exactly as it already is for a plain two-machine subscription (see
    // subscription_notifications_and_ack_retain_one_remote_wait); a
    // missing third peer changes nothing about that.
    Db::open(&watch.source.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-local-child",
            Some(102),
            "https://example.test/pull/102",
        )
        .unwrap();
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    let second = watch.page().await;
    let second_batch = second.pending.as_ref().unwrap();
    assert!(second_batch.get("watchError").is_none(), "{second_batch}");
    assert_eq!(second_batch["machineErrors"][0]["machineId"], json!(missing_id));
    assert_eq!(
        decode_cursor(second_batch["cursor"].as_str().unwrap())["cursorsByMachine"]
            [missing_id.as_str()],
        missing_checkpoint
    );
    watch.ack(&second).await;
    // The compact contract a manager actually consumes: stale coverage for
    // the excluded peer survives across both real acks, with nothing
    // pending, not only through the diagnostic-mode row.
    let compact_after_second_ack = compact_read(&watch.app, &watch.id).await;
    assert!(compact_after_second_ack["pending"].is_null());
    assert!(compact_after_second_ack["staleMachines"][missing_id.as_str()].is_string());

    // The missing peer was never probed at all: excluded pre-spawn, not
    // admitted then rejected. The healthy sibling's own leg was admitted
    // exactly once and never abandoned across both rounds — unaffected by
    // the other peer's absence.
    assert_eq!(watch.relay.counts.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(watch.relay.counts.abandoned.load(Ordering::SeqCst), 0);
    assert_eq!(watch.relay.counts.busy.load(Ordering::SeqCst), 0);

    // Recovery: an event lands on the missing peer while it is still
    // excluded, then it returns to ListActive.
    Db::open(&missing.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-missing-child",
            Some(201),
            "https://example.test/pull/201",
        )
        .unwrap();
    active.lock().unwrap().push(missing_id.clone());
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    let recovered = watch.page().await;
    let recovered_batch = recovered.pending.as_ref().unwrap();
    assert!(recovered_batch.get("watchError").is_none(), "{recovered_batch}");
    assert_eq!(recovered_batch["machineErrors"], json!([]));
    assert_eq!(
        event_pairs(recovered_batch),
        vec![("pending-missing-child".into(), "task.pr_created".into())]
    );
    // Replayed from the preserved checkpoint, not reset to "now".
    assert_ne!(
        decode_cursor(recovered_batch["cursor"].as_str().unwrap())["cursorsByMachine"]
            [missing_id.as_str()],
        missing_checkpoint,
        "recovery must advance past the preserved checkpoint, not just deliver from a fresh one"
    );
    watch.ack(&recovered).await;
    // Confirmed recovery clears staleMachines through the compact contract.
    let compact_recovered = compact_read(&watch.app, &watch.id).await;
    assert!(compact_recovered["pending"].is_null());
    assert_eq!(compact_recovered["staleMachines"], json!({}));
}

/// A production restart mid-outage: the subscribing machine's own server
/// process is torn down and rebuilt from the same persisted DB — a fresh
/// `AppState`, so no in-memory aggregate-wait registry, admission clock or
/// relay connection survives the boundary — while a remote peer stays
/// excluded from `ListActive` throughout. Proves the peer's exact native
/// checkpoint and its recorded stale coverage survive several real ACKs
/// and the restart itself; that several further, genuinely quiet
/// collection cycles post-restart (whose `machineErrors` text keeps
/// changing call to call, since this machine's own routing never itself
/// goes unavailable — see `AppState::desktop_routing_unreachable_error`)
/// manufacture no new pending batch/`batchId` once the fault was already
/// acknowledged; and that the peer's eventual return still replays its
/// backlog from that preserved checkpoint. The
/// `event_subscriptions::outage_isolation_tests` unit tests remain as
/// narrower, non-integration coverage of the same dedup logic in
/// isolation — this is the production worker/registry/restart seam they
/// cannot reach.
#[tokio::test(start_paused = true)]
async fn subscription_remote_outage_survives_a_server_restart_and_recovers() {
    let (watch, missing, active) =
        WatchFixture::new_with_healthy_sibling_and_excluded_peer().await;
    let missing_id = missing.config().desktop_id.clone();
    let sibling = watch.peer.clone();
    let original_checkpoint = decode_cursor(&watch.row().cursor.unwrap())["cursorsByMachine"]
        [missing_id.as_str()]
        .clone();

    // Several real local events and ACKs while the peer stays excluded.
    for pr in [301, 302, 303] {
        Db::open(&watch.source.config().db_path)
            .unwrap()
            .update_pipeline_item_pr(
                "pending-local-child",
                Some(pr),
                &format!("https://example.test/pull/{pr}"),
            )
            .unwrap();
        watch
            .source
            .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
        let page = watch.page().await;
        let batch = page.pending.as_ref().unwrap();
        assert!(batch.get("watchError").is_none(), "{batch}");
        assert_eq!(
            decode_cursor(batch["cursor"].as_str().unwrap())["cursorsByMachine"]
                [missing_id.as_str()],
            original_checkpoint,
            "the excluded peer's checkpoint must not move across acked rounds"
        );
        watch.ack(&page).await;
    }
    assert!(
        watch.row().stale_machines.contains_key(&missing_id),
        "the ongoing fault must be recorded before restart"
    );
    // The same coverage through the actual compact HTTP contract, after
    // several real acks, not only the durable row.
    let compact_before_restart = compact_read(&watch.app, &watch.id).await;
    assert!(compact_before_restart["pending"].is_null());
    assert!(compact_before_restart["staleMachines"][missing_id.as_str()].is_string());

    // Restart: tear down the worker and relay, rebuild the subscribing
    // machine's AppState fresh from the same persisted DB path. The peer
    // stays excluded throughout, carried through unchanged via `active`.
    let watch = watch
        .restart(&[sibling.clone(), missing.clone()], active.clone())
        .await;

    let restarted_row = watch.row();
    assert!(restarted_row.active);
    assert_eq!(
        decode_cursor(restarted_row.cursor.as_ref().unwrap())["cursorsByMachine"]
            [missing_id.as_str()],
        original_checkpoint,
        "a restarted server must not silently reset or advance the excluded peer's checkpoint"
    );
    assert!(
        restarted_row.stale_machines.contains_key(&missing_id),
        "stale coverage must be readable immediately after restart, from the durable row alone"
    );
    // The fresh, post-restart AppState's own HTTP handler must serve the
    // same coverage through the compact contract too — a manager reads the
    // response shape, not the DB struct, and this is a genuinely new
    // process with no in-memory carryover to lean on.
    let compact_after_restart = compact_read(&watch.app, &watch.id).await;
    assert!(compact_after_restart["pending"].is_null());
    assert!(compact_after_restart["staleMachines"][missing_id.as_str()].is_string());

    // Several full, genuinely quiet collection cycles post-restart: the
    // peer stays excluded (its error text changes call to call — this
    // machine's own routing is never itself marked unavailable) and
    // nothing new is observed locally either. None of that may manufacture
    // a fresh pending batch once the fault is already acknowledged.
    let batch_id_before_idle = watch.row().batch_id;
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(kanna_tool_catalog::MAX_WAIT_TIMEOUT_SECS)).await;
        notifications(&watch.source).await;
        assert_eq!(
            watch.row().batch_id, batch_id_before_idle,
            "a still-down, already-reported peer must not manufacture a new pending batch"
        );
        assert_eq!(watch.row().wake_state, "idle");
    }

    // Recovery: an event lands on the peer while still excluded, then it
    // returns.
    Db::open(&missing.config().db_path)
        .unwrap()
        .update_pipeline_item_pr(
            "pending-missing-child",
            Some(401),
            "https://example.test/pull/401",
        )
        .unwrap();
    active.lock().unwrap().push(missing_id.clone());
    watch
        .source
        .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
    let recovered = watch.page().await;
    let recovered_batch = recovered.pending.as_ref().unwrap();
    assert!(recovered_batch.get("watchError").is_none(), "{recovered_batch}");
    assert_eq!(recovered_batch["machineErrors"], json!([]));
    assert_eq!(
        event_pairs(recovered_batch),
        vec![("pending-missing-child".into(), "task.pr_created".into())]
    );
    assert_ne!(
        decode_cursor(recovered_batch["cursor"].as_str().unwrap())["cursorsByMachine"]
            [missing_id.as_str()],
        original_checkpoint,
        "recovery must replay from the preserved checkpoint, advancing past it"
    );
    watch.ack(&recovered).await;
    // Confirmed recovery clears staleMachines through the compact contract,
    // on the same post-restart AppState.
    let compact_recovered = compact_read(&watch.app, &watch.id).await;
    assert!(compact_recovered["pending"].is_null());
    assert_eq!(compact_recovered["staleMachines"], json!({}));
}

fn native_cursor_of(wrapped: &Value) -> String {
    let wrapped = wrapped.as_str().expect("wrapped cursor must be a string");
    let encoded = wrapped
        .strip_prefix("ke1.")
        .expect("expected a machine cursor wrapped in the ke1. envelope");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .expect("valid base64");
    String::from_utf8(bytes).expect("valid utf8 native cursor")
}

/// A minimal fixture for `connect_gated_peer`: unlike `WatchFixture`, it
/// never auto-dispatches the peer's long polls, so the test controls each
/// one's outcome explicitly.
struct GatedPeerFixture {
    source: Arc<AppState>,
    relay: RelayFixture,
    app: Router,
    id: String,
    service: tokio::task::JoinHandle<()>,
}

impl Drop for GatedPeerFixture {
    fn drop(&mut self) {
        self.service.abort();
    }
}

impl GatedPeerFixture {
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
            json!({"acknowledgeBatchId": row.batch_id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }
}

/// A peer's own retained leg completing is the *only* thing that may clear
/// its recorded stale coverage — never a page that merely happens not to
/// mention it, which is exactly what a native call sealed on this machine's
/// own urgent/full/quiet criteria produces while that peer's leg is still
/// pending in the registry. Deterministically drives that exact sequence
/// through `connect_gated_peer`: establish and ACK a fault, hold the next
/// leg genuinely pending (never resolving it, positive or negative) while
/// local batches/ACKs continue, release it as another failure, and only
/// then release a positive (if empty) success — proving stale coverage
/// survives the pending window and the second failure untouched, no
/// fault-only batch is minted while nothing changed, the checkpoint never
/// moves, no extra admission or cancellation touches the retained request,
/// and confirmed recovery wakes exactly once.
#[tokio::test(start_paused = true)]
async fn subscription_remote_stale_coverage_survives_a_pending_leg_until_positive_recovery() {
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
    let peer_id = peer.config().desktop_id.clone();
    let (relay, mut gate) = connect_gated_peer(&source, peer);
    let app = router(source.clone());
    let (status, initial) = subscription_request(
        &app,
        "POST",
        "/v1/event-subscriptions",
        WatchFixture::request(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    let checkpoint_wrapped =
        decode_cursor(initial["cursor"].as_str().unwrap())["cursorsByMachine"][peer_id.as_str()]
            .clone();
    let checkpoint_native = native_cursor_of(&checkpoint_wrapped);
    let fixture = GatedPeerFixture {
        source: source.clone(),
        relay,
        app,
        id: initial["id"].as_str().unwrap().to_string(),
        service: tokio::spawn(super::super::super::event_subscriptions::run(source.clone())),
    };

    // Establish and ACK the peer's first fault.
    let first_leg = recv_gated(&mut gate, "first long poll dispatched").await;
    first_leg.resolve(Ok(HttpInvokeResponse {
        status: 502,
        body: None,
        error: Some("peer connection reset".into()),
    }));
    let first = fixture.page().await;
    let batch = first.pending.as_ref().unwrap();
    assert!(batch.get("watchError").is_none(), "{batch}");
    assert_eq!(batch["machineErrors"][0]["machineId"], json!(peer_id));
    fixture.ack(&first).await;
    assert_eq!(
        fixture.row().stale_machines.get(&peer_id).map(String::as_str),
        Some("peer connection reset")
    );
    assert_eq!(
        decode_cursor(fixture.row().cursor.as_ref().unwrap())["cursorsByMachine"]
            [peer_id.as_str()],
        checkpoint_wrapped
    );

    // The retained-wait registry resumes with a fresh peer leg (the prior
    // one already completed with a failure); hold this one genuinely
    // pending — resolved neither way — while local batches and ACKs
    // continue uninterrupted.
    let second_leg = recv_gated(&mut gate, "second long poll dispatched").await;
    for pr in [101, 102] {
        Db::open(&fixture.source.config().db_path)
            .unwrap()
            .update_pipeline_item_pr(
                "pending-local-child",
                Some(pr),
                &format!("https://example.test/pull/{pr}"),
            )
            .unwrap();
        fixture
            .source
            .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
        let page = fixture.page().await;
        let batch = page.pending.as_ref().unwrap();
        assert!(batch.get("watchError").is_none(), "{batch}");
        assert_eq!(
            batch["machineErrors"].as_array().unwrap().len(),
            0,
            "a still-pending leg must not be reported as errored: {batch}"
        );
        fixture.ack(&page).await;
        assert_eq!(
            fixture.row().stale_machines.get(&peer_id).map(String::as_str),
            Some("peer connection reset"),
            "stale coverage must survive while the peer's own leg has not positively resolved"
        );
        assert_eq!(
            decode_cursor(fixture.row().cursor.as_ref().unwrap())["cursorsByMachine"]
                [peer_id.as_str()],
            checkpoint_wrapped
        );
    }
    assert_eq!(
        fixture.relay.counts.attempts.load(Ordering::SeqCst),
        2,
        "the still-pending leg must not have been re-admitted while held"
    );

    // Release the held leg with another failure. The set of stale machines
    // has not changed, so this must not mint a new fault-only batch either.
    let batch_id_before = fixture.row().batch_id;
    second_leg.resolve(Ok(HttpInvokeResponse {
        status: 502,
        body: None,
        error: Some("peer connection reset again".into()),
    }));
    // The next call's own peer dispatch is the synchronization signal that
    // this one settled — nothing local drives it, so it needs its own
    // native leg's full, un-rushed conclusion.
    until(|| fixture.relay.counts.attempts.load(Ordering::SeqCst) >= 3).await;
    assert_eq!(
        fixture.row().batch_id, batch_id_before,
        "an unchanged (still-down) peer must not manufacture a new pending batch just because its error text changed"
    );
    assert_eq!(
        fixture.row().stale_machines.get(&peer_id).map(String::as_str),
        Some("peer connection reset again"),
        "the stored reason still updates even though no wake was warranted"
    );
    assert_eq!(
        decode_cursor(fixture.row().cursor.as_ref().unwrap())["cursorsByMachine"]
            [peer_id.as_str()],
        checkpoint_wrapped
    );

    // This leg's retained wait resumed a third time; release it with a
    // genuine, positive success — an empty page, checkpoint unchanged. This
    // is the only thing that may clear stale coverage.
    let third_leg = recv_gated(&mut gate, "third long poll dispatched").await;
    third_leg.resolve(Ok(HttpInvokeResponse {
        status: 200,
        body: Some(json!({
            "waitOutcome": "timeout",
            "cursor": checkpoint_native,
            "events": [],
            "hasMore": false,
        })),
        error: None,
    }));
    let recovered = fixture.page().await;
    let recovered_batch = recovered.pending.as_ref().unwrap();
    assert!(recovered_batch.get("watchError").is_none(), "{recovered_batch}");
    assert_eq!(recovered_batch["machineErrors"], json!([]));
    assert_eq!(event_pairs(recovered_batch), Vec::<(String, String)>::new());
    assert!(
        fixture.row().stale_machines.is_empty(),
        "a positive successful observation, even an empty one, must clear stale coverage"
    );
    fixture.ack(&recovered).await;
    // The third leg's own success confirmed the peer but, by itself, did not
    // complete this native call's batch: it carried no events, and a
    // subscription's collector (`subscription_timing::Collection::ready`)
    // requires at least one observed event before it will seal a batch at
    // all, urgent or not. So the production collector legitimately re-arms
    // the peer's just-completed leg once more before the call finishes
    // collecting (task_events.rs's re-arm block) — one normal extra
    // admission, not runaway churn. Bound-wait for exactly that leg and
    // account for it rather than forbidding it or changing production
    // re-arming to satisfy this fixture; leaving it unresolved and letting
    // the fixture teardown reclaim it is the correct end for a leg this
    // test has no further use for.
    let _fourth_leg = recv_gated(&mut gate, "peer's post-recovery re-armed leg").await;
    assert_eq!(
        fixture.relay.counts.attempts.load(Ordering::SeqCst),
        4,
        "exactly one re-armed leg beyond the three real completions above; anything more is runaway churn"
    );
    assert_eq!(fixture.relay.counts.abandoned.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.relay.counts.busy.load(Ordering::SeqCst), 0);
}

/// The exact same-native-call ordering the incident traced: a re-armed peer
/// leg (task_events.rs's re-arm block) completes once with a genuine
/// success and then, still within that same native call, fails on its very
/// next completion. Before the fix, `apply_aggregate_completion` only ever
/// *added* to `confirmedMachines` on success and never revoked that
/// confirmation on a later same-call failure, so the peer came back in both
/// `confirmedMachines` and `machineErrors` on the very same response — and
/// `accept_page`'s add-fault-then-clear-confirmed order let the stale
/// confirmation erase the real fault, silently reporting the peer healthy.
///
/// A subscription's own collector (`subscription_timing::Collection::ready`,
/// via `query.subscription_timing`) governs batch completion here, not the
/// public wait's `minEvents`/`task_event_batch_is_complete` — subscriptions
/// have no such field (`SubscribeRequest` denies unknown fields and never
/// had one). `ready` seals a batch immediately once anything *urgent*
/// arrives (see `subscription_timing::urgent`), so an urgent first event
/// would complete the batch on the spot and the re-arm this test exists to
/// exercise would never happen. `task.pr_created` is explicitly non-urgent,
/// so the single successful event leaves `ready` false until the
/// subscription's own quiet/max-hold window elapses — guaranteeing the
/// re-arm deterministically, the same way `minEvents` would have on the
/// public wait, without needing (or adding) any such field here.
///
/// Proves the peer stays stale through both the diagnostic row and a plain
/// compact read plus ACK, the first leg's successful event and checkpoint
/// are retained rather than lost to the later fault, and a subsequent
/// *unchanged* failure does not mint another coverage-only wake.
#[tokio::test(start_paused = true)]
async fn subscription_remote_same_call_success_then_failure_keeps_the_peer_stale() {
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
    let peer_id = peer.config().desktop_id.clone();
    let (relay, mut gate) = connect_gated_peer(&source, peer);
    let app = router(source.clone());
    // Existing, supported subscription timing parameters (the same override
    // `WatchFixture::request` already uses) — no `minEvents` and none needed:
    // a non-urgent first event already leaves `ready` false on its own.
    let (status, initial) =
        subscription_request(&app, "POST", "/v1/event-subscriptions", WatchFixture::request())
            .await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    let fixture = GatedPeerFixture {
        source: source.clone(),
        relay,
        app,
        id: initial["id"].as_str().unwrap().to_string(),
        service: tokio::spawn(super::super::super::event_subscriptions::run(source.clone())),
    };

    // The peer's leg succeeds first, with one real, non-urgent event: not
    // enough on its own to satisfy the subscription's own collector, so this
    // call is not yet complete and production re-arms the same peer for
    // another leg within this same native call.
    let first_leg = recv_gated(&mut gate, "first (successful) long poll").await;
    first_leg.resolve(Ok(HttpInvokeResponse {
        status: 200,
        body: Some(json!({
            "waitOutcome": "events",
            "cursor": "native-after-first-success",
            "events": [{"taskId": "pending-peer-child", "type": "task.pr_created", "seq": 501}],
            "hasMore": false,
        })),
        error: None,
    }));

    // The re-armed leg then fails, still within the same native call as the
    // success above. Deterministic proof of that ordering: the re-armed
    // dispatch is captured before any batch has been sealed for this
    // subscription — if the first success had already concluded its own
    // native call, this row would already be `ready`.
    let second_leg = recv_gated(&mut gate, "re-armed long poll after the success").await;
    assert_ne!(
        fixture.row().wake_state,
        "ready",
        "the re-armed leg must be dispatched inside the same in-flight native \
         call as the first success, before any batch is sealed"
    );
    second_leg.resolve(Ok(HttpInvokeResponse {
        status: 502,
        body: None,
        error: Some("peer connection reset".into()),
    }));

    let page = fixture.page().await;
    let batch = page.pending.as_ref().unwrap();
    assert!(batch.get("watchError").is_none(), "{batch}");
    assert_eq!(
        event_pairs(batch),
        vec![("pending-peer-child".into(), "task.pr_created".into())],
        "the earlier successful event must not be lost to the later failure: {batch}"
    );
    assert_eq!(batch["machineErrors"][0]["machineId"], json!(peer_id));
    assert!(
        !batch["confirmedMachines"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|id| id.as_str() == Some(peer_id.as_str())),
        "the later same-call failure must revoke the earlier same-call confirmation for the peer \
         (unrelated healthy machines may still legitimately confirm): {batch}"
    );
    fixture.ack(&page).await;
    assert_eq!(
        fixture.row().stale_machines.get(&peer_id).map(String::as_str),
        Some("peer connection reset"),
        "the peer must remain stale — a stale-then-silently-cleared read here is exactly the incident bug"
    );
    assert_eq!(
        native_cursor_of(
            &decode_cursor(fixture.row().cursor.as_ref().unwrap())["cursorsByMachine"]
                [peer_id.as_str()]
                .clone()
        ),
        "native-after-first-success",
        "the earlier successful leg's checkpoint must be retained despite the later fault"
    );

    // Same proof through the plain (non-diagnostic) compact contract a real
    // manager reads, and after ACK.
    let compact = compact_read(&fixture.app, &fixture.id).await;
    assert_eq!(
        compact["staleMachines"][peer_id.as_str()],
        json!("peer connection reset")
    );

    // A later, unchanged failure for the same peer must not mint another
    // coverage-only wake — the set of stale machines has not changed. The
    // dispatch of `third_leg` already pushes `attempts` to 3 (it counts at
    // admission, not completion), so waiting on `attempts >= 3` here would
    // be checking a condition that is already true and would prove nothing
    // about this outage collection having actually settled. Synchronize
    // instead on the *next* (fourth) admission, which can only happen after
    // this call concludes, reaches `accept_page`, and the worker starts its
    // next cycle — proof the unchanged failure was fully processed before
    // asserting no new batch was minted from it.
    let batch_id_before = fixture.row().batch_id;
    let third_leg = recv_gated(&mut gate, "next cycle's peer long poll").await;
    third_leg.resolve(Ok(HttpInvokeResponse {
        status: 502,
        body: None,
        error: Some("peer connection reset".into()),
    }));
    let _fourth_leg = recv_gated(
        &mut gate,
        "peer's following long poll, proving the unchanged-failure call settled",
    )
    .await;
    assert_eq!(
        fixture.row().batch_id, batch_id_before,
        "an unchanged (still-down) peer must not manufacture a new pending batch"
    );
    assert_eq!(
        fixture.row().stale_machines.get(&peer_id).map(String::as_str),
        Some("peer connection reset")
    );
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
