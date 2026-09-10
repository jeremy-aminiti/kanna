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

## Revision round 1: required integration coverage

Review found the prior round's coverage insufficient in two specific ways
and required closing both before further verification:

1. **The pre-spawn `machineErrors` path was never exercised.**
   `subscription_remote_outage_isolates_to_that_leg_and_recovers` only used
   a busy/503 leg (`Unavailable` via an exhausted relay permit), which is a
   different `apply_aggregate_completion` branch than a peer simply absent
   from `ListActive` — the actual MBP-dropped-WiFi shape, and the one the
   `wait_aggregate_task_events` loop-top guard at `task_events.rs:2389`
   (`local_machine_faulted`) exists to keep from short-circuiting local/
   sibling collection. Added `WatchFixture::new_with_healthy_sibling_and_
   excluded_peer` (three machines: source, a healthy sibling reusing the
   existing `peer`/`page`/`ack` machinery, and a peer excluded from
   `ListActive` *before* the worker's first real cycle — established at
   bootstrap, then dropped, never admitted-then-abandoned) and
   `connect_repo_peers` (routes `Invoke` by `desktop_id`, `ListActive` reads
   a mutable, test-controlled roster). New test:
   `subscription_remote_outage_with_a_healthy_sibling_isolates_and_recovers`
   — repeated local delivery/ACKs, the excluded peer's exact checkpoint
   in scope and untouched, the healthy sibling's admission/abandon counts
   unaffected, and recovery replaying from the preserved checkpoint.

2. **No restart regression, and the unit test's dedup coverage was too
   narrow.** The `event_subscriptions::outage_isolation_tests` row-reload
   unit test calls `accept_page` directly against a synthetic batch and a
   reopened `Db` handle — it never restarts the worker or the aggregate-
   wait registry, so it cannot prove restart *resumption*. It remains as
   narrower, non-integration coverage of the dedup logic itself. Added
   `WatchFixture::restart` (drops the fixture — aborting the old worker and
   relay via `Drop` — then rebuilds a fresh `AppState` from the same
   persisted DB path, so no in-memory registry/admission-clock/relay state
   survives) and
   `subscription_remote_outage_survives_a_server_restart_and_recovers`:
   several real ACKs, then a restart, verifying the excluded peer's exact
   checkpoint and `stale_machines` entry are readable from the durable row
   alone immediately after, then several full 240s-timeout quiet collection
   cycles post-restart (whose `machineErrors` text genuinely changes call to
   call — `AppState::desktop_routing_unreachable_error` mints a fresh
   `unix:<now>` string whenever this machine's own routing stays healthy,
   confirmed by reading its implementation) asserting no new pending
   batch/`batchId`, then recovery replaying the backlog.

