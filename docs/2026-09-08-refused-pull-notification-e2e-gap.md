# Refused-pull notification: no two-desktop E2E yet

A pull the source machine refuses now travels back to the machine that asked
for it: the source's engine sends `report-task-pull-refusal` through its
sidecar, the requester's sidecar emits `task_pull_refused`, and the requester's
server records it as a `failed` incoming `task_transfer` with no local task,
which the snapshot reports as a transfer alert and the window turns into a
toast.

That chain crosses two `kanna-server` processes, two `kanna-task-transfer`
sidecars, and (in the reported incident) the relay. It has no single E2E test.

## Why not yet

`tests/remote-e2e`'s harness (`src/harness.ts`) starts **one** desktop: one
server, one sidecar, one daemon, one relay, one set of ports, one Firebase
emulator project. Nothing in it can bring up a second desktop, pair the two,
and drive a transfer between them, and no existing test does — `grep transfer
tests/remote-e2e/src` finds only the sidecar's port in the config writer. A
real end-to-end refusal needs a second `RemoteHarness` with its own
`desktopId`, ports and DB, LAN pairing (or a cloud route) between the two, and
a source task whose provider artifact is deliberately absent.

## What would make it testable

A `startRemoteHarness` that can be called twice against one emulator project
(or a `startPairedRemoteHarnesses` helper), plus a pairing step over the LAN
listener. That is a harness feature in its own right and would unlock every
transfer E2E, not only this one.

## What covers it meanwhile

- `crates/task-transfer/tests/runtime.rs`
  `a_refused_pull_is_reported_back_to_the_machine_that_asked` — two real
  runtimes on real sockets: the destination pulls, the source refuses, and the
  destination receives `TaskPullRefused` with the reason, through the same
  sealed, authenticated peer request path every other transfer step uses.
- `crates/task-transfer/tests/protocol.rs`
  `task_pull_control_peer_and_event_messages_roundtrip` — the wire shapes of the
  control request, the peer request/response, and the `task_pull_refused`
  event. `kanna-server` does not depend on this crate and reads those keys out
  of raw JSON, so this is the pinned half of that contract.
- `crates/kanna-server/src/transfer_engine/import.rs`
  `a_refused_pull_becomes_a_durable_record_and_a_snapshot_alert` — the other
  half: the same event JSON routes to durable work, records one row however
  many times it is redelivered, and surfaces as a snapshot alert.
- `crates/kanna-server/src/http_api/tests/transfers.rs`
  `a_refused_pull_is_readable_on_the_machine_that_asked_for_it` —
  `GET /v1/tasks/{id}/transfers` answers with the refusal for a task that only
  ever existed on the other machine, where it used to answer 404.
- `apps/desktop/src/composables/useTransferFailureToasts.test.ts` — the alert
  becomes exactly one toast naming the task that was asked for, is retired on
  the server the moment it is announced, says nothing in a window mounted
  after that retirement, and never retires a failure that has a task of its
  own (that one keeps the sidebar marker as its standing surface).
- `crates/kanna-server/src/db/tests.rs`
  `a_failed_transfer_with_no_local_task_is_reported_as_a_snapshot_alert` — the
  alert covers *every* task-less failure, not only refusals: an import that
  died before it created anything is the same class of news, and retires by
  the same path.
- `crates/kanna-server/src/db/tests.rs`
  `the_alert_migration_retires_history_without_touching_a_task_s_own_marker` —
  migration 067 replayed against rows that predate it, so an upgrading
  operator is not met with a launch full of toasts for moves they can no
  longer act on, while a failure that has a task keeps its marker.

## Visual verification

The desktop half was verified in the real app (`./kd dev up`, this worktree's
own dev instance — seeded `example-api`/`example-app`/`example-docs`, not the
staging instance), driving it through `tauri-plugin-webdriver`:

- The `⇄✗` marker carries the reason and the dismiss hint in its tooltip, and
  clicking it clears the marker without selecting the row underneath (observed
  live: markers 1 → 0, selection unchanged). Screenshots in
  `docs/task-screenshots/1f709e6f-screenshots/`.
- The refused-pull toast rendered with the right text
  ("The other machine will not send task afed27d1: …"), read from the live
  DOM. It is not in a screenshot: macOS suspends `requestAnimationFrame` for an
  occluded webview, so the toast's enter transition never runs while the window
  is behind another app, and every toast sits at `opacity: 0` in the captured
  frame. Raising that window is what the capture harness warns about here (it
  shares a window origin with the owner's live staging app), so the DOM read is
  the evidence.
- The alert's whole life, on the same instance, after review round 1 added its
  retirement. A task-less failed transfer was inserted (server reported 1
  alert); the window was reloaded, and the alert went to 0 purely as a
  consequence of that mount announcing it, with `dismissedAt` stamped
  `2026-09-09 02:58:52`. A **second** reload produced zero toasts and zero
  alerts — the "announces at every launch, forever" defect. The record stayed
  readable throughout: `GET /v1/tasks/afed27d1/transfers` still answers with
  the row and its reason. The same dev database also held 10 task-less
  failures from the earlier session, and migration 067 had retired all 10, so
  none of them announced — the upgrade behaviour, observed rather than
  reasoned about.

## The bug this was reported alongside

The refusal in the incident was itself wrong: `plan_session_artifacts` paired
`pipeline_item.agent_provider` (stamped once at task creation) with
`pipeline_item.agent_session_id` (rewritten by every spawn), so a Claude task
was refused for a missing Codex rollout. That half is unit-covered in
`crates/kanna-server/src/transfer_engine/push.rs` and needs no second machine:
the plan is computed entirely on the source.
