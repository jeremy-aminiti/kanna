# Actionable subscription delivery verification

The manager released focused verification and then `RESUME HEAVY VERIFICATION
ACTIONABLE-EVENTS`. Heavy commands ran sequentially with `CARGO_BUILD_JOBS=2`.
The canonical full gate and remote E2E returned exit 1; the evidence below does
not establish release readiness. No publish or Ship operation ran.

The new `http_api/tests/task_events/subscription_relevance.rs` fixtures exercise
durable producers → native waits → filtered batching/cursors and initial
subscription mailbox/acknowledgement, including both adapter selections. A
fixture peer ignores the optional selection parameter to exercise old-server
responses through the aggregate relay request path. A detached transition test
in `task_actions.rs` covers daemon connection failure → durable lifecycle failure
→ actionable wait. Existing subscription tests retain fault, restart, uncertain
wake and acknowledgement coverage. These focused Rust fixtures now have execution
evidence below.

These fixtures do not prove a real installed Codex app-server turn or PTY input
wake across independently running server/relay processes. Focused tests cover
selection and mailbox continuity through the HTTP/native/aggregate handlers, and
actual fenced-input/scripted-proxy admissions. The separate-process input test now has execution evidence below; the canonical
repository and remote lanes still have failures. Stop all owned processes;
no shared production/staging probes are needed.

The permit-lifetime predecessor `2c7a34b9` merged through PR #1404 at
`f0b4e1ca8d1a209ccf6e8107fcfb8050911306b0`. Its reviewed head `d1c24e40f`
is patch-identical to the inherited `667476f4d`. After the running full gate
finished, merge checkpoint `e6a8e9486` reconciled that main commit. The only
conflicts were duplicate test-module registration and the add/add remote test
file; the incoming remote suite exactly matched the inherited predecessor.
Both boundary suites remain. The production pinned collection block is
byte-identical to merged main, and reconciliation changed no notification
implementation file relative to tested checkpoint `16973a543`.

Filtered native-page draining now checks the existing wait deadline before
another read, including after its scheduling yield. Zero-time bootstrap draining
uses the existing aggregate zero-time drain budget. A timeout retains the consumed
checkpoint and reports remaining raw pages without counting them toward actionable
batch thresholds. Aggregate re-arming likewise cannot continue a positive wait
past its deadline. Paused-clock regressions cover both timeout modes and resuming
through the excluded backlog to the next actionable event; both now pass.

Other shared surfaces are limited to the subscription/wait catalog descriptions,
subscription section of the server boundary document and manager event-loop
instructions. Preserve independent brief-detail (`9fe6ba82`), machine-resource
(`29696721`) and conversation-queue (`87fe345e`) changes. In particular, preserve
`cc412ba26`'s provider/model/effort section and creation examples when it lands;
this task does not import that independent instruction change.

The separately observed false `awaiting_input` from a quoted fixture belongs to
terminal prompt detection. This task deliberately retains confirmed question
events and makes no speech/transcript classification changes.

## Adopted timing addition

Manager input following consultation `a064daa6` adopted this addition to the same
work item: 1000ms trailing quiet, maximum 5000ms collection hold, and minimum 5000ms
between adapter-call admissions. These are engineering defaults adopted by the
manager, not owner-supplied numbers. The previous relevance-only prohibition on
new timers does not govern the explicitly authorized timing addition.

The internal collector policy is selected only at the owning wait, never from
peer wire parameters. Admission remains before the shared sending CAS; its
monotonic clock and additive `wakeAdmitted` recovery hint survive acknowledgement
and prevent burst credits. Normal collector returns preserve pending peer legs.
The pinned `step` collection lifetime and retirement semantics remain unchanged.