Both are integration tests through the production relay-fixture/worker/
mailbox seam, per the review's requirement — no live network/peer
manipulation, no change to `accept_page`'s or `wait_aggregate_task_events`'s
logic itself. Written and reasoned through carefully but **not compiled or
run**: this revision round's instructions hold builds/tests until the
manager releases them, same as the prior round. `admitted`/`abandoned`/
`busy` semaphore-based assertions reuse `connect`'s exact proven mechanics
(just routed to multiple backing states); the "attempts stays at 1 across
several acked local-only rounds" assertion mirrors
`subscription_notifications_and_ack_retain_one_remote_wait`'s own, already-
passing assertion for the two-machine case. Admission-pacing instrumentation
(`subscription_timing`'s `TestEvent::Admitted`) was not reused — both new
tests use `delivery: "poll"`, which never reaches that code path at all (see
`event_subscriptions.rs`'s `step()`); the review's "if testing actual wake
delivery" phrasing reads as conditional, and switching delivery modes would
need a fake PTY/daemon session neither fixture sets up. Flagged here rather
than done, in case reconciliation wants it revisited.

## Integration readiness against tuning's accepted head (2026-09-10)

Tuning's source review was accepted at `5226be1cb` (172 tests), superseding
the `02b65d2af` snapshot read earlier — its diff against `90fd52ee4` was read
in full for this pass. It is now merging `origin/main` into its own branch
(`task-5edc81f8-5` at `27cf52c364`); `origin/main` is unchanged at
`3f9520ae2`. Nothing from it is applied to this branch — the actual file
merge happens after it lands on `main`, per this task's standing
instructions — but its shape is now stable enough to plan the merge
precisely instead of provisionally.

### What changed since `02b65d2af` that touches this task's files

`event_subscriptions.rs`'s `step()` was substantially rewritten: one native
`collect()` call is capped at `MAX_WAIT_TIMEOUT_SECS` (240s), but a
subscription's own quiet/max-hold window can now exceed that (300s defaults,
or an arbitrary per-subscription override), so `step()` now chains multiple
native calls (a `'chain: loop`) sharing one `subscription_timing::Collection`
instance across them, re-reading its *live* `intrinsic_deadline()` after each
call rather than trusting a pre-call snapshot — this is the "240s collection
cap" defect the correction notice named. `Collection::ready` dropped its
`receiver`/`deadline` parameter (now just `(count, capacity, now)`); the
subscription/aggregate wait's own `receiver`-based capping is a separate,
unchanged concern. `wait_subscription_events` gained a third parameter, the
shared `Arc<Mutex<Collection>>`.

### Confirmed compatible, no redesign needed

- The chain's own stop condition — `if batch["waitOutcome"] == "timeout" &&
  !machine_errors_present { continue 'chain }` — does **not** chain past one
  240s native call when `machineErrors` is present, for *any* machine,
  local or remote. That means a remote-only fault (this task's isolation
  case) still gets exactly one ~240s cycle per `step()` iteration under the
  new code, matching what
  `subscription_remote_outage_survives_a_server_restart_and_recovers`'s
  "advance `MAX_WAIT_TIMEOUT_SECS` per idle cycle" loop already assumes. No
  correction needed there; verified by reading the new code, not assumed.
- This task's `wait_aggregate_task_events` loop-top guard (`task_events.rs`
  near line 2389, `local_machine_faulted`) sits on lines tuning's diff does
  not touch at all — a clean, non-adjacent merge.
- `Collection::intrinsic_deadline`/chain-level `machineErrors` handling and
  this task's `stale_machines` key-only dedup operate at different layers
  (native-call sizing vs. whether `accept_page` wakes the subscriber) and do
  not interact.

### Two exact merge points (mechanical, not semantic)

1. `wait_aggregate_task_events`'s `batch_complete` line: tuning's
   `with_collection(&query, |c| c.ready(events.len(), limit, now)) ||
   !machine_errors.is_empty()` needs `!machine_errors.is_empty()` replaced
   with `local_machine_faulted(&machine_errors)` (same substitution this
   task already made against the pre-tuning signature) — a parameter-count
   adjustment (`ready` dropped `deadline`) plus the existing substitution,
   nothing new.
2. `event_subscriptions.rs`'s `accept_page` call sites: tuning's `step()`
   restructuring calls `accept_page(&mut row, batch, false)` (still 3-arg,
   untouched by tuning) once, after the `'chain` loop produces a final
   `batch`; this task's `accept_page` takes a 4th `local_machine_id`
   parameter and has its own local/remote-fault body. Combine by keeping
   tuning's chain structure and threading this task's 4-arg call/body
   through its single post-chain call site (and the `subscribe()` bootstrap
   call site, similarly unmoved).

### Stale-machine compact coverage: ready to apply once `compact()` exists

Resolved design (recorded in the prior section) needs one field added to
`event_subscriptions.rs`'s `compact()`, once it exists on this branch:

```rust
json!({
    "id": row.id,
    "active": row.active,
    "error": row.error,
    "wakeState": row.wake_state,
    "batchId": row.batch_id,
    "staleMachines": row.stale_machines,   // <- new, top-level, visible even when pending is null
    "pending": pending,
    "query": scope,
})
```

`row.stale_machines: BTreeMap<String, String>` serializes directly as a
`{machineId: reason}` object — already the small, bounded, non-cursor shape
the resolved decision required (at most one entry per currently-stale peer);
no new type or summarization needed. `pending.machineErrors` is untouched
(still the per-batch diagnostic array). This is source-ready but not
applied: `compact()` is tuning's function and does not exist on this branch
before its merge lands.

### Test inventory (unchanged head, all logically re-verified against
### `5226be1cb`, none run — capacity, not authorization, is currently the
### blocker: Studio at ~90.5% busy)

- `crates/kanna-server/src/http_api/event_subscriptions.rs` —
  `outage_isolation_tests::unchanged_remote_fault_does_not_rewake_even_as_its_text_churns`,
  `outage_isolation_tests::stale_machines_and_its_dedup_survive_a_reload_from_the_durable_row`
  (unit-level, call `accept_page`/`Db` directly — unaffected by any of the
  `step()`/`Collection` changes above).
- `crates/kanna-server/src/http_api/tests/task_events/subscription_remote.rs` —
  `subscription_remote_outage_isolates_to_that_leg_and_recovers`,
  `subscription_remote_cursor_rejection_remains_a_hard_pause_distinct_from_outage`,
  `subscription_remote_outage_with_a_healthy_sibling_isolates_and_recovers`,
  `subscription_remote_outage_survives_a_server_restart_and_recovers`
  (integration-level, through the real relay/worker/mailbox seam).

Focused command once capacity allows (unchanged from the prior round):
`CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test -p kanna-server --bin
kanna-server task_events:: -- --test-threads=1`, then the
`event_subscriptions::outage_isolation_tests` filter.

## Merge complete: tuning's accepted checkpoint (2026-09-10)

Authorized and merged: `task-5edc81f8-5` at `27cf52c364adf80dbf1ec6bdb5053dfcea71496d`
(tuning's source review accepted head `5226be1cb`, already carrying a
conflict-free merge of `origin/main` `3f9520ae2`) is now part of this
branch's history (merge commit `3f9ba7998`). Four real conflicts, resolved
by hand and verified by careful reading (not compilation — no cargo launch
this round, per standing capacity instructions): the bootstrap `collect()`/
`accept_page` call site and `batch_complete`'s OR-condition (both simple,
anticipated substitutions against signature changes), a two-line comment
above `until()`, and `kanna_read_event_subscription`'s catalog description.
Everything else — `step()`'s full native-call-chain restructuring, the new
`WatchFixture::request_with`/`new_with`, tuning's own two new tests, all of
this task's own additions (`connect_repo_peers`,
`new_with_healthy_sibling_and_excluded_peer`, `restart`, both integration
tests) — merged with **zero** textual conflict; read in full afterward to
confirm the combination is semantically correct, not merely absent of
markers.

