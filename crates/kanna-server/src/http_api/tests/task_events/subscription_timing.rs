//! Real worker/HTTP/DB collectors and fenced adapter I/O with a paused clock.
use super::*;
use crate::db::TaskEventKind;
use crate::http_api::subscription_timing::TestEvent;
use kanna_daemon::protocol::{
    Command as DaemonCommand, Event as DaemonEvent, SessionInfo, SessionState, SessionStatus,
};
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use tokio::time::Instant;

async fn until(mut predicate: impl FnMut() -> bool) {
    // Real I/O (the isolated executable) must make progress without Tokio
    // auto-advancing to the proxy timeout. No policy clock is moved here.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "timing fixture did not reach barrier"
        );
        tokio::task::yield_now().await;
    }
}

struct Watch {
    state: Arc<AppState>,
    db: Db,
    app: Router,
    id: String,
    events: tokio::sync::mpsc::UnboundedReceiver<TestEvent>,
    service: tokio::task::JoinHandle<()>,
    daemon: tokio::task::JoinHandle<()>,
    inputs: Arc<Mutex<Vec<String>>>,
    lose_input_reply: Arc<AtomicBool>,
    proxy: std::path::PathBuf,
    trace: std::path::PathBuf,
    delivery: &'static str,
    delivery_gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.service.abort();
        self.daemon.abort();
        let _ = std::fs::remove_file(&self.proxy);
        let _ = std::fs::remove_file(&self.trace);
    }
}

impl Watch {
    async fn new(delivery: &'static str) -> Self {
        Self::configured(delivery, false).await
    }

    /// A subscription with its own quiet/max-hold/admission-interval
    /// overrides, so a test can exercise the collector's timing logic
    /// without depending on the (much larger) global defaults or the fixed
    /// 240s native receiver window.
    async fn with_overrides(delivery: &'static str, overrides: Value) -> Self {
        Self::configured_with(delivery, false, overrides).await
    }

    async fn configured(delivery: &'static str, hold_delivery: bool) -> Self {
        Self::configured_with(delivery, hold_delivery, json!({})).await
    }