New paused-clock `task_events/subscription_timing.rs` fixtures drive the worker
through real HTTP/DB waits, fenced daemon input and an isolated scripted Codex
proxy executable. Test-only observation/admission channels establish scheduling
barriers and measure adapter-call admission times, not model output-consumption
time. Coverage includes trailing bursts, irrelevant noise, sustained cap, full
pages, urgent cooldown, unacked backpressure, ack before scheduled delivery,
retirement and restart with an older JSON record. The semaphore-backed remote
suite retains permit/recovery assertions and adds normal quiet returns plus
remote urgency and initial discovery-fault cursor continuity. Its fault expectation
now permits the local PR to arrive on recovery: the owning server must no longer await it before reporting a peer fault.

`tests/remote-e2e/src/terminal-flow.e2e.test.ts` adds a real-process input-adapter
case: producer requests through the relay, separate server/daemon/scripted PTYs,
more than a full page of blocker changes, urgent failure, exact FIFO continuation
and one engine submission per acknowledged page. Protocol proxy fixtures are not
live Codex TUI E2E. Live native timing still needs authenticated shared app-server
root-thread compatibility and operator verification. The separate-process input fixture passed with a second authenticated peer and
retained relay leg; the execution details and remaining failures are below.

The Ship task `219c632b` owns the separately authorized MBP staging publish only
after verified event-delivery work and predecessor merge, and manager release.
This task performs no release operation. The failed canonical repository and
remote gates, review/main merge, and the separate unattended boundary E2E/release
gate remain release blockers.


## Focused verification results, 2026-09-09

All compile/test/Clippy commands below used `CARGO_BUILD_JOBS=1`; test runs used
`--nocapture --test-threads=1`. Only `kanna-server`'s binary test target and the
catalog package were selected, not a workspace/full build.

| Command after the environment prefix | Result |
| --- | --- |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::task_events::subscription_timing -- --nocapture --test-threads=1` | 12 passed, 8.84s; real fenced daemon input and isolated Codex proxy I/O |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::task_events:: -- --nocapture --test-threads=1` | 101 passed, 1 failed, 70.50s; failure was the recovery fixture count described below |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::task_events::subscription_remote:: -- --nocapture --test-threads=1` | After fixture correction: all 5 passed, 11.82s |
| `cargo test -p kanna-server --bin kanna-server http_api::harness_wake::tests:: -- --nocapture --test-threads=1` | 4 passed |
| `cargo test -p kanna-server --bin kanna-server detached_transition_without_daemon_publishes_actionable_failure -- --nocapture --test-threads=1` | 1 passed |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::raw_input::subscription_wake -- --nocapture --test-threads=1` | 2 passed, 15.16s |
| `cargo test -p kanna-tool-catalog -- --nocapture --test-threads=1` | 5 unit + 41 contract tests passed; 0 doc tests |
| `cargo clippy -p kanna-server --bin kanna-server --tests --no-deps -- -D warnings` | Passed |
| `cargo clippy -p kanna-tool-catalog --all-targets --no-deps -- -D warnings` | Passed |

The first attempted command incorrectly selected `--lib`; kanna-server has only
a binary target. The corrected compilation exposed fixture type errors:
`unique_test_file` returns a String, and daemon submission data is bytes. Explicit
PathBuf and UTF-8 conversions fixed them; no production code change was needed.

The broad task-event run then found a stale count in
`subscription_busy_peer_pause_and_same_id_recovery_preserve_checkpoint`: expected
2 attempts but observed 3. The recovered ordinary PR leg legitimately completes
and rearms during the new quiet window. The corrected fixture asserts exactly
3 attempts, 2 admissions, 1 completed/released leg, 1 occupied permit, the original
single 503 and zero abandonment; ack plus unrelated notifications retain the new
silent leg without another request. All five remote cases passed afterward.
The other 101 cases already passed, including the seven relevance fixtures and
all twelve timing cases; they were not rerun after this assertion-only correction.

The permitted pass fixes only test conversions and that causal timing expectation.
The predecessor's pinned collection block remains byte-identical to `667476f4d`.
These focused results were recorded before the heavy runs below. No installed/live
Codex TUI probe, release or handoff ran; review/main merge still precede Ship.


## Batching, debounce and wake-rate coverage map

