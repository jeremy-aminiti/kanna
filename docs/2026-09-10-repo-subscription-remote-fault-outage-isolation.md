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

## Cursor rejection stays a hard, actionable pause

A remote peer rejecting its own embedded checkpoint (an expired/invalid
native cursor) is a distinct failure from peer unavailability, by
construction rather than by an added check: `apply_aggregate_completion`
classifies it as `AggregateMachineWaitError::CursorRejected` and returns a
hard `Err` from `wait_aggregate_task_events` *before* it ever reaches
`machineErrors`. That propagates through `wait_subscription_events` and
`collect()` to `step()`'s pre-existing catch-all `Err` branch, which builds
an explicit `batch["watchError"]` directly — a code path `accept_page`'s
`local_faulted`/`coverage_changed` leniency never touches, since
`machineErrors` is empty in that batch. A remote cursor rejection therefore
still fails the whole subscription (`active` becomes `false`), the poisoned
checkpoint is left exactly as-is (never reset to "now"), and the worker
stops entirely — no retry loop, matching the pre-existing
`an_aggregate_leg_with_a_rejected_cursor_is_asked_once_per_poll` guarantee at
the raw-endpoint layer. This needed no source change; it is covered by
`subscription_remote_cursor_rejection_remains_a_hard_pause_distinct_from_outage`.

## Dedup is by machine id, not by error text

The first cut of `accept_page`'s de-duplication compared the full
`{machineId: error}` map for equality. Tracing the actual error-text
producers found this unsafe for the most common real case:
`AppState::desktop_routing_unreachable_error` (used whenever a peer is
simply absent from `list_active_relay_desktops`'s result — i.e. this
machine's own relay routing is healthy, only the peer isn't currently
listed) embeds a `since` timestamp that is only pinned stable while *this*
machine's own routing is the thing marked unavailable
(`set_desktop_routing_unavailable`). For a peer that is merely absent while
local routing stays healthy, `since` is never pinned, so every call mints a
fresh `unix:<now>` string — comparing full text would have treated that
natural churn as a new fault every mailbox cycle and reintroduced the exact
wake flood this task exists to prevent, for what is likely the single most
common manifestation of the fix's own target scenario. `coverage_changed` now
compares only the `BTreeMap`'s key set (`remote_errors.keys().eq(...)`); the
latest text is still stored unconditionally so a status read reports the
current reason. Covered by
`unchanged_remote_fault_does_not_rewake_even_as_its_text_churns` and
`stale_machines_and_its_dedup_survive_a_reload_from_the_durable_row` (inline
unit tests in `event_subscriptions.rs`, the latter also proving
`stale_machines` and its dedup survive a fresh row reload from the same
durable JSON storage `cursor`/`active`/`wake_admitted` already rely on).

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

## Verification (2026-09-10)

Ran `crates/kanna-server`'s `task_events::` suite (107 tests — covers
`subscription_remote`, `subscription_relevance`, `subscription_timing`, and
the raw aggregate/cursor-rejection tests) and the two new
`event_subscriptions::outage_isolation_tests` unit tests at
`CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1`, plus a scoped
`cargo clippy -p kanna-server --tests --bin kanna-server -- -D warnings` and
`cargo fmt --all -- --check`. All green as of commit `5fc54e2dd`.

Two test bugs were found and fixed along the way (production code
unaffected by either):

- `subscription_remote_cursor_rejection_remains_a_hard_pause_distinct_from_outage`
  originally corrupted an *already-established* subscription's peer cursor.
  By then the worker had already admitted a real long-poll to the peer with
  the valid cursor; abandoning that in-flight leg (via the row-mismatch
  revalidation) does not release its relay permit until its own deadline —
  the same mechanic `subscription_retirement_abandons_one_leg_until_peer_deadline`
  documents. The next attempt hit a busy-503 instead of ever reaching the
  peer's cursor validation, so the test just hung until its own timeout.
  Fixed by adding `WatchFixture::new_with_poisoned_peer_cursor`, which
  corrupts the peer's native cursor before the worker ever spawns.
- The durable-row-reload unit test inserted an `EventSubscription` row
  without first inserting its referenced `pipeline_item`, tripping the
  `task_id -> pipeline_item` foreign key the schema already enforces.

## Reconciliation note