The previously-deferred compact-response decision is now real code, not
just a plan: `compact()` gained a top-level `staleMachines` field
(`row.stale_machines`, unconditionally visible, even when `pending` is
`null`). `docs/kanna-server-boundary.md` — the canonical, AGENTS.md-listed
doc for this surface — now documents both the compact-response addition and
(newly, since it had never been recorded there before) the outage-isolation
contract itself: per-peer fault isolation, the local-vs-remote distinction,
`stale_machines`/`staleMachines` de-duplication by machine id, and
checkpoint preservation across recovery.

Sibling's `docs/2026-09-10-repo-subscription-remote-fault-pauses-all-legs-limitation.md`
landed via the merge (it now exists on this branch, historical/untouched);
it describes the bug this task's merged work resolves and is safe to
remove or mark resolved in a later, separate pass — not done here to avoid
editing a doc this task did not author beyond what reconciliation required.

Head is `3f9ba7998`. Still not run: `cargo build`/`test`/`clippy`/`fmt` on
the merged tree — everything above is verified by manual reading only.

## Revision round 2: recovery required positive confirmation, not silence

Review traced an ordering bug in the merged `accept_page`: it replaced
`row.stale_machines` wholesale with one page's `machineErrors` every call.
`machineErrors` is per-call and only lists discovery-excluded machines and
legs that actually completed (failed) in that call — `wait_aggregate_task_events`
can seal a batch on this machine's own urgent/full/quiet criteria while a
listed peer's own retained leg is still pending in the registry, never
having completed at all. That peer then appears in neither list. The old
code read that silence as recovery, clearing stale coverage with zero
evidence; the peer's eventual (still-failing) completion would then read as
a brand new coverage change and mint another error-only wake for the same
continuous outage — reintroducing, one layer down, exactly the wake flood
this task exists to prevent.

Fix: `wait_aggregate_task_events` now reports `confirmedMachines` — machines
whose own leg positively, successfully completed this call, including an
empty response whose checkpoint does not move — alongside `machineErrors`.
`accept_page` reconciles instead of replacing: start from the durable set,
apply this call's fresh/updated faults, and clear only machines this call
positively confirmed. A machine in neither list is left exactly as it was.

