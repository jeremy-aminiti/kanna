# New revision work across provider recovery

On staging `.16`, a review requested a new implementation revision on task
`6fc9e37a`. The revision reopened the previous implementation's provider
conversation, then quota recovery replaced its provider. The replacement
prompt declared the stage already completed and omitted the new finding.

The revision producer correctly created a new run with reviewer feedback and
`resumed_from_run_id` naming the prior implementation's conversation. It did
not write `replaces_run_id`. `resolve_completed_stage` nevertheless followed
the conversation pointer to the prior success. Quota recovery prepares its
prompt before closing the refused run, so that new, still-running revision
was transparent to the walk.

The correction follows only `replaces_run_id`, both at the initial run and at
every subsequent hop. Conversation reuse does not inherit a completed
obligation. Pointer writers, provider transcript handling, no-work termination
classifications, and the rejected-resume observer's one-shot gate are unchanged.

## Legacy compatibility verified against releases

Staging `.15` is commit `90fd52ee401dfcc900174d9564e2d9c388bec275`.
At that tag, `http_api/task_actions.rs::resume_task` accepts only running,
cancelled, or failed runs, and
`task_creator/stages.rs::prepare_stage_restart` accepts only cancelled or
failed runs for `ResumeProviderSession`. A succeeded run receives HTTP 409;
this release could not create a recovery chain from that success through this
route. Its fallback prompts have no completed-stage lookup.

Staging `.16` is PR #1420's merge,
`0a758b63d31bb94d0005a5b9adbd3fd1ffcbee4d`. That release includes both
`abb2f98a4` (admit recovery after success) and `6f47b2aaa` (write explicit
replacement provenance on every restart), followed by `8e6c3a8d8` (classify
no-work terminations). The intermediate development commits are not separate
staging releases. Thus the shipped HTTP success-recovery producer already
writes the pointer retained by this correction.

Earlier desktop recovery, before `2f38fb766`, did permit a direct PTY resume
without checking the run's status. That path in `stores/sessions.ts` called
`spawnPtySession`, and the Tauri `spawn_session` command forwarded `Spawn` to
the daemon; neither inserted a replacement stage run or conversation lineage.
A surviving original success is recognized by its own status, without walking
any pointer. This is distinct from a succeeded ancestor behind a legacy
resumed-only recovery row.

No released producer of that latter success-recovery shape was established by
this trace. Resumed-only revision rows, however, are real and demonstrated by
the incident. Rows from a custom/intermediate build or an imported history
that contain only conversation lineage remain ambiguous: they cannot inherit
an ancestor's success. This is an explicit provenance rule, not a claim that
replaying completed work would be harmless. No migration, backfill, stamp, or
speculative compatibility watermark is added.

## Integration coverage and limits

`task_creator/tests/quota_recovery/revision_recovery.rs` drives the real
`POST /v1/tasks/{id}/actions/request-revision` route against git worktrees and
a fake daemon, then feeds a provider notice through the real terminal watcher
and quota-replacement producer. It checks both the spawned command and DB
records: the new finding and ordinary revision instruction reach the fallback,
the old success is unchanged, and fallback preserves the clean committed tip,
workspace, stage, and revision count. A second HTTP resume checks the new-work
boundary at an interior hop in the replacement chain. An empty-transcript
revision runs the same sequence as the fresh-workspace comparison.

`recovery.rs::rejected_resume_after_a_success_verdict_keeps_the_no_redo_instruction`
now creates its successful-session recovery through HTTP before the real
rejected-resume observer runs. Its former hand-inserted resumed-only row did
not reproduce the shipped producer's replacement provenance. The no-redo,
original-result, and one-shot assertions remain, with both real pointers
checked on the recovery run.

Existing direct, repeated, fresh-fallback, rejected-resume, quota, spawn-failure,
linked genuine-failure, rerun, and live-session recovery controls are retained.
Daemon joins and mutation handoff waits in the new sequence are bounded.

Verification used `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1`, normal framework
temporary paths, and the worktree's private Cargo artifacts. With only the
walker correction removed, the exact resumed-revision regression failed at
the captured fallback Spawn's missing reviewer finding: the prompt instead
declared `ALREADY completed` and repeated the old success. Its preceding DB,
lineage, and clean-tip assertions passed. Restoring the correction made that
exact regression pass, including the second recovery. The exact fresh-revision
comparison also passed. Sequential `task_creator::tests::recovery::` and
`task_creator::tests::quota_recovery::` runs passed all 21 and 15 tests,
respectively, with no ignored tests. Commands targeted
`cargo test -p kanna-server --bin kanna-server`; each full module run also had
a five-minute process-group cleanup deadline. No live daemon or agent CLI was
launched by these fake-daemon tests.

`cargo clippy -p kanna-server --bin kanna-server --tests -- -D warnings`,
`cargo fmt --all`, and `git diff --check` also passed. Desktop, native app,
workspace-wide, and full-gate verification were outside this bounded slot.

These tests substitute a fake daemon and record Spawn arguments; they do not
prove delivery into a live provider context after real quota exhaustion or a
machine restart. The real-daemon/restart harness gap remains documented in
[the double-recovery note](2026-09-10-double-recovery-e2e-gap.md).
Workspace-allocation policy and copied-history quota detection are separately
owned changes and are not part of this correction.
