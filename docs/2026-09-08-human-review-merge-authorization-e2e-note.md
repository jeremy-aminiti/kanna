# Human-reviewed merge authorization: E2E coverage and remaining gaps

**Date:** 2026-09-08; revised 2026-09-09 for the owner's conversation entry point.
**Surface:** an explicit instruction in the PR review conversation, relayed by
`kanna_queue_reviewed_pr` through the existing decision/delivery path.
**Design:** [PR review](./specs/pr-review-dispatch.md#the-humans-route-to-the-merge-queue),
[server boundary](./kanna-server-boundary.md#human-reviewed-merge-authorization).

## Coverage added or retained

The decision must name the exact reviewed PR/head/version and quote the
instruction verbatim. `operator-relayed` is an honest declaration, not verified
human presence. The observed latest stage-run id corroborates task state; it
does not prove who spoke or who called. No TUI speech is fabricated into
`task_input`. No speech classifier or GitHub approval is involved.

- **Remote E2E** (`tests/remote-e2e/src/task-listing-actions.e2e.test.ts`): the
  scripted review PTY executes the real catalog-backed CLI tool. The server
  records the relayed decision and delivers to a live merge singleton through
  the daemon. Assertions cover verbatim instruction, observed run, PR identity,
  `MERGE` plus `HUMAN-REVIEW-DECISION` with origin, moved-head refusal with no
  decision, a repeated call refused with no second delivery, and untouched
  `merge_signaled_at`. The fixture command is explicit scripted tool input;
  this is not a test of a model interpreting human speech.
- **Server integration** (`http_api/tests/input.rs`,
  `human_review_merge_authorization`): the same relayed route, provenance,
  no-context/head/version refusals, and delivered/pending/uncertain refusals.
  The strict-recording regression acknowledges input at the fake daemon,
  forces `task_input` INSERT to fail, checks `uncertain`, then repeats the HTTP
  request and checks 409 plus exactly one daemon submission.
- **Non-authorization**: existing `http_api/tests/actions.rs` completion with
  review metadata creates no decision; ordinary policy handoff in `input.rs`
  also creates none. DB tests retain uniqueness, immutable decisions and
  separate delivery updates. Catalog tests keep plain handoff's parameters
  unchanged and require head/version/instruction on the new tool.
- **Desktop**: `MainPanel.test.ts` retains read-only PR identity, overlap and
  delivery status, including stale-head handling, and asserts no queue action.
  The button, confirmation, action plumbing and its obsolete suite are removed.
- **Mobile**: task screen/action-menu tests retain only the remaining actions.
  Controller tests cover read-only decision projection and cached review-context
  restore. Queue actions and transport methods are removed; this adds no native
  code and changes no runtime version.
- **Agent contracts**: `qa-assets.test.ts` pins explicit instruction only,
  verbatim relay, no inferred approval, no extra confirmation, and no automatic
  retries. The merge agent still checks the exact head and never manufactures
  a decision or changes a PR under an old decision.

## Execution status and remaining gaps

Before main reconciliation, focused checks on 2026-09-09 at checkpoint
`a8a39ee4da3bd52c6bf6505e5195c9faf10eb435` passed: desktop MainPanel 26 tests, mobile
screen/menu/controller/transports 437, agent asset contracts 61, and scripted
fixture helpers 12. Desktop, mobile and remote-harness TypeScript checks passed.

After `RESUME FOCUSED VERIFICATION PR1401`, the following Rust checks passed
sequentially with `CARGO_BUILD_JOBS=1` and one test thread:

| Target and selector | Passed |
| --- | ---: |
| `kanna-server --bin kanna-server human_review_merge_authorization` | 8 |
| Server `merge_handoff_route_sends_an_ordinary_repo_policy_request` | 1 |
| Server `merge_handoff_does_not_signal_when` | 2 |
| Server `complete_stage_publishes_a_standalone_reviewers_pull_request_identity` | 1 |
| Server `complete_stage_refuses_a_review_context_it_cannot_use` | 1 |
| `kanna-tool-catalog --test catalog` | 42 |
| `kanna-cli --bin kanna-cli typed_cli_surfaces_match_catalog_tools_and_params` | 1 |
| `kanna-mcp --test stdio_http reviewed_pr_queue_preserves_the_instruction_and_reports_refusal_without_retry` | 1 |

The MCP test drives the real stdio adapter into an HTTP fixture, preserving the
verbatim instruction and returning a duplicate refusal without automatic retry.
The server regression executes HTTP → daemon acknowledgment → failed ledger
INSERT → uncertain decision → refused retry, with exactly one submission.

Scoped Clippy passed with `-D warnings` and `CARGO_BUILD_JOBS=1` for the default
library/binary targets of `kanna-server`, `kanna-tool-catalog`, `kanna-cli`, and
`kanna-mcp`, plus the catalog and stdio HTTP test targets separately. Formatting
and diff checks passed. This is focused evidence, not a full Rust lane.

The manager released heavy verification on 2026-09-09. Main
`f0b4e1ca8d` was merged without conflicts, preserving the checkpoint, at
`29288aef34a6784c22ae7d0a011b0740c214a124`. The only overlapping file was
`http_api/tests/actions.rs`: main changed fixture cleanup; this PR adds review
context/non-authorization tests. No queue implementation changed. Full checks
after reconciliation use `CARGO_BUILD_JOBS=2`, sequentially. Results below
must not be conflated with the earlier focused evidence. PR #1401 remains
held until the final verified head receives independent review.

Post-reconciliation command history (each command uses `CARGO_BUILD_JOBS=2`):

| Head | Command | Exit / outcome |
| --- | --- | --- |
| `29288aef3` | `./kd test all` | 1: workspace, Bazel, full Clippy passed; MCP exact tool-list test omitted the new tool. Fixed in `2095c9a00`. |
| `2095c9a00` | `./kd test all` | 1: workspace, Bazel, full Clippy and MCP passed; server 1431 passed / 1 failed because the queue route was absent from the exhaustive LAN audit table. Fixed in `97ece7570`. All eight human-review integration cases and the stdio HTTP queue case passed. |
| `97ece7570` | `./kd test all` | 1: workspace, Bazel, full Clippy, MCP, server (1432 tests) and catalog passed. Worker unit tests failed two default-database assertions; daemon and desktop mock lanes were not reached. |
| `97ece7570` | `cargo test -p kanna-daemon -- --test-threads=1` | 0: 839 passed, 4 ignored. Separate continuation, not a full-gate pass. |
| `97ece7570` | `./kd test desktop-mock-e2e` | 1: 47 of 48 files passed. Terminal-output performance failed with `terminal buffer not registered for session` in its blocked-WebView event-loop case. Harness cleanup completed. |
| `97ece7570` | `./kd test remote-e2e` | 1: stopped in `terminal-flow.e2e.test.ts` with 12 failures, before task-actions, LAN and image-attachment files. |
| `97ece7570` | task-actions command below, without `-t` | 1: 2 passed / 6 failed. New queue test omitted viewport registration; corrected only that test. Other failures remain open. |
| `97ece7570` plus the queue viewport correction | task-actions command below | 0: queue case passed, 7 other cases filtered out. Real CLI → HTTP → durable decision → daemon → live singleton; stale head writes no decision, repeated call sends no second MERGE. |

The first three full commands failed fast; none is a full-gate pass. The
verification fixes add one tool-list expectation, one LAN route audit entry,
and a measured viewport on each of the queue test's two terminal observers.
Geometry-aware remote observers deliberately defer attachment until measurement;
this is fixture setup, not a runtime policy or authorization change.

The exact focused real-boundary command was:

```sh
CARGO_BUILD_JOBS=2 KANNA_REMOTE_E2E_ENV=dev pnpm --dir tests/remote-e2e exec vitest run --no-file-parallelism --maxWorkers=1 --maxConcurrency=1 --hookTimeout=240000 --testTimeout=120000 src/task-listing-actions.e2e.test.ts -t 'relays an explicit review instruction through the catalog tool exactly once'
```

The complete task-actions file used the same command without the `-t` selector.
Formatting, diff checks, and remote-harness TypeScript checking passed after
this correction. The final commit records this tested source and evidence;
its exact identity and final focused rerun are reported in the task result.

### Open verification failures (not waived)

- Worker `config::tests::the_default_database_is_the_workers_own_under_its_data_dir`
  and `unit::tests::the_unit_launches_against_the_resolved_database` expect
  `/srv/worker/kanna-worker.db`, but `kd` injects this worktree's `KANNA_DB_PATH`.
  The worker parser honors that inherited value. Worker code and `kd` context
  are unchanged from main; no unrelated fixture or production fix was added.
- Desktop `terminal-output-performance.test.ts` failed the blocked-WebView
  event-loop case while waiting for its fixture terminal buffer. This file and
  the terminal implementation were not changed for the queue revision. No
  isolated retry is used to waive the full-file failure.
- Full remote `terminal-flow` first missed the expected runtime edge, then
  eleven cases timed out waiting for terminal markers: setup output, ordinary
  input, long input, multiline input, partial raw draft, streaming input,
  no-capability draft, bracketed continuation, menu input, child completion,
  and reconnect. Several legacy observers omit the viewport now required for
  remote attachment; only the newly added queue test was corrected in scope.
- The separate complete task-actions file also failed its ordinary terminal
  observer, old short-cursor regex, cross-desktop task lookup, and two remote
  singleton credential/refusal cases. Its sixth failure was the queue observer,
  subsequently fixed and verified in the focused run. The complete file has
  not been claimed green.

### Remaining unrun coverage

The full gate still needs a clean pass after the worker fixture issue is
resolved, including later Rust targets/doc-tests that fail-fast prevented.
The desktop mock lane needs its failure resolved and a full pass. The complete
remote lane needs its failures resolved and a full pass; `lan-layer` and
`task-image-attachment` were not reached. Existing ignored Rust cases and
workspace-skipped cloud/device cases remain unrun. No production credential
suite, new mobile visual run, or two-machine reviewed-decision readback is
claimed. Logs and exact exit files are retained under `.tmp/pr1401-*` in this
worktree. These gaps keep PR #1401 held for independent review and verification.

The prior desktop click-to-store and mobile action-menu/confirmation E2E gaps
are retired with those controls, not claimed as tested. Read-only desktop
status and mobile detail/cache projection have narrower tests; the rendered
read-only remnant has no new desktop/device E2E pass.

The earlier simulator pass installed and launched the app and rendered its
Tasks shell. It could not pair with a desktop on the same host: the Bonjour
`.local` name resolved to loopback and `lan_trust` correctly refused it. No
paired review task or queue interaction was exercised. Durable input 269's
mobile on-device waiver remains; it now applies only to the read-only remnant.
No new waiver is introduced and no pairing guard is changed.

A reviewer and merge singleton on **different machines** remain untested in
this scenario. The remote fixture drives the relay but resolves the singleton
on the same desktop. The message and merge-agent contract carry machine/id and
read-back guidance; that is not a two-machine E2E result.

Queueing now requires a live or resumed review conversation; `kanna_resume_task`
is recovery when the session stops. A closed triage parent remains irrelevant.
Deterministic wiring and asset tests cannot prove that a model follows its
contract or that a human was present; neither is claimed.