Tracing this through `step`'s real native-call chain (added by tuning to
honor a quiet/max-hold window larger than one 240s native call) surfaced a
second, related gap purely from adding a new field to a response the chain
already partially discards: the chain's `continue 'chain` path only
explicitly carries `retained_events` forward, so a peer's positive-but-empty
confirmation from an earlier chained call would be silently dropped the
moment the chain moved on to a fresh one — and, separately, nothing
previously gave the chain a reason to stop *early* for a confirmation the
way a fresh failure already does, so a recovered-but-otherwise-quiet
subscription could sit chaining natively forever without ever reporting it.
Fixed both in `step`, mirroring `retained_events`'s own existing pattern
rather than inventing a new mechanism: accumulate `confirmedMachines` across
chained calls (dropping any machine this call's `machineErrors` just
re-failed, so the most recent signal always wins), and stop the chain early
specifically when an accumulated confirmation matches a machine the
subscription currently has recorded stale — never for an ordinary healthy
peer succeeding, which remains free to keep the chain running its full
quiet/max-hold window exactly as before.

Also completed, per the review's second finding: the compact `staleMachines`
contract is now exercised through actual (non-diagnostic) HTTP reads in all
three outage/restart integration tests — visible with `pending: null` after
multiple real acks, present in `pending.machineErrors` while a batch exists,
cleared on confirmed recovery, and the compact-response-omits-cursors
contract asserted on every such read — including specifically after the
restart test's fresh `AppState` rebuild, the scenario the review named
explicitly.

New integration coverage:
`subscription_remote_stale_coverage_survives_a_pending_leg_until_positive_recovery`
(`subscription_remote.rs`), driven through a new `connect_gated_peer`
fixture whose long polls the test resolves explicitly and deterministically
(a specific failure, held genuinely unresolved, another failure, then a
specific success) rather than relying on a real peer round trip — proving
stale coverage survives an unresolved leg and a second failure untouched, no
fault-only batch is minted while nothing changed, the checkpoint never
moves, no extra admission or abandonment touches the retained request, and
confirmed recovery wakes exactly once. The existing
`event_subscriptions::outage_isolation_tests::unchanged_remote_fault_does_not_rewake_even_as_its_text_churns`
unit test was corrected to require the same explicit `confirmedMachines`
evidence for its own recovery step (it previously inferred recovery from an
empty `machineErrors` alone, the exact semantic the review found wrong) and
extended with an explicit still-pending-leg case at the unit level.

Not run: `cargo build`/`test`/`clippy`/`fmt` — this round's instructions
hold builds/tests until released, same as prior rounds. Verified by careful
manual reading only, including full re-derivation of `step`'s native-call
chain timing to confirm the new integration test's synchronization points
are reachable without hanging (documented inline in the test itself).

## Round 3: same-native-call ordering, and a test fixture that never ran

Round 2's `confirmedMachines` fix handled the ACROSS-chained-native-calls
case (`step`'s `'chain: loop`, most recent call wins) but missed the
identical race one layer down, WITHIN one native call. `wait_aggregate_task_events`
re-arms a machine's just-completed leg whenever the batch it is filling
isn't done yet (task_events.rs's re-arm block, pre-existing from the tuning
task) — so a peer can complete twice in the same call: once successfully,
then again with a failure once its re-armed leg also resolves.
`apply_aggregate_completion`'s success branch only ever *inserted* into
`confirmed_machines`; its `Unavailable` branch never removed the earlier
insertion. So that peer came back in both `confirmedMachines` and
`machineErrors` on the very same response, and `accept_page`'s
add-fault-then-clear-confirmed processing order let the stale confirmation
silently erase the real fault — reporting a peer that had just failed as
healthy. Fix: the `Unavailable` branch now also does
`confirmed_machines.remove(&completion.machine_id)` before recording the
fault, so the most recent completion is authoritative within a native call
exactly as `step`'s chain already made it across calls. Once source-level
`confirmedMachines`/`machineErrors` are mutually exclusive per machine, per
call, `accept_page`'s existing add-then-remove order needs no change — traced
through, not assumed, since a machine can no longer appear in both lists at
once for it to matter.

New integration coverage:
`subscription_remote_same_call_success_then_failure_keeps_the_peer_stale`
(`subscription_remote.rs`) forces exactly that ordering deterministically —
using a single non-urgent first event (see `subscription_timing::urgent`) so
the subscription's own collector (`Collection::ready`) does not seal the
batch on its own, guaranteeing the re-arm rather than racing for it — then
resolves the re-armed leg with a failure in the same call. Proves: the peer
reads stale through both the diagnostic row and a plain compact read, and
after ACK; the earlier successful event and its checkpoint are retained, not
lost to the later fault; and a subsequent unchanged failure for the same
peer does not mint another coverage-only wake.

