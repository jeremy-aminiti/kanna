# Remote task graph verification — 3726e419

Recorded 2026-09-09 from commit `006dc35a3ca0e14ef682850d122127723786b125` plus the verification additions in this task.

## Current-head disposition (2026-09-09)

The worktree is clean at `88e961be0dbb182cc8ee6a30225ed5075bffe078`.
The production two-instance WebDriver case introduced in `ab418b56` is valid
remote-projection evidence: it selects an owner task from the viewer, renders
the owner graph, invokes the real viewer Open in IDE shortcut, and asserts the
translated refusal. Its recorded result was **1/1 passed**. This evidence is
not discounted because the case lives in `apps/desktop/tests/e2e` rather than
`tests/remote-e2e`.

`88e961be0` subsequently fixed preservation of the graph's `fromRef` mode, so
the prior real UI pass does not by itself prove that current-head change. No
mock-only refusal duplicate is required. The remaining focused proof is a
current-head run of the existing real two-instance graph/refusal case, with
its full stdout/stderr kept under `.tmp/` and its actual exit status written to
an explicit `.tmp/` status file by the owning command. A detached handle that
loses its result is not evidence of a pass.

## Focused passing checks

- Real two-instance relay graph: `pnpm --dir tests/remote-e2e exec vitest run ... src/task-listing-actions.e2e.test.ts -t 'reads a remote task commit graph from the owning desktop'` exited 0: **1 passed, 7 skipped**. The test writes a commit in the additional desktop's task worktree and reads that task's `/v1/tasks/{id}/graph` through the first desktop's relay client.
- Remote local-action refusal: `pnpm --dir apps/desktop exec vitest run src/composables/useAppKeyboardActions.test.ts -t 'refuses Open in IDE for a task owned by another machine'` exited 0: **1 passed, 10 skipped**. It verifies the translated warning and that no local `run_script` command is invoked.
- `CARGO_BUILD_JOBS=2 cargo clippy -p kanna-server -p kanna-task-transfer -- -D warnings` exited 0.
- `pnpm --dir apps/desktop exec vue-tsc --noEmit` exited 0.
- The real two-instance log records **1 passed** for `CARGO_BUILD_JOBS=2 KANNA_E2E_SCREENSHOT_DIR=docs/task-screenshots/3726e419-screenshots pnpm --dir apps/desktop exec tsx tests/e2e/run.ts real/remote-task-graph-refusal.test.ts`. It starts two private desktop instances with the emulator relay, creates and commits a synthetic owner task, selects its cloud projection in the viewer, renders `remote graph visual proof`, and refuses the viewer-local Open in IDE action. The runner stopped both owned app stacks and emulators. Its outer owning-process exit file was not retained, so this is not represented as an independently captured exit-0 result.

## Known incomplete checks

- `./kd test all` has no current successful, retained-result run. The future
  canonical run must likewise retain full output and an explicit exit-status
  file under `.tmp/` before it is reported as passing.
- Current-head canonical rerun (`CARGO_BUILD_JOBS=2 ./kd test all`) fixed the
  branch-caused route-audit omission below and then failed only in the
  independent `kanna-worker` default-database baseline: `config::tests::the_default_database_is_the_workers_own_under_its_data_dir` and
  `unit::tests::the_unit_launches_against_the_resolved_database` received this
  worktree's `build.kanna/kanna-wt-task-3726e419-7.db` instead of
  `/srv/worker/kanna-worker.db`. The full output is retained at
  `.tmp/kd-test-all-current-head-rerun.log`. This task does not modify the
  worker default-DB implementation.
- The first current-head canonical attempt found and the task fixed one
  branch-caused failure: the new `GET /v1/tasks/{task_id}/graph` registration
  was absent from the LAN-auth route audit manifest. The rerun passed the
  server binary suite, including `every_registered_http_route_denies_unpaired_lan_by_default`
  (**1,417 passed, 0 failed**).
- The earlier full remote-E2E wrapper reported `terminal-flow.e2e.test.ts` exit 1, but its producer assertion was truncated. Its cause is **unknown**.
- The unfiltered `task-listing-actions.e2e.test.ts` run exited 1 (5 failed, 3 passed): terminal `SCRIPT_READY` timeout; short-cursor format mismatch; relay task-events 404; singleton refusal text mismatch; and merge singleton 503. Their branch causality is **unknown**. No waiver is implied.
- `pnpm exec tsc --noEmit` from repository root exited 1 because no root `tsconfig.json` was selected; it printed TypeScript help and is not a successful typecheck. An earlier Vue typecheck wrapper also failed before execution due to an incorrect redirection path.
- The earlier real UI run used its isolated primary (`http://127.0.0.1:25690`) and secondary (`http://127.0.0.1:33363`) WebDriver endpoints and confirmed cloud owner identity, but did not record each native window title or app PID. Its screenshots are therefore superseded as native-title identity evidence. Commit `38013d6c3` adds a shared, bound-session native-title reader using Tauri `plugin:window|title` (not WebDriver's `document.title` route); `a85e69ffb` makes this graph/refusal case require each instance's exact task/worktree title before database reset or UI interaction. A new run with that guard and a durable owning-process exit file remains required.

## Visual verification

The earlier focused two-instance native WebDriver target rendered the expected viewer UI: `remote-graph.png` shows the remote marker and the owner-created `remote graph visual proof` commit; `remote-local-action-refusal.png` shows the translated “This action is not available for a task on another machine.” warning. The test settles the WebDriver-only toast enter transition before capture; it does not alter production behavior. These captures remain useful visual artifacts, but are not accepted as title-identity proof until rerun with the guard above.
