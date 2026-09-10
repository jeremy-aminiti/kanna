# Repo-scoped subscription: one remote peer's outage no longer pauses every leg

## Incident

An MBP lost DHCP on hotel Wi-Fi. The manager's repo-scoped subscription
(`watch-1788966846045349000-0`) fanned observation out across machines; once
the MBP became unreachable, the whole subscription surfaced `watchError` and
paused on acknowledgement — including this machine's own, otherwise-healthy
local observation. The manager worked around it with a temporary
`local_only: true` subscription (`watch-1789035297611759000-0`).

## Root cause: two short-circuits, not one

A prior investigation (see the sibling subscription-tuning task's now-stale
`docs/2026-09-10-repo-subscription-remote-fault-pauses-all-legs-limitation.md`)
identified `accept_page`'s blanket `machineErrors` non-empty check
(`crates/kanna-server/src/http_api/event_subscriptions.rs`) as the cause: any
non-empty `machineErrors` synthesized a whole-batch `watchError`, and `read`
deactivated the subscription on acknowledging it. That is real, but it is
only half the story — changing that condition alone would not have been
sufficient, matching this task's mandate to verify independently.

`wait_aggregate_task_events` (`crates/kanna-server/src/http_api/task_events.rs`)
has its own, separate short-circuit for the subscription/mailbox path
(`query.subscription_timing`):

- A loop-top check broke out **before spawning any leg at all**, including
  the local one, whenever `machine_errors` was already non-empty (the common
  case once a peer is known unreachable). This is the actual mechanism that
  denied the local leg the chance to collect its own events during an
  outage — not just a policy decision made after the fact in `accept_page`.
- A `batch_complete` OR-condition separately ended the wait the instant
  *any* leg (even a fast-failing remote one) reported an error, cutting a
  still-in-progress healthy leg's normal wait short mid-call.

Fixing only `accept_page` would have left both of these in place: every
mailbox cycle would still return almost instantly with an empty,
error-only batch for the down peer, and (worse, if `accept_page`'s pause were
simply removed without addressing this) that would have produced a tight,
repeated wake cycle instead of a paused subscription — trading one bug for
another.

## Fix

Both short-circuits, and `accept_page`'s pause decision, are now scoped to
**this machine's own leg**, not any peer:

- `wait_aggregate_task_events`: the fast-surface break and the
  `batch_complete` OR both trigger only when `machineErrors` contains an
  entry for the local machine id. A remote-only fault lets the remaining
  active machines (including local) run their full, normal collection/quiet/
  max-hold cycle, exactly as if the peer were healthy but silent.
- `accept_page`: only a local-attributed fault (or an already-explicit
  `watchError`, e.g. an invalid cursor) still fails the whole subscription.
  Remote-only faults are tracked on the row (`EventSubscription.stale_machines`,
  machine id -> last error) and diffed against each fresh page: an unchanged,
  already-reported fault does not create a new pending batch or wake, while a
  new fault, a recovery, or real events (with the fault riding along as an
  annotation) do.

The down peer's own `cursorsByMachine` entry was already left untouched by
`apply_aggregate_completion` on failure — that part of the existing design
was correct and needed no change. Recovery is automatic: once the peer is
reachable again, its own leg is retried on the mailbox's normal cadence (once
per collection cycle, not an independent poller) and resumes from its
preserved checkpoint.

No new ownership boundary was introduced. The fix lives entirely inside the
existing `EventSubscription` row (one additive `#[serde(default)]` field,
restart-durable the same way as `wake_admitted`) and the existing aggregate
wait/mailbox worker; scheduling, admission pacing and relay long-poll
lifecycle are unchanged.

## Coverage

`crates/kanna-server/src/http_api/tests/task_events/subscription_remote.rs`:
`subscription_remote_outage_isolates_to_that_leg_and_recovers` replaces
`subscription_busy_peer_pause_and_same_id_recovery_preserve_checkpoint`
(which asserted the old pause-and-resubscribe behavior as correct) and
exercises, against the production aggregation/mailbox/relay-fixture stack:
local delivery and acknowledgement continuing with the peer down, the peer's
checkpoint surviving across two acks, a notification storm with an unchanged
fault not manufacturing a fresh wake, and recovery replaying the peer's
backlog from its preserved checkpoint with no unsubscribe/resubscribe.

`subscription_watch_failure_is_a_readable_attention_batch_and_does_not_spin`
(`crates/kanna-server/src/http_api/tests/task_events.rs`) is unchanged and
still passes: a `local_only` subscription with an invalid cursor is a local
fault and still gets no free pass.

## Reconciliation note

This task's source changes were written against `main` at
`90fd52ee401dfcc900174d9564e2d9c388bec275`, before the subscription-tuning
task's timing/API changes (task `5edc81f8`, committed at `ed906ff53`) merge.
Once that merges, the sibling's
`docs/2026-09-10-repo-subscription-remote-fault-pauses-all-legs-limitation.md`
is resolved by this change and should be removed or marked resolved, and this
task's new test's timing assumptions (1s quiet / 5s max hold, from the
pre-tuning `subscription_timing.rs` constants) need re-validation against
whatever the merged timing constants become.