    async fn configured_with(
        delivery: &'static str,
        hold_delivery: bool,
        overrides: Value,
    ) -> Self {
        let mut state =
            test_state_with_seed(&format!("timed-{delivery}"), "Timing", seed_orchestration);
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let proxy = std::path::PathBuf::from(crate::test_paths::unique_test_file(
            "subscription-proxy",
            "py",
        ));
        let trace = proxy.with_extension("jsonl");
        // No global PATH/env override: this one AppState owns its executable.
        std::fs::write(&proxy, format!(r#"#!/usr/bin/python3
import sys, json
for line in sys.stdin:
    request = json.loads(line)
    with open({}, 'a') as trace:
        trace.write(json.dumps(request) + '\n')
    if 'id' not in request:
        continue
    result = {{'thread': {{'cwd': '/workspace/manager'}}}} if request['method'] == 'thread/read' else {{}}
    print(json.dumps({{'id': request['id'], 'result': result}}), flush=True)
"#, serde_json::to_string(&trace.to_string_lossy()).unwrap())).unwrap();
        std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mutable = Arc::get_mut(&mut state).unwrap();
        let delivery_gate = hold_delivery.then(|| Arc::new(tokio::sync::Semaphore::new(0)));
        mutable.subscription_delivery_barrier = delivery_gate.clone();
        mutable.subscription_test_events = Some(tx);
        mutable.subscription_proxy_executable = Some(proxy.to_string_lossy().into());
        let db = Db::open(&state.config().db_path).unwrap();
        start_run(&db, "manager", "child-c", "in progress");
        db.connection_for_e2e_tests().execute(
            "UPDATE stage_run SET agent_provider='codex', cwd='/workspace/manager', provider_session_id='native-manager' WHERE id='manager'", [],
        ).unwrap();
        std::fs::create_dir_all(&state.config().daemon_dir).unwrap();
        let listener = UnixListener::bind(super::super::daemon_socket_path_for_dir(
            &state.config().daemon_dir,
        ))
        .unwrap();
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let recorded = inputs.clone();
        let lose_input_reply = Arc::new(AtomicBool::new(false));
        let uncertain = lose_input_reply.clone();
        let daemon = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(result) = clients.join_next(), if !clients.is_empty() => { result.unwrap(); }
                    connection = listener.accept() => {
                        let (stream, _) = connection.unwrap();
                        let recorded = recorded.clone();
                        let uncertain = uncertain.clone();
                        clients.spawn(async move {
                            let (read, mut write) = stream.into_split();
                            let mut read = BufReader::new(read);
                            while let Some(command) = super::super::read_test_daemon_command_optional(&mut read, &mut write).await {
                                let reply = match command {
                                    DaemonCommand::List => DaemonEvent::SessionList { sessions: vec![SessionInfo {
                                        session_id: "child-c".into(), pid: 42, cwd: "/workspace/manager".into(),
                                        state: SessionState::Active, idle_seconds: 0, status: SessionStatus::Idle,
                                        status_observed: true, kind: Default::default(), composer_text: None,
                                        composer_attestation: Default::default(),
                                    }] },
                                    DaemonCommand::SubmitInputIfSession { session_id, expected_pid, data } => {
                                        assert_eq!(session_id, "child-c"); assert_eq!(expected_pid, 42);
                                        recorded.lock().unwrap().push(String::from_utf8(data).unwrap());
                                        if uncertain.load(Ordering::SeqCst) { return; }
                                        DaemonEvent::Ok
                                    }
                                    other => panic!("unexpected wake command: {other:?}"),
                                };
                                if write.write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes()).await.is_err() { break; }
                            }
                        });
                    }
                }
            }
        });
        let app = router(state.clone());
        // Start empty so this is a fresh registration, not restart recovery.
        let service = tokio::spawn(super::super::super::event_subscriptions::run(state.clone()));
        tokio::task::yield_now().await;
        let mut body = json!({"taskId":"child-c", "taskIds":["child-a","child-b"], "localOnly":true, "delivery":delivery});
        if let Some(extra) = overrides.as_object() {
            for (key, value) in extra {
                body[key] = value.clone();
            }
        }
        let (status, row) =
            subscription_request(&app, "POST", "/v1/event-subscriptions", body).await;
        assert_eq!(status, StatusCode::OK, "{row}");
        assert!(row["pending"].is_null());
        Self {
            state,
            db,
            app,
            id: row["id"].as_str().unwrap().into(),
            events,
            service,
            daemon,
            inputs,
            lose_input_reply,
            proxy,
            trace,
            delivery,
            delivery_gate,
        }
    }

    fn row(&self) -> crate::db::EventSubscription {
        self.db.event_subscription(&self.id).unwrap().unwrap()
    }
    fn emit(&self, kind: TaskEventKind) {
        self.db
            .append_task_event("child-a", kind, json!({}))
            .unwrap();
    }
    async fn observed(&mut self) -> Instant {
        let mut observed = None;
        until(|| {
            while let Ok(event) = self.events.try_recv() {
                match event {
                    TestEvent::Observed(count, at) if count > 0 => {
                        observed = Some(at);
                        return true;
                    }
                    TestEvent::Admitted(..) => panic!("admission overtook observation barrier"),
                    _ => {}
                }
            }
            false
        })
        .await;
        observed.unwrap()
    }
    async fn admitted(&mut self) -> (i64, Instant) {
        let mut admitted = None;
        until(|| {
            while let Ok(event) = self.events.try_recv() {
                if let TestEvent::Admitted(batch, at) = event {
                    admitted = Some((batch, at));
                    return true;
                }
            }
            false
        })
        .await;
        admitted.unwrap()
    }
    /// Advances the paused clock in small steps until a chained native call
    /// has actually returned via its own receiver timing out (not the
    /// subscription's intrinsic deadline) and the worker re-issued another
    /// call — the causal proof that a native receiver boundary was crossed
    /// mid-collection, as opposed to inferring it from a single large
    /// `advance()` that happens to also land past the eventual admission.
    /// Self-paced (rather than a fixed pre-computed offset) because exactly
    /// when the in-flight native call dispatched — and hence exactly when
    /// its own 240s receiver lands — depends on scheduler turns this test
    /// does not control. Returns the instant the timeout was observed, so a
    /// caller can size its remaining advance against a deadline it computed
    /// independently (e.g. from its own `observed()` instant) rather than
    /// assuming this step landed exactly on a round boundary.
    async fn leg_timed_out(&mut self) -> Instant {
        for _ in 0..280 {
            while let Ok(event) = self.events.try_recv() {
                if matches!(event, TestEvent::LegTimedOut) {
                    return Instant::now();
                }
                assert!(
                    !matches!(event, TestEvent::Admitted(..)),
                    "admission overtook the expected native-leg-timeout barrier"
                );
            }
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        panic!("native leg did not time out within the expected window");
    }
    fn no_admission(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            assert!(!matches!(event, TestEvent::Admitted(..)), "{event:?}");
        }
    }
    async fn delivery_returned(&mut self) {
        until(|| {
            while let Ok(event) = self.events.try_recv() {
                if matches!(event, TestEvent::Delivered) {
                    return true;
                }
            }
            false
        })
        .await;
    }
    async fn delivered(&self) {
        until(|| self.row().wake_state == "notified").await;
        if self.delivery == "input" {
            assert!(self
                .inputs
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains(&self.id));
            let inputs = self.db.list_task_inputs("child-c", 100).unwrap();
            assert!(inputs.iter().all(|input| input.source == "engine"));
        } else {
            let trace = std::fs::read_to_string(&self.trace).unwrap();
            let request: Value = trace
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .rfind(|request| request["method"] == "turn/start")
                .unwrap();
            assert_eq!(request["params"]["threadId"], "native-manager");
            assert_eq!(request["params"]["input"], json!([]));
            assert_eq!(
                request["params"]["toolOutput"]["name"],
                "kanna_event_subscription"
            );
            assert_eq!(self.db.count_task_inputs("child-c").unwrap(), 0);
        }
    }
    async fn ack(&self, batch: i64) {
        let (status, _) = subscription_request(
            &self.app,
            "POST",
            &format!("/v1/event-subscriptions/{}/read", self.id),
            json!({"acknowledgeBatchId":batch}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test(start_paused = true)]
async fn both_adapters_trail_bursts_and_rate_gate_urgent_attention() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::new(delivery).await;
        watch.emit(TaskEventKind::PrCreated);
        let first = watch.observed().await;
        tokio::time::advance(Duration::from_millis(800)).await;
        watch.emit(TaskEventKind::TaskClosed);
        // max_hold is measured from `first`, quiet from this later event —
        // with both set to 300s, max_hold's earlier deadline always wins, so
        // this later observation does not push sealing out to `last + 300s`.
        watch.observed().await;
        tokio::time::advance(Duration::from_millis(299_199)).await;
        watch.no_admission();
        tokio::time::advance(Duration::from_millis(1)).await;
        let (batch, admitted) = watch.admitted().await;
        assert_eq!(admitted, first + Duration::from_secs(300));
        watch.delivered().await;
        assert_eq!(
            watch.row().pending.unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        watch.ack(batch).await;
        watch.emit(TaskEventKind::LifecycleFailed);
        watch.observed().await;
        until(|| watch.row().pending.is_some()).await;
        // Urgent sealed immediately, but cannot bypass the previous admission.
        watch.no_admission();
        tokio::time::advance(Duration::from_secs(60)).await;
        let (_, next) = watch.admitted().await;
        assert_eq!(next - admitted, Duration::from_secs(60));
        watch.delivered().await;
    }
}

#[tokio::test(start_paused = true)]
async fn sustained_full_pages_ack_and_later_failure_cannot_bypass_either_adapter_gate() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::new(delivery).await;
        for _ in 0..201 {
            watch.emit(TaskEventKind::PrCreated);
        }
        watch.emit(TaskEventKind::LifecycleFailed);
        let (batch, first) = watch.admitted().await;
        watch.delivered().await;
        let immutable = watch.row().pending.unwrap();
        assert_eq!(immutable["events"].as_array().unwrap().len(), 100);
        tokio::time::advance(Duration::from_secs(70)).await;
        watch.no_admission();
        assert_eq!(
            watch.row().pending.unwrap(),
            immutable,
            "unacked urgent backlog cannot replace this page"
        );
        watch.ack(batch).await;
        let (batch, second) = watch.admitted().await;
        assert!(second - first >= Duration::from_secs(60));
        watch.delivered().await;
        watch.ack(batch).await;
        watch.observed().await;
        until(|| watch.row().pending.is_some()).await;
        assert_eq!(
            event_pairs(watch.row().pending.as_ref().unwrap()),
            vec![
                ("child-a".into(), "task.pr_created".into()),
                ("child-a".into(), "task.lifecycle_failed".into())
            ]
        );
        watch.no_admission();
        tokio::time::advance(Duration::from_secs(60)).await;
        let (_, third) = watch.admitted().await;
        assert!(third - second >= Duration::from_secs(60));
        watch.delivered().await;
    }
}