This task's source changes were written against `main` at
`90fd52ee401dfcc900174d9564e2d9c388bec275`, before the subscription-tuning
task's timing/API changes (task `5edc81f8`) merge. **`02b65d2af` is not that
task's finished/accepted head** — as of this writing `5edc81f8` is actively
fixing its own review defects (legacy default query compatibility, the 240s
collection cap, and compact/diagnostic contract tests) on top of it. Its diff
was read in full for *awareness and planning* only; nothing from it has been
merged or copied into this branch, and its exact shape will change before it
actually lands. Reconciliation must re-read `5edc81f8`'s final, accepted
commit — not reuse this snapshot — before touching any shared file. As read
at `02b65d2af`, the reconciliation surface was:

- `accept_page`'s outage logic is untouched there — confirms it was
  deliberately deferred to this task, as its own limitation doc says.
- `compact()`/`response(row, diagnostic)` (new there) will need my
  `local_machine_id` parameter on `accept_page` merged in; independent edits
  to the same functions/call sites, not a design conflict.
- `Collection::from_query(query.quiet_ms, query.max_hold_ms)` (new there)
  sits on lines adjacent to my `local_machine_faulted` closure and scoped
  break/OR conditions in `wait_aggregate_task_events`/`wait_local_task_events`
  — an expected adjacent-line merge conflict, orthogonal in effect (theirs
  governs collection duration, mine governs early-termination scope).
- `WatchFixture::request()`/`ack()` (shared by my two new tests) gained
  `diagnostic:true` plus `quietMs:2000`/`maxHoldMs:10000` overrides, and
  `until()`'s budget grew from 2s to 400s simulated — my tests should
  inherit this cleanly since they use the same helpers, but must be
  re-run once merged rather than assumed to still pass.
- `catalog.json`'s `kanna_read_event_subscription` description was rewritten
  there (compact-response documentation) — my `staleMachines`/outage-behavior
  sentence will need re-inserting into whatever their final text becomes, not
  restored verbatim.

### Resolved: compact response must carry stale-machine coverage

The previously-flagged open decision ("should `compact()` expose
`staleMachines`?") is resolved — decided, not left to this task to invent:

- The compact response must report **current** stale-machine coverage even
  when `pending` is `null` — an operator reading a quiet subscription must
  be able to see a degraded peer without needing a delivered batch to carry
  it, and without `diagnostic: true`.
- A delivered batch (`pending` non-null) continues to carry `machineErrors`
  as it already does.
- Neither must reintroduce a giant/cursor-shaped blob into the compact
  response — the point of `compact()` (stripping the durable cursor) stands;
  whatever field carries this is a small, summarized shape (e.g. machine id
  plus reason), not `stale_machines` embedded verbatim if that ever grows
  cursor-like, and never the durable `cursor` itself.

**This task owns implementing that integration**, after `5edc81f8`'s actual
final commit merges: add the compact-mode field sourced from
`row.stale_machines`, and cover it with tests exercising ack, restart, and
peer recovery through the compact response specifically (not just the
existing diagnostic-mode assertions). Until then, `stale_machines` remains
correctly persisted and deduped server-side (see the two
`event_subscriptions::outage_isolation_tests` unit tests) but is only
visible via `diagnostic: true` or a delivered batch's `machineErrors` — a
known, tracked, non-final gap, not a defect in the outage-isolation logic
itself.

Once `5edc81f8`'s final commit merges, the sibling's
`docs/2026-09-10-repo-subscription-remote-fault-pauses-all-legs-limitation.md`
is resolved by this change and should be removed or marked resolved, and this
task's new tests' timing assumptions (1s quiet / 5s max hold, from the
pre-tuning `subscription_timing.rs` constants used at verification time) need
re-validation against the merged timing constants (300s/300s/60s defaults at
time of reading, though `5edc81f8` is still revising the 240s collection cap
too — re-check rather than assume), mitigated for these specific fixtures by
the per-subscription overrides `WatchFixture` will carry post-merge.

Current `main` was revalidated at `3f9520ae2` (Android emulator pairing
terminal work): touches no file this task shares (`apps/mobile/**`,
`crates/kanna-server/src/http_api/ksp.rs`, `tools/kd/**`); this task's fork
point `90fd52ee4` still applies with no conflicts.
