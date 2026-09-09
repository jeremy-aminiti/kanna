# Actionable subscription delivery verification

`RESUME FOCUSED VERIFICATION ACTIONABLE-EVENTS` released focused subscription,
adapter, HTTP and catalog tests and scoped Clippy, sequentially with
`CARGO_BUILD_JOBS=1`. Full Rust/workspace builds, `./kd test all`, and separate-process
multi-instance E2E remain held until `RESUME HEAVY VERIFICATION ACTIONABLE-EVENTS`.
No release, server restart, live subscription manipulation or load generation is
authorized. Focused results below do not establish release readiness.

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
actual fenced-input/scripted-proxy admissions. The separate-process test and full
repository lanes must still run after capacity release. Stop all owned processes;
no shared production/staging probes are needed.

The permit-lifetime fix in task `2c7a34b9` remains an explicit predecessor and
must merge first. Its committed checkpoint
`667476f4d8c0969a70a357b91864b23b4dc5b2e0` was integrated locally after draft
checkpoint `e47e038d6`, with no conflicts. The pinned `event_subscriptions.rs::step`
and `subscription_remote.rs` boundary suite are preserved; this task's `collect`
change and separate `subscription_relevance` module remain alongside them.
No push, PR or handoff is permitted until the predecessor's review/gate clears.

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
root-thread compatibility and operator verification. The separate-process fixture
remains unrun under the heavy hold; focused protocol fixtures supply no waiver.

The Ship task `219c632b` owns the separately authorized MBP staging publish only
after verified event-delivery work and predecessor merge, and manager release.
This task performs no release operation. Full repository checks (`./kd test all`)
and the separate unattended boundary E2E/release gate remain pending capacity
release and successful execution.


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
No full gate, separate-process E2E, installed/live Codex TUI probe, release or
handoff ran. The work remains predecessor-first and requires review/main merge
before the manager releases Ship.