#[tokio::test(start_paused = true)]
async fn ack_during_cooldown_invalidates_scheduled_wake_without_erasing_gate() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::new(delivery).await;
        watch.emit(TaskEventKind::AwaitingInput);
        let (first_batch, first_at) = watch.admitted().await;
        watch.delivered().await;
        watch.ack(first_batch).await;
        watch.emit(TaskEventKind::LifecycleFailed);
        watch.observed().await;
        until(|| watch.row().pending.is_some()).await;
        let unsent = watch.row();
        assert_eq!(unsent.wake_state, "pending");
        watch.ack(unsent.batch_id).await;
        watch.emit(TaskEventKind::AwaitingInput);
        watch.observed().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        let (batch, at) = watch.admitted().await;
        assert_eq!(batch, unsent.batch_id + 1);
        assert_eq!(at - first_at, Duration::from_secs(60));
        watch.delivered().await;
        let (status, _) = subscription_request(
            &watch.app,
            "POST",
            &format!("/v1/event-subscriptions/{}/read", watch.id),
            json!({"acknowledgeBatchId":first_batch}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
}

#[tokio::test(start_paused = true)]
async fn lone_noise_sustained_and_urgent_bursts_have_the_same_bounds_for_both_adapters() {
    for delivery in ["input", "codex_app_server"] {
        // Per-subscription overrides, not the 300000/300000/60000ms global
        // defaults: this test exercises the collector's quiet/max-hold/
        // admission arithmetic itself, which the fixed 240s native receiver
        // window would otherwise dominate now that the defaults exceed it
        // (any intervening real event — even an irrelevant one — triggers a
        // re-check that can complete the batch at the receiver instead).
        let mut watch = Watch::with_overrides(
            delivery,
            json!({"quietMs": 2_000, "maxHoldMs": 10_000, "minAdmissionIntervalMs": 1_000}),
        )
        .await;
        watch.emit(TaskEventKind::PrCreated);
        let first = watch.observed().await;
        for _ in 0..4 {
            tokio::time::advance(Duration::from_millis(400)).await;
            watch.emit(TaskEventKind::RunStarted); // irrelevant, never resets quiet
            tokio::task::yield_now().await;
        }
        watch.no_admission();
        tokio::time::advance(Duration::from_millis(400)).await;
        let (batch, at) = watch.admitted().await;
        assert_eq!(at - first, Duration::from_secs(2));
        watch.delivered().await;
        watch.ack(batch).await;
        watch.emit(TaskEventKind::PrCreated);
        let first = watch.observed().await;
        for _ in 0..6 {
            tokio::time::advance(Duration::from_millis(1_600)).await;
            watch.emit(TaskEventKind::PrCreated);
            watch.observed().await;
            watch.no_admission();
        }
        tokio::time::advance(Duration::from_millis(400)).await;
        let (batch, at) = watch.admitted().await;
        assert_eq!(
            at - first,
            Duration::from_secs(10),
            "continuous relevance cannot extend the cap"
        );
        watch.delivered().await;
        watch.ack(batch).await;
        tokio::time::advance(Duration::from_millis(1_500)).await;
        watch.emit(TaskEventKind::PrCreated);
        watch.observed().await;
        tokio::time::advance(Duration::from_millis(200)).await;
        watch.emit(TaskEventKind::AwaitingInput);
        let urgent_at = watch.observed().await;
        let (_, admitted) = watch.admitted().await;
        assert_eq!(
            admitted, urgent_at,
            "urgent attention seals the ordinary burst with no quiet hold"
        );
        watch.delivered().await;
    }
}

#[tokio::test(start_paused = true)]
async fn retirement_during_collection_and_cooldown_cannot_dispatch_for_either_adapter() {
    for delivery in ["input", "codex_app_server"] {
        for cooldown in [false, true] {
            for close in [false, true] {
                let mut watch = Watch::new(delivery).await;
                if cooldown {
                    watch.emit(TaskEventKind::AwaitingInput);
                    let (batch, _) = watch.admitted().await;
                    watch.delivered().await;
                    watch.ack(batch).await;
                    watch.emit(TaskEventKind::LifecycleFailed);
                } else {
                    watch.emit(TaskEventKind::PrCreated);
                }
                watch.observed().await;
                let checkpoint = watch.row().cursor;
                if close {
                    watch.db.close_pipeline_item("child-c").unwrap();
                } else {
                    start_run(&watch.db, "replacement", "child-c", "in progress");
                }
                watch
                    .state
                    .publish_state_changed(kanna_agent_protocol::StateChangeScope::Tasks);
                until(|| !watch.row().active).await;
                tokio::time::advance(Duration::from_secs(70)).await;
                watch.no_admission();
                assert_eq!(watch.row().cursor, checkpoint);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn restart_before_scheduled_send_rearms_once_and_preserves_the_pending_page() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::new(delivery).await;
        watch.emit(TaskEventKind::AwaitingInput);
        let (batch, _) = watch.admitted().await;
        watch.delivered().await;
        watch.ack(batch).await;
        watch.emit(TaskEventKind::LifecycleFailed);
        watch.observed().await;
        until(|| watch.row().pending.is_some()).await;
        let page = watch.row().pending;
        watch.service.abort();
        while !watch.service.is_finished() {
            tokio::task::yield_now().await;
        }
        let restarted = Instant::now();
        watch.service = tokio::spawn(super::super::super::event_subscriptions::run(
            watch.state.clone(),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(59_999)).await;
        watch.no_admission();
        assert_eq!(watch.row().pending, page);
        tokio::time::advance(Duration::from_millis(1)).await;
        let (next, admitted) = watch.admitted().await;
        assert_eq!(next, batch + 1);
        assert_eq!(admitted - restarted, Duration::from_secs(60));
        watch.delivered().await;
        assert_eq!(watch.row().pending, page);
    }
}

#[tokio::test(start_paused = true)]
async fn restart_after_ack_rearms_one_cooldown_and_old_records_remain_readable() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::new(delivery).await;
        watch.emit(TaskEventKind::AwaitingInput);
        let (batch, _) = watch.admitted().await;
        watch.delivered().await;
        watch.ack(batch).await;
        let checkpoint = watch.row().cursor;
        assert!(watch.row().wake_admitted);
        watch.service.abort();
        while !watch.service.is_finished() {
            tokio::task::yield_now().await;
        }
        // Model an independently shipped older writer: its record omits the
        // additive hint. The new service conservatively rearms recovered rows.
        let mut old = serde_json::to_value(watch.row()).unwrap();
        old.as_object_mut().unwrap().remove("wakeAdmitted");
        watch
            .db
            .connection_for_e2e_tests()
            .execute(
                "UPDATE event_subscription SET record=? WHERE id=?",
                [old.to_string(), watch.id.clone()],
            )
            .unwrap();
        assert!(!watch.row().wake_admitted);
        watch.service = tokio::spawn(super::super::super::event_subscriptions::run(
            watch.state.clone(),
        ));
        watch.emit(TaskEventKind::LifecycleFailed);
        let observed = watch.observed().await;
        until(|| watch.row().pending.is_some()).await;
        assert_eq!(watch.row().cursor, checkpoint);
        tokio::time::advance(Duration::from_millis(59_999)).await;
        watch.no_admission();
        tokio::time::advance(Duration::from_millis(1)).await;
        let (_, admitted) = watch.admitted().await;
        assert_eq!(admitted - observed, Duration::from_secs(60));
        watch.delivered().await;
    }
}

#[tokio::test(start_paused = true)]
async fn ack_racing_real_delivery_result_keeps_cooldown_and_cannot_resurrect_page() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::configured(delivery, true).await;
        watch.emit(TaskEventKind::AwaitingInput);
        let (batch, first) = watch.admitted().await;
        watch.delivery_returned().await;
        assert_eq!(watch.row().wake_state, "sending");
        let checkpoint = watch.row().pending.unwrap()["cursor"].clone();
        watch.ack(batch).await;
        watch.delivery_gate.as_ref().unwrap().add_permits(1);
        watch.emit(TaskEventKind::LifecycleFailed);
        watch.observed().await;
        assert_eq!(json!(watch.row().cursor), checkpoint);
        until(|| watch.row().pending.is_some()).await;
        watch.no_admission();
        tokio::time::advance(Duration::from_secs(60)).await;
        let (next_batch, next) = watch.admitted().await;
        assert_eq!(next_batch, batch + 1);
        assert_eq!(next - first, Duration::from_secs(60));
        watch.delivery_returned().await;
        watch.delivery_gate.as_ref().unwrap().add_permits(1);
        watch.delivered().await;
    }
}

