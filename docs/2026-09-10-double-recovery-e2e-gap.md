# Double reboot recovery has no end-to-end test

Server-routed recovery must never hand a finished agent its stage
instructions again. One recovery of a succeeded run is covered by
integration tests in `crates/kanna-server/src/task_creator/tests/recovery.rs`.
The sequence that actually broke on 2026-09-10 — a machine reboot, a
recovery, and then a *second* loss of the recovery session — is covered at
the same integration level but not end to end.

## What is covered

`a_second_recovery_still_knows_the_stage_already_succeeded` and
`a_second_recovery_after_a_fresh_fallback_keeps_the_success_verdict` drive two
sequential `POST /v1/tasks/{id}/actions/resume` calls through the real HTTP
route against a fake daemon, asserting the spawned command line and the
persisted runs across both the retained-transcript and fresh-fallback chains.

One case per no-work termination producer that can sit between a success and a
later recovery: `a_rejected_resume_is_transparent_to_a_later_recovery` runs the
real `handle_task_terminal_state` rejected-resume observer before the resume
route; `a_quota_replacement_is_transparent_to_a_later_recovery` and
`a_failed_replacement_spawn_is_transparent_to_a_later_recovery` cover the quota
and spawn-failure shapes.

`a_genuine_failure_in_the_lineage_stops_the_completed_stage_walk` and
`a_lineage_free_run_after_a_success_recovers_with_ordinary_semantics` pin the
two ways the walk must *not* apply. The genuine-failure fixture links its
failed run to the success by both pointers on purpose: without the link it
would prove only that the walk stops when there is nowhere to go.

## What is not covered

No test kills a real daemon twice with real provider processes attached. The
integration cases substitute a fake daemon for the PTY lifecycle, so they
prove the server's decision and its durable record, not that a real
`claude --resume` reattaches to a real transcript after two machine restarts.
Specifically unproven end to end:

- a real PTY dying with a `succeeded` run behind it, twice in a row;
- the daemon's own session accounting across two restarts within one task;
- that the recovery prompt reaches the provider's context window intact after
  the second hop.

## What would make it testable

A harness that can start the real daemon, spawn a provider process against a
recorded transcript, kill the daemon without letting it hand off, and repeat —
asserting on the second replacement's prompt. `crates/daemon/tests/` covers
handoff and reconnect but has no hard-kill-and-restart fixture that also owns
a server and a provider transcript. Building that fixture, or extending the
existing daemon tests with a no-handoff kill plus a stub provider that records
its argv and prompt, would close this gap.

## Meanwhile

The integration cases above, plus a negative control: with the completed-stage
walk disabled, all four no-redo cases fail, and they pass with it. Recorded in
PR #1420.
