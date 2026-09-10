//! Internal subscription scheduling. Public waits, peer legs and adapters do
//! not own this policy. Defaults adopted by the manager, not measured tuning.
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

pub(super) const QUIET: Duration = Duration::from_millis(1_000);
pub(super) const MAX_HOLD: Duration = Duration::from_millis(5_000);
pub(super) const ADMISSION_INTERVAL: Duration = Duration::from_millis(5_000);

#[derive(Default)]
pub(super) struct Collection {
    first: Option<Instant>,
    last: Option<Instant>,
    urgent: bool,
}

impl Collection {
    pub(super) fn observe(&mut self, events: &[Value], now: Instant) {
        if !events.is_empty() {
            self.first.get_or_insert(now);
            self.last = Some(now);
            self.urgent |= events.iter().any(urgent);
        }
    }

    pub(super) fn deadline(&self, receiver: Instant) -> Instant {
        match (self.first, self.last) {
            (Some(first), Some(last)) => (last + QUIET).min(first + MAX_HOLD).min(receiver),
            _ => receiver,
        }
    }

    pub(super) fn ready(
        &self,
        count: usize,
        capacity: i64,
        receiver: Instant,
        now: Instant,
    ) -> bool {
        count > 0 && (self.urgent || count >= capacity as usize || now >= self.deadline(receiver))
    }
}

/// Called only on relevant structured facts. Unknown attention is conservative;
/// no provider-authored prose participates in this decision.
fn urgent(event: &Value) -> bool {
    let payload = &event["payload"];
    match event["type"].as_str() {
        Some("run.finished") => payload["status"] != "succeeded",
        Some("task.runtime_changed") => {
            payload["runtimeState"] == "waiting"
                || payload["runtimeState"] == "exited"
                || payload["notificationContext"]["providerParked"] == true
                || payload["notificationContext"]["latestRun"]["status"] == "failed"
                || payload["currentTask"]["latestRun"]["status"] == "failed"
        }
        Some(
            "task.awaiting_advance"
            | "task.blocked"
            | "task.unblocked"
            | "task.closed"
            | "task.pr_created"
            | "task.merge_signaled"
            | "task.workflow_changed"
            | "task.revision_requested",
        ) => false,
        // Includes confirmed input, failed lifecycle/handoff/teardown, provider
        // parking and future unknown attention. Watch faults seal separately.
        _ => true,
    }
}

/// Monotonic process-local gate. The record persists only that an admission
/// occurred; recovery never trusts a wall-clock timestamp or accumulates credit.
pub(super) struct Admission {
    next: Option<Instant>,
}

impl Admission {
    pub(super) fn new(recovered: bool) -> Self {
        Self {
            next: recovered.then(|| Instant::now() + ADMISSION_INTERVAL),
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.next.filter(|deadline| *deadline > Instant::now())
    }

    pub(super) fn admitted(&mut self) {
        self.next = Some(Instant::now() + ADMISSION_INTERVAL);
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) enum TestEvent {
    Observed(usize, Instant),
    Admitted(i64, Instant),
    Delivered,
}

#[cfg(test)]
pub(super) fn observed(state: &super::AppState, count: usize) {
    if let Some(events) = &state.subscription_test_events {
        let _ = events.send(TestEvent::Observed(count, Instant::now()));
    }
}
