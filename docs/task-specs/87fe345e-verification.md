# Verification record — task 87fe345e / PR #1401

Compact, durable evidence for the human-review merge-queue branch, written so
it survives worktree/log truncation across stage forks. Supersedes nothing in
`docs/2026-09-08-human-review-merge-authorization-e2e-note.md`; this file is
the short-form pointer into it plus the terminal-flow causal trace that note
predates.

## Current head

`6736e028d5c19b71954379694a791213872064c4` on branch
`human-pr-review-merge-authorization` (PR #1401), rebased onto
`origin/main` `55bb648d8fba330150dd53cd510f9b1ed6156360`.

`git range-diff` across both rebases performed this session
(`3f9520ae`→`55bb648d`) shows all 10 commits patch-identical (`=`) at every
step — the reviewed diff content is unchanged; only its base moved:

```
 1:  d6ffba1b6 =  1:  95d5ed4de Record a human's PR review merge authorization durably
 2:  8e0d97ad3 =  2:  1b5cc8f82 Give the operator a Queue for merge control
 3:  41802bc22 =  3:  09af41e28 Hold the PR review agents to briefing, and teach merge the human decision
 4:  490d8f7da =  4:  a15b63696 Close the review gaps in the human merge authorization
 5:  e3f2413c7 =  5:  18f78da3f Name the review-decision events where the feed is documented
 6:  7ea8dff2f =  6:  d0423111e fix(server): pass strict_recording to the human-review merge handoff
 7:  2e67a9fd1 =  7:  e92bb6261 feat(review): queue reviewed PRs from explicit conversation instructions
 8:  8fd31af54 =  8:  d4712fc8a test(mcp): include conversational queue in tool list contract
 9:  e4d611f1b =  9:  15d16507b docs(api): register reviewed queue route in LAN authorization audit
10:  3152c502e = 10:  6736e028d test(review): register remote viewports and record full verification results
```

## Queue-specific proof, exact commands and exits

From the independent QA verdict at head `40ed27ac9317c58cd8c8c75080db3dc82858ceb6`
(post-reconciliation against main `f0b4e1ca8d`, `CARGO_BUILD_JOBS=2`):

| Head | Command | Exit / outcome |
| --- | --- | --- |
| `29288aef3` | `./kd test all` | 1: workspace/Bazel/full Clippy passed; MCP tool-list test missed new tool. Fixed in `2095c9a00`. |
| `2095c9a00` | `./kd test all` | 1: + MCP passed; server 1431/1432 — 1 failed (LAN audit table missing queue route). Fixed in `97ece7570`. All 8 human-review integration cases + stdio HTTP queue case passed. |
| `97ece7570` | `./kd test all` | 1: workspace/Bazel/Clippy/MCP/server(1432)/catalog passed. Worker unit tests failed 2 default-db assertions (pre-existing baseline, unrelated fixture, owned by task 1750eec4). Daemon/desktop-mock not reached (fail-fast). |
| `97ece7570` | `cargo test -p kanna-daemon -- --test-threads=1` | 0: 839 passed, 4 ignored. |
| `97ece7570` | `./kd test desktop-mock-e2e` | 1: 47/48 files passed. `terminal-output-performance` failed (`terminal buffer not registered for session`). |
| `97ece7570` | `./kd test remote-e2e` | 1: stopped in `terminal-flow.e2e.test.ts`, 12 failures, before other files. |
| `97ece7570` + viewport correction | `CARGO_BUILD_JOBS=2 KANNA_REMOTE_E2E_ENV=dev pnpm --dir tests/remote-e2e exec vitest run --no-file-parallelism --maxWorkers=1 --maxConcurrency=1 --hookTimeout=240000 --testTimeout=120000 src/task-listing-actions.e2e.test.ts -t 'relays an explicit review instruction through the catalog tool exactly once'` | 0: queue case passed (real CLI → HTTP → durable decision → daemon → live singleton; stale-head refusal; no duplicate MERGE). |

This session, re-verified directly on `6736e028d` (not carried by range-diff alone):

| Command | Exit |
| --- | --- |
| `CARGO_BUILD_JOBS=1 cargo test -p kanna-worker` | 0: 15/15 passed (confirms `a2899480a` worker-fixture fix, now an ancestor of this head, resolves the prior baseline failure). |
| `CARGO_BUILD_JOBS=1 cargo test -p kanna-tool-catalog --test machine_stats` | 0: 1/1 passed. |
| `pnpm exec tsc --noEmit` (apps/desktop, apps/mobile) | 0/0 clean. |
| `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 KANNA_REMOTE_E2E_ENV=dev pnpm --dir tests/remote-e2e exec vitest run --no-file-parallelism --maxWorkers=1 --maxConcurrency=1 --hookTimeout=240000 --testTimeout=120000 src/terminal-flow.e2e.test.ts` | 1: 12 failed / 4 passed of 16. See causal trace below. Log retained this session at `.tmp/terminal-flow-run1.log` (gitignored, worktree-local). |

Not re-executed on `6736e028d` this session (carried forward by range-diff
patch-identity to the heads above, not by direct rerun): the 8 server
integration cases, the `task-listing-actions.e2e.test.ts` queue case, and the
desktop/mobile component suites (MainPanel 26, mobile 437, agent-contract 61).

Static visual proof: accepted at head `40ed27ac9317` — title bar
`Kanna — task 87fe345e · task-87fe345e-9`, read-only "Reviewed pull request"
section names PR #1401 with no queue buttons.

Full `./kd test all` has never completed a clean pass on any head; each
attempt failed fast on a different issue, each since fixed, but no single run
has gone the whole distance. Desktop-mock-e2e and the full remote-e2e suite
(`lan-layer`, `task-image-attachment` unreached) remain outstanding.

## Terminal-flow causal trace — file:line

Failure signature (all 12 cases): `timed out waiting for terminal output
{MARKER} from {sessionId}; output so far: (none)`, thrown at
`tests/remote-e2e/src/terminalFlowTestUtils.ts:556` inside
`TerminalEventCollectorImpl.waitForOutput` (method starts
`terminalFlowTestUtils.ts:544`).

Root mechanism: `TerminalEventCollectorImpl.resize()`
(`terminalFlowTestUtils.ts:526`) is what registers the viewport —
`terminalFlowTestUtils.ts:261`: `resize(cols, rows) {
client.registerTerminalViewer(taskId, cols, rows); }`. A remote observer that
never calls `.resize()` never registers a viewport and never attaches, so it
receives zero bytes regardless of what the daemon actually streamed.

- `tests/remote-e2e/src/task-listing-actions.e2e.test.ts:743-745` (this PR's
  new queue test) explicitly calls `reviewEvents.resize(80, 24)` and
  `mergeEvents.resize(80, 24)` before waiting, with the comment "Remote
  viewers attach only after reporting a measured viewport." This is the fix
  this PR actually shipped, scoped only to its own new test.
- `tests/remote-e2e/src/terminal-flow.e2e.test.ts` calls
  `collectTerminalEvents(...)` then `waitForTerminalOutput(...)` at all 12
  failing call sites (lines 675/711/790/821/850/921/964/1012/1051/1082/1087/
  1125) with **no `.resize()` call anywhere in the file** — grep for
  `\.resize(` in that file returns nothing. Every failure is exactly this gap.

`git diff origin/main...HEAD -- tests/remote-e2e/src/terminal-flow.e2e.test.ts`
is empty: this branch makes zero changes to the failing file.

## Why signal_agent/task_input/review_context changes cannot execute before this failure

`crates/daemon/` (PTY spawn, output capture, socket streaming — the only code
that could produce the bytes `waitForOutput` is waiting on) has **zero diff**
in this branch. No changed line runs anywhere in the actual output path.

Server-side changed code that a plain scripted task (no `reviewContext`, no
approve-post workflow) still passes through, traced to its guard clauses:

- `crates/kanna-server/src/http_api/tasks.rs::create_task_with_requested_id` —
  new `review_context` handling: `match payload.review_context.as_ref() {
  Some(...) => validate, None => None }`, then
  `persist_created_task_review_context`, whose first line is
  `let Some(context) = review_context else { return Ok(()); }` — zero DB I/O
  when unset. `terminalFlowTestUtils.ts`'s `createScriptedTask` never sets
  `reviewContext` (added as an optional field at `terminalFlowTestUtils.ts:134`,
  consumed at `:179-186`; the sibling `reviewedPrQueue?` field at `:126` is
  likewise unset by every `terminal-flow.e2e.test.ts` call — both are only
  referenced by the new queue test). Inert for these tests.
- `crates/kanna-server/src/mobile_api.rs::map_task_detail` — now performs 2
  extra reads (`read_task_review_context`, `latest_human_review_decision`) on
  every task-detail response, review or not. Real added work on a shared
  path, but it is downstream of PTY output production (a detail-fetch read),
  not upstream of or coupled to it — cannot suppress terminal streaming.
- `signal_merge_handoff`, `deliver_merge_handoff`,
  `ensure_merge_handoff_before_close` (`signal_agent.rs`) are
  merge/approve-post-workflow-specific; terminal-flow's plain scripted tasks
  carry no approve post and no pending handoff, so these are not reached. The
  one existing-function change reached at task-close time
  (`ensure_merge_handoff_before_close`) is a mechanical struct rename
  (`MergeHandoffRequest`→`MergeHandoffMessage`) with identical field values.
- `task_input.rs`'s 6-line change only fires on the `task_input_record_failed`
  error branch (a DB-insert-failure case); the success path taken by every
  passing input delivery is byte-identical to `origin/main`.
- All other signal_agent.rs additions (`deliver_human_review_merge_request`,
  `classify_delivery_failure`, `human_review_request_lines`,
  `worktree_head_commit`, `queue_reviewed_pr`) are new functions/routes never
  called by `terminal-flow.e2e.test.ts`.

Conclusion: changed branches do execute on the call path before the failure
(task creation, task-detail polling), but every one is a proven no-op or a
downstream/unrelated read for a plain scripted task — none sit in, or before,
the daemon's PTY-output-streaming code that `waitForOutput` is blocked on.
The failure is the pre-existing missing-viewport-registration gap in
`terminal-flow.e2e.test.ts`, not a product regression from this branch.
