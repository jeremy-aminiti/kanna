# Event subscription remote permit ownership: evidence and coverage gap

Base: `7f54ddbfa7e1d3cc44278ea28c10840b3340229d` (`HEAD` and the local
`origin/main` ref verified at task start). Incident runtime: staging.14 on
Studio and MBP. Task: `2c7a34b9`; architect advisory: `8d6bad1d`.

## Trace and bounded change

The subscription worker raced every server state notification and every
subscription change notification against `collect(..., 240)`. Notifications
cancelled the aggregate even when this subscription's owner, query, and
checkpoint had not changed. The aggregate removes its pending-leg session
from the cursor-keyed registry while running and restores it only on normal
completion. Cancellation drops its JoinSet. The remote leg's cancellation
only drops the local response receiver; an admitted relay HTTP handler on the
peer keeps its long-poll permit until its own completion or timeout. Repeating
that sequence can consume the peer's long-poll budget while ordinary routes
remain healthy on their separate short-request budget.

The change pins that collect across notifications and revalidates the durable
row (existence, active state, revision, pending page, query, cursor) and owner
run/stage/branch binding. Actual retirement or a changed row still interrupts
observation. Mailbox accept/ack remains the checkpoint authority. Cancelled
aggregate sessions are deliberately not reinserted: they can contain advanced
checkpoints whose events have not reached a mailbox.

Retirement can abandon at most one in-flight leg per peer for that retirement.
Its receiver retains the existing timeout, at most `MAX_WAIT_TIMEOUT_SECS`
(240 seconds). Repeated genuine retirements can overlap several such holds
within that window; there is no universal historical one-per-subscription
permit bound. A completed aggregate left in the registry retains the existing
10-minute registry TTL. This patch does not add remote cancellation or change
those lifecycle limits.

This is a subscriber-side server fix. A fixed Studio can retain its waits
against an unfixed MBP; fixing only the receiving MBP does not fix an unfixed
Studio producer. No relay service deployment, DB migration, authentication,
wire-format, or timeout change is involved. Subscriptions call the server's
aggregate directly with `shortCursor:false`: `ks1` contains per-machine `ke1`
checkpoints around native cursors, including `kc1` current-state progress.
MCP's legacy `km1` task-id fan-in is not in this subscription path; null MCP
task context does not erase the explicitly supplied repository scope.

## Live observations are not branch verification

The manager reported one durable repository watch, registration retries using
its existing id/settings, repeated MBP 503 machineErrors/watchError pages, and
healthy ordinary info/list/stats routes. A bounded MBP log read found bursts
of same-repository, same-cursor `GET /v1/task-events` with timeout 240, including
request ids 140, 142, 144, 146 through 173 and probe 175. Those logs did not
record cancellation, permit release, active counts, or watcher ownership.
They cannot distinguish orphaned requests from independent simultaneous
callers. The code trace strongly supports the cancellation mechanism; it does
not retrospectively identify every live request's owner.

After batch 13 was acknowledged and the watch paused, a timeout=1 probe near
15:37:50 UTC returned a normal timeout after earlier admitted waits could
expire. Same-id/settings registration retained the cursor and resumed without
a restart. Batch 16 subsequently reported another MBP 503. After acknowledgement
paused that watch, a single timeout=1 probe at 15:48:53 UTC still returned 503;
a bounded probe at 15:50:57 UTC returned a normal empty timeout. Registration
again retained the cursor. No unsubscribe, cursor reset, server restart, or
live stress was reported in these recoveries. This timing supports transient
permit occupancy, not cursor loss or a CPU-utilization threshold.

## Added wiring regressions

`crates/kanna-server/src/http_api/tests/task_events/subscription_remote.rs`
adds three integration tests:

- `subscription_notifications_and_ack_retain_one_remote_wait`: a repository
  subscription resolves different local/peer repo ids by remote URL hash,
  establishes aggregate/current-state checkpoints, receives both notification
  kinds, retries registration, and processes local event bursts and a remote
  event. It checks one admitted remote long poll, zero abandoned/duplicate
  legs or 503s, and mailbox cursor advancement only through acknowledgement.