#[tokio::test(start_paused = true)]
async fn restart_during_actual_delivery_parks_uncertainty_without_repeated_wake() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::configured(delivery, true).await;
        watch.emit(TaskEventKind::AwaitingInput);
        watch.admitted().await;
        watch.delivery_returned().await;
        let page = watch.row().pending;
        watch.service.abort();
        while !watch.service.is_finished() {
            tokio::task::yield_now().await;
        }
        watch.service = tokio::spawn(super::super::super::event_subscriptions::run(
            watch.state.clone(),
        ));
        until(|| watch.row().wake_state == "uncertain").await;
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(60)).await;
            watch.state.event_subscriptions_changed.notify_waiters();
            tokio::task::yield_now().await;
            watch.no_admission();
        }
        assert_eq!(watch.row().pending, page);
    }
}

/// Advances the paused clock in 1s increments until at least `target`. A
/// single large `advance()` can leave a background task's own periodic (here,
/// 5s) recheck timer unpolled through several of its intermediate ticks
/// instead of driving each one in turn — harmless when nothing of interest
/// happens in between, but exactly the case a moving-deadline regression must
/// step through faithfully rather than skip over.
async fn advance_in_one_second_steps_to(target: Instant) {
    while tokio::time::Instant::now() < target {
        tokio::time::advance(Duration::from_secs(1)).await;
    }
}

