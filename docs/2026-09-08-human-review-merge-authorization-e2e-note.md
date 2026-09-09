# Human-reviewed merge authorization: E2E coverage and the one gap

**Date:** 2026-09-08
**Surface:** the operator's "Queue for merge" control on a PR review task, and
everything between it and the repository's merge singleton.
**Design:** [specs/pr-review-dispatch.md](./specs/pr-review-dispatch.md#the-humans-route-to-the-merge-queue),
[kanna-server-boundary.md](./kanna-server-boundary.md#human-reviewed-merge-authorization).

## Why this needs E2E at all

The behaviour is a boundary, and the boundary is the whole feature: a human's
merge authorization must be a record of a *person acting*, created by a field
no agent-facing surface can send, pinned to the exact commit they read, and
delivered to a merge singleton that may be on another machine with no living
review or triage session. Every one of those clauses crosses a component
boundary. A unit test can show that one function refuses a stale head; only an
end-to-end run shows that the control, the route, the durable records, the
daemon delivery, and the message the merge agent actually reads are the same
system.

## What is covered end to end

`tests/remote-e2e/src/task-listing-actions.e2e.test.ts` —
*"carries a human's merge authorization from the review task to the merge
singleton"* — runs against the real relay, `kanna-server`, SQLite, daemon and a
scripted agent, and asserts:

- a review task created with a `reviewContext` projects it back through task
  detail, which is the only place the control can learn which pull request and
  which commit it would be authorizing;
- a decision naming a head that has moved is refused, and **no**
  `human_review_decision` row is written;
- an accepted authorization writes one immutable decision carrying the PR, the
  PR's own qualified head (`owner/name:branch`, never the review task's
  `task-*` branch and never the local `pr/<n>` ref), the base, the exact
  confirmed sentence, `operator` origin, and its delivery outcome;
- the request the merge singleton actually receives carries the `MERGE` line
  with that head, plus `HUMAN-REVIEW-DECISION`, `HUMAN-AUTHORIZATION`,
  `TRIAGE-RANK` and `RELATED-PR`;
- a second press resolves to the same decision and sends the merge master
  nothing further;
- `pipeline_item.merge_signaled_at` — the approve post's one-handoff stamp, a
  different question on a different workflow — is untouched.

## What is covered by narrower tests

- **Server contract** (`crates/kanna-server/src/http_api/tests/input.rs`,
  `mod human_review_merge_authorization`): the wire line and durable decision,
  refusal on a moved head, on a superseded context version, and on a task with
  no published context; no resend of a delivered decision; refusal to resend an
  `uncertain` delivery.
- **Durable records** (`crates/kanna-server/src/db/tests.rs`): version bump on
  refresh, rejection of a context that cannot identify the PR or commit, one
  decision per reviewed head with a moved head producing a second decision
  rather than a rewrite, and delivery outcome recorded without touching the
  decision.
- **Publication paths** (`http_api/tests/actions.rs`): a standalone reviewer
  publishing its PR identity through `complete_stage` metadata, and a malformed
  context refused rather than silently dropped.
- **Desktop** (`components/__tests__/MainPanel.test.ts`,
  `stores/workflow.queueReviewedPr.test.ts`): the control appears only with a
  published context, confirmation is a separate step, the request carries the
  version and head that were displayed, a delivered or uncertain decision stops
  re-offering it while a failed delivery does not, a head that moved past an
  earlier decision re-offers it, and a server refusal is surfaced verbatim.
- **Mobile** (`screens/TaskScreen.test.tsx`, `screens/taskActionMenu.test.ts`):
  the same availability rules, the confirmation text, and the exact decision
  the control submits.
- **Agent contracts** (`packages/core/src/workflow/qa-assets.test.ts`): both
  review agents refuse to relay the verdict and point at the control; the merge
  agent's human-reviewed request policy, expected-head precondition, and
  "queue authorization only" limits.

## The gap

**There is no driven desktop-UI E2E for this control.** The assertions above
that a *click* produces the request are component-level: they mount
`MainPanel.vue` and call the store, rather than driving the shipped app through
`tauri-plugin-webdriver`. The repository has a desktop WebDriver lane
(`kd test remote-e2e --desktop-pairing`), but it is scoped to the pairing UI
and gaining a second scenario in it is its own piece of work — it needs a
seeded review task with a review context in a driven instance, which today's
harness does not build.

What would make it testable: a desktop-pairing-style lane fixture that seeds a
task with a `reviewContext` in the driven instance's database, so the test can
open the task, press the control, confirm, and assert the `human_review_decision`
row. Until then, the seam between "the button was clicked" and "the store was
called" is covered by component tests only.

**Also not covered end to end:** the mobile control, on a device. Its action
menu, its confirmation text, the decision it submits, and every transport layer
under it have unit coverage, and the server action it calls is the same one the
remote E2E exercises — but no Appium case presses `Queue for Merge` on a
simulator. The same fixture problem applies: the driven app needs a task with a
published review context, which the mobile E2E lane does not seed today.

**And not covered end to end:** a reviewer and a merge singleton on *different*
machines. The remote E2E exercises the relay transport but resolves the
singleton on the same desktop. The cross-machine path is the ordinary
`signal_agent_request` remote-owner branch, which this change does not modify —
it inherits whatever coverage that branch has — but the specific claim that a
merge master on another machine can read the decision back through
`kanna_get_task` with `machine_id` is asserted in the merge agent's
instructions and in the message contents, not by a two-machine run.

## Not a gap

Deterministic wiring tests cannot prove that an LLM follows a policy. That the
merge agent honours the expected-head precondition, refuses to rebase under an
old decision, and never manufactures a decision is pinned as an
**asset-contract** assertion over `.kanna/agents/merge/AGENT.md`, which is the
same instrument the rest of this repository's agent policy uses. It is
deliberate, not a substitute waiting to be replaced by a live-agent test.