| Mechanism | Existing behavior and this change | Evidence |
| --- | --- | --- |
| Batching | Existing durable mailbox, exact cursors and ack backpressure; subscription cap 100/minimum 1 retained. Relevance now precedes all count/flush thresholds, including legacy peer filtering. Raw waits retain their public defaults. | Relevance native/aggregate/bootstrap tests; both-adapter full-page/FIFO/ack tests; separate-process 105 blocker changes and retained peer failure. |
| Trailing debounce | Subscription temporal debounce was absent; public wait debounce is first-match and is unchanged. New owning subscription collector closes ordinary pages at last relevant observation + 1s, capped by first relevant observation + 5s or the existing receiver deadline. Capacity and urgent attention seal earlier. Irrelevant events never reset it. | Both-adapter lone/noise/burst/sustained/urgent and receiver-deadline tests; semaphore-backed aggregate normal-return and fault tests. |
| Wake rate | Ack backpressure was not a temporal rate limit. New shared gate admits either adapter at least 5s apart, before sending CAS, with no burst credits. Recovery conservatively rearms at most one 5s cooldown. Urgency never bypasses this gate. | Both-adapter admission timestamps for bursts/full pages/failure/cooldown; ack-race, restart, retirement, definitely-undelivered retry and uncertain-result tests. |

The fixed 1s/5s values are manager-adopted engineering defaults. An urgent page
observed in the current collection is eligible immediately when a slot is free,
otherwise after the remaining admission cooldown (at most 5s). Ordinary collection
adds at most 5s from its first relevant observation, subject to normal execution.
These are scheduler bounds with healthy execution and available acknowledgement,
not universal creation-to-wake guarantees: pending unacked pages, older backlog,
unavailable transport, stalled servers and busy harness consumption remain outside
them. Both actual adapter fixtures passed; only the input adapter has the new
separate-process timing fixture. Live Codex TUI timing remains unverified.

## Heavy verification, 2026-09-09

Logs are retained locally under `.tmp/verification/`; they are not committed.
Commands ran sequentially, with `CARGO_BUILD_JOBS=2`.

| Base and command | Actual result |
| --- | --- |
| Before reconciliation, clean `16973a543`: `./kd test all` | **Exit 1**. All 19 JS/TS tasks succeeded (14 cached), Bazel prerequisite passed, workspace Clippy with `-D warnings` passed, workspace test compilation passed, desktop unit tests and all 1,439 server unit tests passed, including all 12 actual-adapter timing cases. Server process integrations passed. Worker stopped the gate: 13 passed, 2 failed. Daemon and desktop-mock tails were not reached. |
| Same compiled worker binary, only `KANNA_DB_PATH` removed | **Exit 0**, all 15 passed with default test concurrency; also 15 passed serially. Diagnostic only; does not replace the canonical failure. |
| After reconciliation, `./kd test remote-e2e --dev` | **Exit 1**. Smoke and cloud pairing/auth/discovery specs completed before fail-fast stopped at terminal-flow: 13 failures. Task-listing/actions, LAN and image-attachment specs were not reached. |
| After bounded fixture corrections, terminal-flow filtered to `paces real PTY mailbox` | **Exit 0**, 1 passed, 15 skipped, 67.24s. Separate server/relay/daemon/scripted PTYs, local full pages and urgent failure, acknowledged exact continuation, a second peer's failure through the single retained positive-timeout relay request, and one actual engine PTY submission per page. |
| After reconciliation, `cargo test -p kanna-daemon -- --test-threads=1` | **Exit 0**, 839 passed, 4 ignored. |
| After reconciliation, `./kd test desktop-mock-e2e` | **Exit 1**, 1 of 48 targets failed: terminal-output-performance could not find the registered terminal buffer. |
| After peer cleanup correction, complete terminal-flow spec | **Exit 1**, 4 passed (including the new timing case), 12 failed, 292.45s. |
| Fresh harness, only `sends remote input to the agent PTY and rejects input after exit` selected | **Exit 1**, 1 failed, 15 skipped, 36.57s. No timing test or additional peer ran; terminal stream still produced no `SCRIPT_READY`. |
| Remote harness unit tests and TypeScript check | 8 tests in 2 files passed; `tsc --noEmit` passed. |