/// Advances to just before `deadline`, asserts nothing has been admitted yet,
/// then steps across it and returns the admission. Computed relative to
/// `tokio::time::Instant::now()` rather than a fixed literal, since prior
/// self-paced barriers (`leg_timed_out`) do not land on a predictable offset.
async fn advance_to_deadline_and_admit(watch: &mut Watch, deadline: Instant) -> (i64, Instant) {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining > Duration::from_millis(1) {
        tokio::time::advance(remaining - Duration::from_millis(1)).await;
        watch.no_admission();
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    watch.admitted().await
}

#[tokio::test(start_paused = true)]
async fn a_relevant_event_observed_near_a_native_leg_start_survives_its_timeout() {
    let mut watch = Watch::new("input").await;
    // Reproduces the finding's own example: an ordinary event observed near
    // the start of the first native call. That call must still expire at its
    // own 240s receiver — well before the subscription's 300s intrinsic
    // deadline — and re-issue a second call rather than losing the event it
    // already has. The leg timeout is a self-paced barrier (the worker
    // actually reporting a `"waitOutcome": "timeout"` re-issue), not an
    // assumption from a single large `advance()`.
    tokio::task::yield_now().await;
    watch.emit(TaskEventKind::PrCreated);
    let observed = watch.observed().await;
    watch.leg_timed_out().await;
    watch.no_admission();
    let (_, admitted) =
        advance_to_deadline_and_admit(&mut watch, observed + Duration::from_secs(300)).await;
    assert_eq!(admitted - observed, Duration::from_secs(300));
    watch.delivered().await;
    assert_eq!(
        event_pairs(watch.row().pending.as_ref().unwrap()),
        vec![("child-a".into(), "task.pr_created".into())]
    );
}

#[tokio::test(start_paused = true)]
async fn events_straddling_a_native_leg_boundary_are_all_retained_and_acked_together() {
    let mut watch = Watch::new("input").await;
    // One event lands inside leg 1; a second, different event lands only
    // after leg 1's own receiver has timed out and leg 2 has started. Both
    // must reach the eventual pending batch — a native "timeout" outcome is
    // call-local, so the first event exists only in leg 1's own (discarded)
    // response unless the chain retains it explicitly.
    tokio::task::yield_now().await;
    watch.emit(TaskEventKind::PrCreated);
    let first_observed = watch.observed().await;
    watch.leg_timed_out().await;
    watch.emit(TaskEventKind::TaskClosed);
    watch.observed().await;
    watch.no_admission();
    // max_hold is anchored to the first event, so the intrinsic deadline is
    // first_observed + 300s regardless of the second (later) event's own
    // quiet window.
    let (_, admitted) =
        advance_to_deadline_and_admit(&mut watch, first_observed + Duration::from_secs(300)).await;
    assert_eq!(admitted - first_observed, Duration::from_secs(300));
    watch.delivered().await;
    assert_eq!(
        event_pairs(watch.row().pending.as_ref().unwrap()),
        vec![
            ("child-a".into(), "task.pr_created".into()),
            ("child-a".into(), "task.closed".into()),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_later_event_that_extends_the_live_quiet_deadline_mid_leg_is_not_sealed_early() {
    // quiet (300s) < max_hold (600s), so quiet — anchored to the LATEST
    // observation — actually controls the deadline, unlike the default
    // quiet == max_hold configuration where max_hold (anchored to the first
    // observation) always wins or ties regardless of later events.
    let mut watch =
        Watch::with_overrides("input", json!({"quietMs": 300_000, "maxHoldMs": 600_000})).await;
    tokio::task::yield_now().await;
    watch.emit(TaskEventKind::PrCreated);
    let first_observed = watch.observed().await;
    // Leg 1 dispatches with nothing observed yet (240s ceiling) and times out
    // at its own receiver, well short of the then-current 300s intrinsic
    // deadline.
    watch.leg_timed_out().await;
    // Leg 2 dispatches sized to that 300s deadline (60s remaining, clamped
    // under the 240s ceiling). A second relevant event lands inside it,
    // pushing the live intrinsic deadline out (last_observed + 300s quiet,
    // still short of first_observed + 600s max_hold) — but leg 2's own
    // receiver was already fixed at the stale 300s point when it was
    // dispatched.
    watch.emit(TaskEventKind::TaskClosed);
    let second_observed = watch.observed().await;
    let obsolete_deadline = first_observed + Duration::from_secs(300);
    let live_deadline = second_observed + Duration::from_secs(300);
    assert!(
        live_deadline > obsolete_deadline,
        "the second event must genuinely extend the deadline for this test to be meaningful"
    );
    // Crossing leg 2's own (now-stale) receiver deadline must not seal or
    // admit anything: the chain must re-evaluate the live collection instead
    // of trusting how leg 2's timeout was originally sized. Stepped in 1s
    // increments (rather than one large `advance()`) so the paused-clock
    // runtime reliably drives the worker's own periodic recheck through each
    // intermediate tick instead of skipping past them.
    advance_in_one_second_steps_to(obsolete_deadline + Duration::from_secs(1)).await;
    watch.no_admission();
    // The chain keeps re-issuing (through however many further 240s-capped
    // legs it takes) until the live deadline is actually reached.
    advance_in_one_second_steps_to(live_deadline + Duration::from_secs(1)).await;
    let (_, admitted) = watch.admitted().await;
    assert_eq!(admitted, live_deadline);
    watch.delivered().await;
    assert_eq!(
        event_pairs(watch.row().pending.as_ref().unwrap()),
        vec![
            ("child-a".into(), "task.pr_created".into()),
            ("child-a".into(), "task.closed".into()),
        ]
    );
    let (batch_id, pending_cursor) = {
        let row = watch.row();
        let pending = row.pending.clone().unwrap();
        (row.batch_id, pending["cursor"].clone())
    };
    watch.ack(batch_id).await;
    assert_eq!(json!(watch.row().cursor), pending_cursor);
}

#[tokio::test(start_paused = true)]
async fn page_capacity_accounting_survives_a_native_leg_boundary_and_seals_on_the_combined_total() {
    let mut watch = Watch::new("input").await;
    // 60 events (well short of the 100-event page) fill leg 1; it can only
    // end on its own 240s receiver, since neither capacity nor quiet/max-hold
    // (anchored 300s out) are reached yet.
    for _ in 0..60 {
        watch.emit(TaskEventKind::PrCreated);
    }
    let observed = watch.observed().await;
    watch.leg_timed_out().await;
    watch.no_admission();
    // 40 more events land in leg 2, completing the page at exactly 100. If
    // leg 2's own capacity accounting were not reduced by leg 1's retained
    // 60, it would need another 100 of its own (or, before this fix, silently
    // drop leg 1's 60 and need 100 more just to notice a full page).
    for _ in 0..40 {
        watch.emit(TaskEventKind::PrCreated);
    }
    let (_, admitted) = watch.admitted().await;
    // Sealed by the combined page reaching capacity, far short of the 300s
    // quiet/max-hold deadline.
    assert!(admitted - observed < Duration::from_secs(300));
    watch.delivered().await;
    assert_eq!(
        watch.row().pending.unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
}

#[tokio::test(start_paused = true)]
async fn definitely_undelivered_retry_needs_a_notification_and_an_admission_slot() {
    let mut watch = Watch::new("input").await;
    watch.daemon.abort();
    while !watch.daemon.is_finished() {
        tokio::task::yield_now().await;
    }
    watch.emit(TaskEventKind::AwaitingInput);
    let (batch, _) = watch.admitted().await;
    until(|| watch.row().wake_state == "pending" && watch.row().error.is_some()).await;
    tokio::time::advance(Duration::from_secs(70)).await;
    watch.no_admission(); // no timer-driven transport retry
    watch.state.event_subscriptions_changed.notify_waiters();
    let (same, second) = watch.admitted().await;
    assert_eq!(same, batch);
    until(|| watch.row().wake_state == "pending" && watch.row().error.is_some()).await;
    watch.state.event_subscriptions_changed.notify_waiters();
    tokio::task::yield_now().await;
    watch.no_admission();
    tokio::time::advance(Duration::from_secs(60)).await;
    let (_, third) = watch.admitted().await;
    assert_eq!(third - second, Duration::from_secs(60));
    assert_eq!(watch.db.count_task_inputs("child-c").unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn lost_input_reply_and_native_identity_fault_never_retry_or_switch_adapter() {
    for delivery in ["input", "codex_app_server"] {
        let mut watch = Watch::new(delivery).await;
        if delivery == "input" {
            watch.lose_input_reply.store(true, Ordering::SeqCst);
        } else {
            let proxy = std::fs::read_to_string(&watch.proxy).unwrap();
            std::fs::write(
                &watch.proxy,
                proxy.replace("/workspace/manager", "/workspace/other"),
            )
            .unwrap();
        }
        watch.emit(TaskEventKind::AwaitingInput);
        watch.admitted().await;
        until(|| watch.row().wake_state == "error").await;
        let page = watch.row().pending;
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(60)).await;
            watch.state.event_subscriptions_changed.notify_waiters();
            tokio::task::yield_now().await;
            watch.no_admission();
        }
        assert_eq!(watch.row().pending, page);
        assert_eq!(watch.row().delivery, delivery);
        assert_eq!(watch.db.count_task_inputs("child-c").unwrap(), 0);
        assert_eq!(
            watch.inputs.lock().unwrap().len(),
            usize::from(delivery == "input")
        );
    }
}
