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

Focused checks on 2026-09-09 passed: desktop MainPanel 26 tests, mobile
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

`./kd test all`, workspace-wide builds/full Rust lanes, and live multi-instance
remote E2E remain **held and unrun** until `RESUME HEAVY VERIFICATION PR1401`.
The revised live remote scenario is written but has not passed a run. PR #1401
also remains held until the final verified head receives independent review.

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