The canonical worker failures are
`config::tests::the_default_database_is_the_workers_own_under_its_data_dir` and
`unit::tests::the_unit_launches_against_the_resolved_database`. Both expected
`/srv/worker/kanna-worker.db`, but received the task's inherited `KANNA_DB_PATH`.
The production parser explicitly reads that environment variable; the fixture
assumes it is absent. Running the identical compiled binary with only that
variable removed passes. No worker code changed and no baseline waiver is claimed.

The desktop failure is
`terminal-output-performance.test.ts` → `classifies a blocked WebView event loop
and resumes numbered output without loss`. After session discovery and selection,
`waitForTerminalBufferText` calls `terminalBuffers.lines` before finding a buffer;
that hook throws immediately for a missing registration. This is separate evidence
from the worker environment failure. Its ownership/race remains unresolved; no
terminal implementation or test was changed to hide it.

The new remote timing fixture initially failed during subscription registration
because legacy emulator identity does not grant server-to-server routing. It now
seeds the existing desktop-secret credential contract for both desktops. The
harness's default accelerated one-second cursor eviction also cannot represent a
production five-second pacing window; an optional fixture switch disables that
existing debug override for this spec, leaving other callers' default unchanged.
The successful run asserts no watch/machine errors, exactly one peer long poll
before and after local page acknowledgements, and that same leg returns peer
failure. No production auth, cursor TTL, timing or permit policy changed.

The initial registration failure occurred before peer cleanup and could cascade
into later stream tests. Cleanup now surrounds the entire peer case, including
registration. The complete rerun still had 12 failures: the raw busy-edge case and all 11
stream cases. Four passed, including the new timing case. A representative stream
case also failed alone in a fresh harness, with the timing test and additional
peer skipped: no `SCRIPT_READY` output arrived. This rules out that peer setup as
a necessary cause of the representative failure; it does not establish the cause
of every stream failure or a baseline waiver.
The other original failure waited for a raw runtime busy edge; daemon logs reported
an unclassified scripted Claude footer (`✻ Working (1s · esc to interrupt)`).
Prompt detection is out of scope, and this task does not suppress true input facts.


Final reconciliation checks: `cargo fmt --all -- --check`, TypeScript checking
and `git diff --check` passed. The new bounded changes after `e6a8e9486` are
fixture credential/retention support, cleanup and verification notes; no further
Rust production change was needed. Main-relative diff retains the predecessor's
pinned collection exactly, with no duplicate or reverted permit fix. No unrelated
manager model-choice instructions were imported. Owned process cleanup completed with no failed entries; the inventory was
removed and no current-worktree build/temp processes remained. Local `bazel
shutdown` returned exit 0 before recording the blocked stage result.

**Not ready:** canonical full gate exit 1, canonical remote gate exit 1,
desktop-mock tail exit 1, and unresolved stream/runtime observation failures.
Focused actual-adapter tests and the positive separate-process timing test do not
waive these failures. At this checkpoint the three remote specs skipped by fail-fast were unverified;
the follow-up below records their execution. Live Codex TUI compatibility and
the separate unattended release boundary remain unverified.


## Matched-main stream investigation

