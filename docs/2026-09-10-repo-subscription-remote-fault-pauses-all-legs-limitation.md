# Repo-scoped subscription: one remote peer's fault pauses every leg

## Symptom

A repo-scoped `kanna_subscribe_events` subscription that fans out across
machines pauses observation **entirely** — including this machine's own
local notifications — when only one remote peer in the aggregate is
unreachable or errors. The operator-visible effect: a manager subscribed to a
whole repository stops receiving *any* wake, local or remote, until it
notices the pause and re-subscribes.

Observed in operation: subscription `watch-397` reached this paused state
(one peer's fault) and was left in place rather than discarded (its position
is still readable/resumable per the retry contract in
`docs/kanna-server-boundary.md`). The manager worked around it by starting a
fresh `local_only: true` subscription, `watch-1789035297611759000-0`, since
re-subscribing without `local_only` would hit the same remote fault again on
the next ack and re-pause immediately.

## Root cause in the current design

`accept_page` (`crates/kanna-server/src/http_api/event_subscriptions.rs`)
treats `machineErrors` as all-or-nothing for the whole subscription:

```rust
if batch["machineErrors"].as_array().is_some_and(|errors| !errors.is_empty()) {
    batch["watchError"] = json!("Some machines could not be observed; ...");
}
```

Any non-empty `machineErrors` — even for one peer out of many in an
aggregate repo/parent scope — sets `watchError` on the *entire* batch. `read`
then does, on acknowledgement:

```rust
row.error = batch["watchError"].as_str().map(str::to_owned);
if row.error.is_some() {
    row.active = false;
}
```

So acknowledging that page deactivates the whole subscription — the local
leg's healthy observation is paused along with the faulted remote leg, not
just the faulted machine's contribution. This is faithfully covered by
existing tests (`subscription_watch_failure_is_a_readable_attention_batch_and_does_not_spin`
in `task_events.rs`, and the peer-fault fixtures in
`subscription_remote.rs`) — those tests document today's actual (all-legs-paused)
behavior correctly; this note is not a claim that they are wrong.

## Why this task does not fix it

This is a structural fault-isolation gap in the aggregate collector, not a
query/timing knob — fixing it means teaching `wait_aggregate_task_events` and
`accept_page` to report a stale/errored machine as a per-machine annotation
(parallel to the `machineErrors[].stale` tagging the raw aggregate wait
already does) while leaving `active` and the local leg's observation
untouched. That is a different, broader change than the bounded
quiet/max-hold/admission-interval and event-type filter knobs this task
implemented, and touching it risks the exact kind of redesign this task was
explicitly told to avoid. No live subscription was manipulated to investigate
or work around this — the manager's `local_only` re-subscription above was
its own prior operational action, not something performed by this task.

## What a real fix would need

- Per-machine fault state on the subscription row (or in the batch) that
  marks one peer's leg stale without setting the whole-batch `watchError`/
  `active=false` that a local-only or otherwise-healthy leg does not deserve.
- A way for `kanna_read_event_subscription`'s compact response to surface
  "this repo-scoped watch lost peer X, still reading everything else" as a
  qualitatively different state than "this watch is fully paused" — today's
  `error`/`active` pair cannot express that distinction.
- Regression coverage proving one peer's fault leaves every other peer's
  (and the local leg's) delivery cadence unaffected, alongside the existing
  all-legs-paused tests, which should keep asserting today's behavior for any
  scope that has no healthy alternative leg (e.g. a `local_only` or
  single-peer subscription still gets no free pass).

## Narrower coverage in the meantime

None added by this task. The existing tests above continue to encode the
current (blast-radius-including-local) behavior as expected; a `local_only`
subscription remains the operator's only current mitigation for a
repo/parent-scoped watch that keeps re-hitting one bad peer.