Also repaired: `GatedInvoke.response.send(...)`, used throughout the Round 2
gated-peer test, was a *partial move* of only the `response` field —
`_permit` survived it and stayed alive until the enclosing `let` binding's
scope ended (the whole test function), not when the response actually
resolved. Under `connect_gated_peer`'s one-permit budget that starves every
dispatch after the first resolved one with a synthetic 503, so the test as
written would never actually reach its second gated leg. Fixed by giving
`GatedInvoke` a `resolve(self, ...)` method that destructures itself,
explicitly drops the permit, and only then sends the response — releasing
admission at the real completion boundary instead of at scope-end — and
updating every call site (including the new Round 3 test) to use it. Also
bounded every `gate.recv()` wait with a new `recv_gated` helper (mirrors
`until`'s virtual-clock budget) so a fixture regression fails the test
instead of hanging the suite, and corrected the existing gated test's final
admission-count assertion: the third leg's own non-terminal success carried
no events, and a subscription's collector requires at least one observed
event before it will seal a batch at all (urgent or not), so it legitimately
triggers one more re-armed dispatch before that native call concludes, which
the assertion now explicitly bound-waits for and accounts for (4 attempts,
not 3) rather than forbidding — without touching production re-arming to
satisfy the fixture.

Not run: `cargo build`/`test`/`clippy`/`fmt` — still held this round per
instructions. Verified by manual tracing of `apply_aggregate_completion`,
the re-arm block, `wait_aggregate_task_events`'s collection loop exit
conditions, and `GatedInvoke`'s exact partial-move semantics.

## Round 4: the Round 3 test never actually exercised the ordering it claimed

Independent review caught two defects in Round 3's new regression test that
manual tracing alone had missed, both stemming from one wrong assumption:
that the public wait's `minEvents`/`task_event_batch_is_complete` governs
subscription batch completion. It does not — `SubscribeRequest`
(`event_subscriptions.rs`) has `deny_unknown_fields` and has never had a
`minEvents` field; a subscription's own collector
(`subscription_timing::Collection::ready`, selected by
`query.subscription_timing`) governs completion instead. Concretely:

1. The test's subscribe POST included `"minEvents": 2`, an unknown field
   `SubscribeRequest` rejects outright — the registration itself would have
   failed with 400 before the worker ever started, so the test as written
   could not have run at all.
2. Even with that field removed, the test's crafted event used
   `task.awaiting_input`, which `subscription_timing::urgent` treats as
   urgent — `Collection::ready` seals a batch immediately on any urgent
   event, so the very first success would have completed the batch on its
   own and the re-arm the test exists to exercise would never have happened.

Fixed by using `WatchFixture::request()`'s existing, already-supported
`quietMs`/`maxHoldMs` overrides (no new field added) and switching the
crafted event to `task.pr_created`, which `subscription_timing::urgent`
explicitly treats as non-urgent — so a single successful event leaves
`Collection::ready` false (no urgency, under capacity, quiet/max-hold not yet
reached) until the subscription's own window elapses, guaranteeing the
re-arm deterministically the same way `minEvents` would have on the public
wait, without needing or adding any such field on subscriptions. Added an
explicit deterministic proof that the re-armed second leg is dispatched
before any batch is sealed (`wake_state != "ready"` at that point), directly
proving the success and the subsequent failure land in the same native
collection rather than inferring it.

Also corrected in the same test: the `confirmedMachines` assertion after the
failure now checks the peer's own id is absent from that list, rather than
requiring the whole list to be empty — an unrelated healthy machine (the
local leg, say) completing successfully within the same call may legitimately
appear there too, and the fix under test only concerns the peer's own
entry. And the final "no new batch from an unchanged failure" check
previously synchronized on `attempts >= 3`, a condition already true the
moment the third request is *received* (admission is counted at dispatch,
not completion) — so it proved nothing about that outage collection having
actually settled through `accept_page` before the assertion ran. Fixed by
synchronizing on the *next* (fourth) admission instead, which can only be
dispatched after the third leg's own native call fully concludes and the
worker starts its next cycle.

The sibling Round 3 test's own trailing comment (why the third leg's empty
success still triggers one more re-arm) also cited the same wrong
`minEvents` premise; corrected to cite `Collection::ready`'s actual
`count > 0` requirement — the code and the passing assertion were already
correct, only the stated reason was wrong.

No production code changed this round; no new API field was added. Not run:
`cargo build`/`test`/`clippy`/`fmt` — still held. Verified by manual tracing
of `SubscribeRequest`'s `deny_unknown_fields`, `subscription_timing::urgent`
and `Collection::ready`, and the worker's `step`/`accept_page` sequencing
that makes the fourth-admission synchronization sound.