The follow-up compares committed branch `495fd541d` with authoritative main
`0c8bab681974b80912e35bb7483137471191a4a9` (including machine-resource PR #1405).
Main was exported with `git archive` under this task's `.tmp/verification/baseline-main`;
no main checkout, other task worktree or branch was changed. Offline frozen-lockfile
installation downloaded no packages. A blob-by-blob comparison after the run found
one changed tracked file in the exported source: terminal-flow's existing
`keepArtifacts: true` option. All production files matched the main commit.

Both runs selected only `sends remote input to the agent PTY and rejects input
after exit`, used the original accelerated one-second test cursor TTL, retained
runtime artifacts, and used the same task-local Firebase emulator cache. The
branch's temporary `expireShortCursors: true, keepArtifacts: true` fixture setting
was restored to its committed production-retention setting immediately afterward.
Both ran with:

```sh
CARGO_BUILD_JOBS=2 KANNA_APP_ENV=dev KANNA_REMOTE_E2E_ENV=dev \
KANNA_E2E_DEBUG_TERMINAL_EVENTS=1 \
RUST_LOG=info,kanna_server::ksp=debug,kanna_daemon::output=debug \
FIREBASE_EMULATORS_PATH="$TASK_ROOT/.tmp/verification/firebase-emulators" \
GIT_CEILING_DIRECTORIES="$TASK_ROOT/.tmp/verification" \
TMPDIR="$RUNTIME_ROOT" \
pnpm --dir "$SOURCE_ROOT/tests/remote-e2e" exec vitest run \
  --no-file-parallelism --maxWorkers=1 --maxConcurrency=1 \
  --hookTimeout=240000 --testTimeout=120000 src/terminal-flow.e2e.test.ts \
  -t 'sends remote input to the agent PTY and rejects input after exit'
```

`TASK_ROOT` denotes this worktree. `SOURCE_ROOT` was the exported main tree or
this worktree, and `RUNTIME_ROOT` was the corresponding `.tmp/verification/runtime-baseline`
or `runtime-branch`. These are path substitutions, not differences in test policy.
`GIT_CEILING_DIRECTORIES` prevents the source tarball from falsely reporting its
containing branch's Git identity. It does not change the independently initialized
scripted task repositories.

| Run | Exit and assertion | Full producer/consumer log under `.tmp/verification/` |
| --- | --- | --- |
| Main `0c8bab681`, isolated representative | **1**; 1 failed, 14 skipped; 45.13s. `SCRIPT_READY` timeout, output `(none)`. | `remote-stream-matched-baseline-local-cache.log` |
| Branch `495fd541d`, equivalent fixture/env | **1**; 1 failed, 15 skipped; 75.71s. Same `SCRIPT_READY` timeout, output `(none)`. | `remote-stream-matched-branch-clean-cache.log` |

Both logs show successful task spawn, daemon PTY output chunks, collector
`connected: true` and input availability, but no collector snapshot/output event.
The count of skipped cases differs only because the branch adds the timing case.
This establishes that the representative no-output failure occurs on current
main without the filtering/timing additions. It does not establish the root defect
within terminal transport, or waive every other stream failure.

The compared producer/consumer path is scripted agent → daemon output → server
KSP/relay → relay tunnel → mobile `relayClient`/`StreamClient` → terminal collector.
Those production sources and the scripted-agent and Node relay fixtures are
identical between branch and main. The changed collector utility signature is
only the `pinSingleStageWorkflow` harness type; runtime terminal collection is
unchanged. Filtering acts on durable event waits and subscription delivery, which
this representative case never registers. No terminal repair was made.

Two setup failures are retained separately, not counted as the matched assertion:

- `remote-stream-matched-baseline.log`: exit 1 before tests; Firestore's shared
  cached JAR disappeared and Java reported `NoSuchFileException`. The existing
  `FIREBASE_EMULATORS_PATH` option isolated both retries from the shared cache.
- `remote-stream-matched-branch.log`: exit 1 before tests; Cargo's reused cache
  retained main's catalog artifact, missing the branch relevance export. A scoped
  `cargo clean -p kanna-tool-catalog` (exit 0) rebuilt that dependency. No source
  change or full clean was needed. Main and branch reused only this task's build
  cache; source comparisons and full logs retain the distinction.

The worker `KANNA_DB_PATH` and desktop terminal-buffer registration failures keep
their separate dispositions. This matched stream result supplies no evidence
about either of them. The predecessor pinned collection still matches current
main exactly. Main's newer machine-resource change has not been merged into this
branch; the three-dot main-relative diff contains only the intended notification
work, and future reconciliation must retain that independent resource change.


### Previously skipped remote specs

These are the three files the canonical fail-fast runner did not reach, run
individually and sequentially with its existing Vitest arguments, not a full-suite
rerun. Each used `CARGO_BUILD_JOBS=2 KANNA_APP_ENV=dev KANNA_REMOTE_E2E_ENV=dev`,
the task-local `FIREBASE_EMULATORS_PATH` above, and `TMPDIR` set to `runtime-branch`:

```sh
pnpm --dir tests/remote-e2e exec vitest run --no-file-parallelism \
  --maxWorkers=1 --maxConcurrency=1 --hookTimeout=240000 --testTimeout=120000 \
  src/task-listing-actions.e2e.test.ts
# Subsequent sequential invocations select src/lan-layer.e2e.test.ts
# and src/task-image-attachment.e2e.test.ts with the same arguments.
```

Task-listing/actions returned **exit 1: 2 passed, 5 failed, 50.27s**. Its full log
is `remote-skipped-task-listing.log`. Failures are recorded separately:

- Task creation's terminal observation timed out on `SCRIPT_READY` with no output.
- Parent-scoped events expected `/^kh1\.[0-9a-f]{8}$/`, but received a
  `kh1.<issuer>.<nonce>` handle. Both the formatter and this test file are
  byte-identical to current main: this is a source-confirmed fixture-contract
  mismatch, not a changed cursor format in this branch.
- The durable relay-cursor case returned 404 `task not found` after creating an
  additional legacy-identity desktop.
- The repository singleton case expected a directory refusal but received 503
  `target desktop-secret authentication is required` from that peer.
- The stage/merge case also received the peer-authentication 503.

The last three are not disposed by the matched stream comparison. The listing
fixture and its additional-desktop default identity behavior are unchanged from
main, but no matched runtime baseline of those cases was run. No authentication,
terminal or cursor-fixture repair was folded into this task.


LAN returned **exit 1: 3 passed, 8 failed, 262.79s**. Its full log is
`remote-skipped-lan.log`. Seven cases failed on terminal-ready/output sentinels
(`SCRIPT_READY`, `SCRIPT_INPUT_READY` or `MOBILE_PTY_SNAPSHOT_SENTINEL`), including
window/scrollback, remount, draft input and LAN/relay parity. The Activity-dismissal
case separately expected a task id in the mobile result and received an empty
array. The representative relay baseline does not dispose of these LAN or
Activity assertions; no matched baseline of this spec was run.


Image attachments returned **exit 1: 4 failed, 142.15s**. Its full log is
`remote-skipped-images.log`. Each case stopped at `SCRIPT_READY` with output
`(none)` before its photo upload/refusal/removal assertions. They are now executed,
but those downstream attachment assertions remain unproven.

### Independent-review handoff and explicit failure matrix

The bounded investigation changed only this verification note. Temporary fixture
settings were restored; all owned services were stopped, both inventories cleaned
without failed entries, and no current-worktree build/temp executable remained.
Baseline source, full combined producer/consumer logs and individual daemon/server
logs remain under `.tmp/verification/` for inspection. No full suite was rerun,
no terminal repair was made, and no branch, PR or release was published.

The review target is the filtering/timing implementation and causal fixtures
through `495fd541d`, plus this evidence update. Compare against `0c8bab681` using
the three-dot diff; the reviewed permit predecessor is retained byte-for-byte.
The newer main machine-resource additions must be retained during any later
reconciliation. The reviewer should assess the default selection, collector
checkpoints/retained legs, admission-before-CAS, ack/recovery invariants and the
passing actual-adapter/process timing evidence against the documented contract.

| Finding | Evidence and disposition for review |
| --- | --- |
| Representative relay `SCRIPT_READY`/no-output failure | Reproduced on exported main and branch with matched fixture/environment; occurs without any event subscription registration. Shared current-main/harness failure, not dependent on this branch's relevance/timing code. Root terminal defect intentionally not repaired. |
| Short-cursor expectation | Source-confirmed mismatch already present on main: unchanged three-part producer versus unchanged two-part fixture regex. No claim that all other cursor assertions passed. |
| Peer-authentication/404 listing failures | Exact refusals retained; baseline source comparison exists, but matched runtime disposition remains open. |
| Other relay/LAN/Activity failures and image assertions blocked by readiness | Execution results retained; no blanket extension of the representative baseline. Downstream assertions remain unproven. |
| Worker database-path failure | Same compiled binary passes after removing only inherited `KANNA_DB_PATH`; canonical exit remains 1. This is independent evidence, not a stream disposition. |
| Desktop terminal registration | Canonical tail exit 1, missing buffer registration; no matched baseline or repair. Remains open. |

This is a concrete packet for independent technical review and failure disposition,
not a green-gate or Ship handoff. All previously skipped specs have execution
results; completing their blocked assertions still requires the shared harness/
transport and other failure dispositions above. That engineering work is not
replaced by an owner waiver. Merge and Ship remain held until independent review
explicitly resolves these findings and the required release evidence is satisfied.

### Revision 1: final automatic completion and verdictless-exit bootstrap

This revision addresses only the two review findings. Subscription enrichment
uses the task's resolved workflow and the completion engine's shared
post-or-successor check. Successful automatic final main completion without a
post remains actionable until explicitly advanced, including a fresh settled
scan. A new subscriber also reconciles an open exited session whose latest run
was cancelled without a verdict; a durable runtime echo remains quiet. Closed
or replaced sessions stay quiet. No completion state store, timing change,
worker repair, terminal repair, or relay repair was added.

The final-stage fixture invokes the actual complete-stage HTTP route and checks
persisted success, an open task and no awaiting-advance event. Both `input` and
`codex_app_server` selections receive the live completion in the mailbox and
receive the unresolved completion on fresh registration. The zero-exit fixture
invokes `handle_task_terminal_state`, settles its runtime using the existing
DB debounce fixture, then registers each selection and verifies one bootstrap
page, exact acknowledgement continuation without replay, and unchanged HTTP
`readState`. A pinned automatic review-to-PR successor remains quiet before and
after entering PR. These are boundary fixtures, not live provider TUI tests.

After the renewed capacity release, the following ran sequentially with
`CARGO_BUILD_JOBS=2`. Logs are under `.tmp/revision-verification/` in this revision
worktree.

| Command after the environment prefix | Result |
| --- | --- |
| `cargo test -p kanna-tool-catalog subscription_relevance_tests` | Exit 0; 5 passed |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::task_events::subscription_relevance -- --test-threads=1` | Exit 0; 9 passed, 5.88s |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::actions::complete_final_auto_without_post_remains_open_without_awaiting_advance -- --exact --test-threads=1` | Exit 0; 1 passed, 0.35s |
| `cargo test -p kanna-server --bin kanna-server http_api::tests::task_events:: -- --test-threads=1` | Exit 0; 104 passed, 56.77s, including retained-peer, timing and adapter fixtures |
| `cargo test -p kanna-tool-catalog -- --test-threads=1` | Exit 0; 7 unit and 41 contract tests passed |
| `cargo clippy -p kanna-server --bin kanna-server --tests --no-deps -- -D warnings` | Exit 0 |
| `cargo clippy -p kanna-tool-catalog --all-targets --no-deps -- -D warnings` | Exit 0 |

The latest manager instruction requires this committed focused-results report
before another full gate. `./kd test all` has therefore not been rerun for this
revision. The earlier canonical exit 1 and every category in the independent
failure matrix above remain intact: the matched-main representative stream
failure does not dispose of peer-auth/LAN/Activity/downstream failures, and the
worker environment diagnostic and desktop registration failure remain separate.
Worker fixture repair `1750eec4` is in independent review and was not duplicated.
Live Codex TUI/native timing and the unattended release boundary remain unverified.
