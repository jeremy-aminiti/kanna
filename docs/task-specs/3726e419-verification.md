# Remote task graph verification — 3726e419

Recorded 2026-09-09 from commit `006dc35a3ca0e14ef682850d122127723786b125` plus the verification additions in this task.

## Focused passing checks

- Real two-instance relay graph: `pnpm --dir tests/remote-e2e exec vitest run ... src/task-listing-actions.e2e.test.ts -t 'reads a remote task commit graph from the owning desktop'` exited 0: **1 passed, 7 skipped**. The test writes a commit in the additional desktop's task worktree and reads that task's `/v1/tasks/{id}/graph` through the first desktop's relay client.
- Remote local-action refusal: `pnpm --dir apps/desktop exec vitest run src/composables/useAppKeyboardActions.test.ts -t 'refuses Open in IDE for a task owned by another machine'` exited 0: **1 passed, 10 skipped**. It verifies the translated warning and that no local `run_script` command is invoked.
- `CARGO_BUILD_JOBS=2 cargo clippy -p kanna-server -p kanna-task-transfer -- -D warnings` exited 0.
- `pnpm --dir apps/desktop exec vue-tsc --noEmit` exited 0.

## Known incomplete checks

- The earlier full remote-E2E wrapper reported `terminal-flow.e2e.test.ts` exit 1, but its producer assertion was truncated. Its cause is **unknown**.
- The unfiltered `task-listing-actions.e2e.test.ts` run exited 1 (5 failed, 3 passed): terminal `SCRIPT_READY` timeout; short-cursor format mismatch; relay task-events 404; singleton refusal text mismatch; and merge singleton 503. Their branch causality is **unknown**. No waiver is implied.
- `pnpm exec tsc --noEmit` from repository root exited 1 because no root `tsconfig.json` was selected; it printed TypeScript help and is not a successful typecheck. An earlier Vue typecheck wrapper also failed before execution due to an incorrect redirection path.

## Visual verification blocker

`CARGO_BUILD_JOBS=2 ./kd dev up` built and launched the task-owned `kanna-desktop` app; it connected to its daemon. The owned `kd` stack was then stopped. A full-screen capture was black, and macOS denied UI inspection with `osascript` error **-1719**: assistive access is not allowed. This is not successful visual proof. To complete visual verification, grant the running automation terminal/app **Accessibility (Assistive Access)** and **Screen Recording** permission in macOS Privacy & Security, then rerun the isolated `kd` visual lane.
