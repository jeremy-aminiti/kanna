# Remote task graph verification — 3726e419

Recorded 2026-09-09 from commit `006dc35a3ca0e14ef682850d122127723786b125` plus the verification additions in this task.

## Focused passing checks

- Real two-instance relay graph: `pnpm --dir tests/remote-e2e exec vitest run ... src/task-listing-actions.e2e.test.ts -t 'reads a remote task commit graph from the owning desktop'` exited 0: **1 passed, 7 skipped**. The test writes a commit in the additional desktop's task worktree and reads that task's `/v1/tasks/{id}/graph` through the first desktop's relay client.
- Remote local-action refusal: `pnpm --dir apps/desktop exec vitest run src/composables/useAppKeyboardActions.test.ts -t 'refuses Open in IDE for a task owned by another machine'` exited 0: **1 passed, 10 skipped**. It verifies the translated warning and that no local `run_script` command is invoked.
- `CARGO_BUILD_JOBS=2 cargo clippy -p kanna-server -p kanna-task-transfer -- -D warnings` exited 0.
- `pnpm --dir apps/desktop exec vue-tsc --noEmit` exited 0.
- `CARGO_BUILD_JOBS=2 KANNA_E2E_SCREENSHOT_DIR=docs/task-screenshots/3726e419-screenshots pnpm --dir apps/desktop exec tsx tests/e2e/run.ts real/remote-task-graph-refusal.test.ts` exited 0: **1 passed**. It starts two private desktop instances with the emulator relay, creates and commits a synthetic owner task, selects its cloud projection in the viewer, renders `remote graph visual proof`, and refuses the viewer-local Open in IDE action. The runner stopped both owned app stacks and emulators. Screenshots are uncommitted under `docs/task-screenshots/3726e419-screenshots/`.

## Known incomplete checks

- The earlier full remote-E2E wrapper reported `terminal-flow.e2e.test.ts` exit 1, but its producer assertion was truncated. Its cause is **unknown**.
- The unfiltered `task-listing-actions.e2e.test.ts` run exited 1 (5 failed, 3 passed): terminal `SCRIPT_READY` timeout; short-cursor format mismatch; relay task-events 404; singleton refusal text mismatch; and merge singleton 503. Their branch causality is **unknown**. No waiver is implied.
- `pnpm exec tsc --noEmit` from repository root exited 1 because no root `tsconfig.json` was selected; it printed TypeScript help and is not a successful typecheck. An earlier Vue typecheck wrapper also failed before execution due to an incorrect redirection path.

## Visual verification

The focused two-instance native WebDriver target rendered the validated viewer UI. `remote-graph.png` shows the remote marker and the owner-created `remote graph visual proof` commit; `remote-local-action-refusal.png` shows the translated “This action is not available for a task on another machine.” warning. The test settles the WebDriver-only toast enter transition before capture; it does not alter production behavior.
