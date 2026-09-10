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

    async fn configured(delivery: &'static str, hold_delivery: bool) -> Self {
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
        let (status, row) = subscription_request(&app, "POST", "/v1/event-subscriptions",
            json!({"taskId":"child-c", "taskIds":["child-a","child-b"], "localOnly":true, "delivery":delivery})).await;
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
        let last = watch.observed().await;
        tokio::time::advance(Duration::from_millis(999)).await;
        watch.no_admission();
        tokio::time::advance(Duration::from_millis(1)).await;
        let (batch, admitted) = watch.admitted().await;
        assert_eq!(admitted, last + Duration::from_secs(1));
        assert!(admitted > first + Duration::from_secs(1));
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
        tokio::time::advance(Duration::from_secs(5)).await;
        let (_, next) = watch.admitted().await;
        assert_eq!(next - admitted, Duration::from_secs(5));
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
        tokio::time::advance(Duration::from_secs(10)).await;
        watch.no_admission();
        assert_eq!(
            watch.row().pending.unwrap(),
            immutable,
            "unacked urgent backlog cannot replace this page"
        );
        watch.ack(batch).await;
        let (batch, second) = watch.admitted().await;
        assert!(second - first >= Duration::from_secs(5));
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
        tokio::time::advance(Duration::from_secs(5)).await;
        let (_, third) = watch.admitted().await;
        assert!(third - second >= Duration::from_secs(5));
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
        tokio::time::advance(Duration::from_secs(5)).await;
        let (batch, at) = watch.admitted().await;
        assert_eq!(batch, unsent.batch_id + 1);
        assert_eq!(at - first_at, Duration::from_secs(5));
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
        let mut watch = Watch::new(delivery).await;
        watch.emit(TaskEventKind::PrCreated);
        let first = watch.observed().await;
        for _ in 0..4 {
            tokio::time::advance(Duration::from_millis(200)).await;
            watch.emit(TaskEventKind::RunStarted); // irrelevant, never resets quiet
            tokio::task::yield_now().await;
        }
        watch.no_admission();
        tokio::time::advance(Duration::from_millis(200)).await;
        let (batch, at) = watch.admitted().await;
        assert_eq!(at - first, Duration::from_secs(1));
        watch.delivered().await;
        watch.ack(batch).await;
        watch.emit(TaskEventKind::PrCreated);
        let first = watch.observed().await;
        for _ in 0..6 {
            tokio::time::advance(Duration::from_millis(800)).await;
            watch.emit(TaskEventKind::PrCreated);
            watch.observed().await;
            watch.no_admission();
        }
        tokio::time::advance(Duration::from_millis(200)).await;
        let (batch, at) = watch.admitted().await;
        assert_eq!(
            at - first,
            Duration::from_secs(5),
            "continuous relevance cannot extend the cap"
        );
        watch.delivered().await;
        watch.ack(batch).await;
        tokio::time::advance(Duration::from_secs(5)).await;
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
                tokio::time::advance(Duration::from_secs(6)).await;
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
        tokio::time::advance(Duration::from_millis(4_999)).await;
        watch.no_admission();
        assert_eq!(watch.row().pending, page);
        tokio::time::advance(Duration::from_millis(1)).await;
        let (next, admitted) = watch.admitted().await;
        assert_eq!(next, batch + 1);
        assert_eq!(admitted - restarted, Duration::from_secs(5));
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
        tokio::time::advance(Duration::from_millis(4_999)).await;
        watch.no_admission();
        tokio::time::advance(Duration::from_millis(1)).await;
        let (_, admitted) = watch.admitted().await;
        assert_eq!(admitted - observed, Duration::from_secs(5));
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
        tokio::time::advance(Duration::from_secs(5)).await;
        let (next_batch, next) = watch.admitted().await;
        assert_eq!(next_batch, batch + 1);
        assert_eq!(next - first, Duration::from_secs(5));
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
            tokio::time::advance(Duration::from_secs(5)).await;
            watch.state.event_subscriptions_changed.notify_waiters();
            tokio::task::yield_now().await;
            watch.no_admission();
        }
        assert_eq!(watch.row().pending, page);
    }
}

#[tokio::test(start_paused = true)]
async fn receiver_deadline_clips_quiet_hold_without_losing_the_last_page() {
    let mut watch = Watch::new("input").await;
    // Native collection is waiting on the existing 240s receiver window.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(239_500)).await;
    watch.emit(TaskEventKind::PrCreated);
    let observed = watch.observed().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    let (_, admitted) = watch.admitted().await;
    assert_eq!(admitted - observed, Duration::from_millis(500));
    watch.delivered().await;
    assert_eq!(
        event_pairs(watch.row().pending.as_ref().unwrap()),
        vec![("child-a".into(), "task.pr_created".into())]
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
    tokio::time::advance(Duration::from_secs(10)).await;
    watch.no_admission(); // no timer-driven transport retry
    watch.state.event_subscriptions_changed.notify_waiters();
    let (same, second) = watch.admitted().await;
    assert_eq!(same, batch);
    until(|| watch.row().wake_state == "pending" && watch.row().error.is_some()).await;
    watch.state.event_subscriptions_changed.notify_waiters();
    tokio::task::yield_now().await;
    watch.no_admission();
    tokio::time::advance(Duration::from_secs(5)).await;
    let (_, third) = watch.admitted().await;
    assert_eq!(third - second, Duration::from_secs(5));
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
            tokio::time::advance(Duration::from_secs(5)).await;
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