- `subscription_retirement_abandons_one_leg_until_peer_deadline`: unsubscribe
  and run replacement each interrupt collection after one notification;
  subsequent notifications produce no further remote request. The abandoned
  request retains its permit until the actual peer handler's timeout expires
  on a paused Tokio clock. Retirement leaves the durable checkpoint unchanged.
- `subscription_busy_peer_pause_and_same_id_recovery_preserve_checkpoint`:
  deliberate fixture budget occupancy produces a readable peer error; ack
  pauses the watch and preserves the peer checkpoint. Releasing the fixture
  permit and registering with the same id/settings replays a peer event that
  arrived while paused, without resetting the cursor or duplicating a wait.

The fixture uses real subscription HTTP routes, the observer service, SQLite
fixtures, aggregate registry/cursor encoding, the desktop relay request queue,
`RelayHttpInvokePermits`, and the peer's authenticated HTTP handler. It models
receiver work surviving caller cancellation and owns that work in a JoinSet
for teardown. It does not execute the production relay WebSocket read loop,
`dispatch_relay_http_invoke`'s blocking-pool dispatch, or the cloud relay
service. Thus these are boundary integration tests, not two-machine E2E proof.
They add the subscription cancellation scenario missing from the existing
`shrinking_limit_retains_one_peer_leg_and_resumes_past_only_emitted_events`.

## Verification and remaining E2E

The manager released the bounded lane with
`RESUME HEAVY VERIFICATION EVENT-WATCH`. Every compiled command below ran
sequentially with `CARGO_BUILD_JOBS=1`, `TMPDIR="$PWD/.tmp/event-watch-tests"`,
and `--test-threads=1` for tests. Cargo used the worktree's configured `.build/`
artifacts. Commands redirected logs without a pipeline; their actual exit
statuses were retained.

- `cargo test -p kanna-server task_events::subscription_remote -- --test-threads=1`:
  initial compile succeeded in 7m52s; the first execution exited 101 (one
  passed, two failed). The fixtures incorrectly expected `stage.changed` to
  reach the mailbox, but the existing actionable predicate excludes it. The
  fixtures now emit PR events; no predicate or batching policy was changed.
  Rerun: exit 0, all three passed in 1.25s.
- Negative control: temporarily restore only `event_subscriptions.rs` from
  HEAD while retaining the new tests, then run
  `cargo test -p kanna-server --bin kanna-server subscription_notifications_and_ack_retain_one_remote_wait -- --test-threads=1`.
  Exit 101 as expected: the notification sequence generated a peer 503
  `desktop is busy; too many concurrent requests` and a mailbox watchError.
  The pinned-collect draft was restored in a `finally` block. This is a
  reproduced fixture mechanism, not identification of every live caller.
- `cargo test -p kanna-server --bin kanna-server http_api::tests::task_events -- --test-threads=1`:
  exit 0, 81 passed in 41.58s, including the restored positive regression,
  existing subscription/mailbox tests, aggregate retention, cursor handling,
  and task-event authorization boundaries.
- `cargo clippy -p kanna-server --all-targets -- -D warnings`: exit 0,
  completed in 1m53s with no warnings.

Logs are under `.tmp/event-watch-{boundary,boundary-rerun,negative-control,task-events,clippy}.log`.
Formatting of changed Rust files and `git diff --check` passed after the
compiled lane. All owned compiled commands exited; no live services were started.

`./kd test all` remains held until `RESUME FULL GATE EVENT-WATCH`. No full
workspace Rust gate, real desktop launch, two-machine relay E2E, or live stress
was run. The merge/review hold and other tasks' capacity holds remain in force.
The separate actionable-filtering task `6b153714` must preserve this collection
lifecycle and reconcile shared subscription code/tests after this fix lands;
its predicate/wait/batch policy is outside this patch.

The missing E2E needs two isolated real kanna-server processes with separate
DBs and different repo ids for a shared remote URL hash, connected through an
isolated relay service. A subscriber must produce unrelated state/subscription
notifications while a peer holds a long poll; measure admitted requests and
permit releases without logging credentials or raw cursors. Exercise mailbox
acknowledgement, same-id retry/recovery, and retirement, including a fixed
subscriber against an older receiver. That harness would cover real transport,
receiver dispatch, and independently shipped version compatibility. No shared
staging/production long-poll reproduction is authorized. Merge and stage
advance remain held separately from implementation and test preparation.
