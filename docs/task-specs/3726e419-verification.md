# Remote task graph verification — 3726e419

Recorded 2026-09-09. Historical evidence below retains the SHA that produced it;
the published PR head is `9d834b3c920a07b1b0dbc3338c716b45cef651e9`.

## Current-head disposition (2026-09-09)

The reviewed committed tip was `3ccd9a9606ddc01b9fc57ed50fa3fde1df8a07ce`.
Publication rebased it onto current `origin/main`, producing
`9d834b3c920a07b1b0dbc3338c716b45cef651e9`. The final coverage commit has the
same stable patch id on both tips; the trees are intentionally different because
of that rebase and its compatible conflict resolutions.

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

- Main-reconciled real two-instance graph/refusal run at `f269d4b00`:
  `CARGO_BUILD_JOBS=1 KANNA_E2E_SCREENSHOT_DIR=docs/task-screenshots/3726e419-screenshots pnpm --dir apps/desktop exec tsx tests/e2e/run.ts real/remote-task-graph-refusal.test.ts` completed with recorded shell exit **0** in `.tmp/remote-task-graph-refusal-main-reconciled.exit`; full stdout/stderr is `.tmp/remote-task-graph-refusal-main-reconciled.log`. It passed **1 file / 1 test**. Before either instance received a test session action, the canonical runner preflight verified both at their distinct WebDriver endpoints (`http://127.0.0.1:31432` and `http://127.0.0.1:30894`) with task `3726e419`, worktree/branch `task-3726e419-7`, commit `f269d4b00`, and native title `Kanna — task 3726e419 · task-3726e419-7 (0.0.68 @ f269d4b00)`. The runner reported both stacks stopped, no tmux session, and no Firebase emulator window. Fresh captures are under `docs/task-screenshots/3726e419-screenshots/` and were visually inspected.
- Real two-instance relay graph: `pnpm --dir tests/remote-e2e exec vitest run ... src/task-listing-actions.e2e.test.ts -t 'reads a remote task commit graph from the owning desktop'` exited 0: **1 passed, 7 skipped**. The test writes a commit in the additional desktop's task worktree and reads that task's `/v1/tasks/{id}/graph` through the first desktop's relay client.
- Remote local-action refusal: `pnpm --dir apps/desktop exec vitest run src/composables/useAppKeyboardActions.test.ts -t 'refuses Open in IDE for a task owned by another machine'` exited 0: **1 passed, 10 skipped**. It verifies the translated warning and that no local `run_script` command is invoked.
- `CARGO_BUILD_JOBS=2 cargo clippy -p kanna-server -p kanna-task-transfer -- -D warnings` exited 0.
- `pnpm --dir apps/desktop exec vue-tsc --noEmit` exited 0.
- The real two-instance log records **1 passed** for `CARGO_BUILD_JOBS=2 KANNA_E2E_SCREENSHOT_DIR=docs/task-screenshots/3726e419-screenshots pnpm --dir apps/desktop exec tsx tests/e2e/run.ts real/remote-task-graph-refusal.test.ts`. It starts two private desktop instances with the emulator relay, creates and commits a synthetic owner task, selects its cloud projection in the viewer, renders `remote graph visual proof`, and refuses the viewer-local Open in IDE action. The runner stopped both owned app stacks and emulators. Its outer owning-process exit file was not retained, so this is not represented as an independently captured exit-0 result.

## Known incomplete checks

- Latest canonical gate on the published product head `9d834b3c9` ran with
  `GHOSTTY_SOURCE_DIR` bound to the pinned cache, `CARGO_BUILD_JOBS=1`, and
  `RUST_TEST_THREADS=1`; it exited **1**. Rust tests passed, but mock E2E
  failed 2 of 48 targets: `sidebar-task-state.test.ts` observed busy/unread as
  normal/700 rather than italic/400, and
  `terminal-output-performance.test.ts` reported an unregistered terminal
  buffer. Raw output and the owning exit are
  `.tmp/3726e419-pr-head-kd-test-all.log` and `.exit`. This canonical gate is
  explicitly **not passed**.

- Latest revision-round-4 canonical gate: `CARGO_BUILD_JOBS=2 ./kd test all`
  exited **1**. Full stdout/stderr is retained at
  `task-3726e419-9/.tmp/kd-test-all-revision4.log`; the owning shell's actual
  exit status is `task-3726e419-9/.tmp/kd-test-all-revision4.exit`. Its first failing target was
  desktop mock E2E `tests/e2e/mock/modal-tear-off.test.ts`, whose
  `modal tear-off` assertion expected repo `modal-tear-off` but received
  `modal-tear-off-startup`; the same run later also failed
  `tests/e2e/mock/terminal-output-performance.test.ts` because the terminal
  buffer was not registered. These failures are **unattributed**: the complete
  PR diff includes the modal tear-off implementation/test and the
  `TerminalView.vue`/`useTerminal.ts` runtime path. The terminal-output test
  file is unchanged, but its relevant runtime is not. No source change was
  made and this gate is not represented as passing or pre-existing.
- An **older** current-head canonical rerun (`CARGO_BUILD_JOBS=2 ./kd test all`)
  fixed the branch-caused route-audit omission below and then failed only in the
  independent `kanna-worker` default-database baseline: `config::tests::the_default_database_is_the_workers_own_under_its_data_dir` and
  `unit::tests::the_unit_launches_against_the_resolved_database` received this
  worktree's `build.kanna/kanna-wt-task-3726e419-7.db` instead of
  `/srv/worker/kanna-worker.db`. The full output is retained at
  `.tmp/kd-test-all-current-head-rerun.log`. This task does not modify the
  worker default-DB implementation. This historical worker result does not
  supersede or explain the later modal/terminal failures above.
- The first current-head canonical attempt found and the task fixed one
  branch-caused failure: the new `GET /v1/tasks/{task_id}/graph` registration
  was absent from the LAN-auth route audit manifest. The rerun passed the
  server binary suite, including `every_registered_http_route_denies_unpaired_lan_by_default`
  (**1,417 passed, 0 failed**).
- The final keyboard native control did **not** execute its test body. Its
  command was `CARGO_BUILD_JOBS=1 pnpm --dir apps/desktop exec tsx
  tests/e2e/run.ts tests/e2e/mock/keyboard-shortcuts.test.ts`; the isolated
  runner was bound to task-11's tmux session
  `kanna-e2e-task-3726e419-11-51451-1789012703356`, owner URL
  `http://172.31.32.99:37061`, and assigned WebDriver URL
  `http://127.0.0.1:21474`. Ghostty's pinned-source clone failed first with
  exit 128 (`curl 56`, connection reset). Therefore there is no test count,
  native-window identity, or successful keyboard-native result to claim.
- The earlier full remote-E2E wrapper reported `terminal-flow.e2e.test.ts` exit 1, but its producer assertion was truncated. Its cause is **unknown**.
- The unfiltered `task-listing-actions.e2e.test.ts` run exited 1 (5 failed, 3 passed): terminal `SCRIPT_READY` timeout; short-cursor format mismatch; relay task-events 404; singleton refusal text mismatch; and merge singleton 503. Their branch causality is **unknown**. No waiver is implied.
- `pnpm exec tsc --noEmit` from repository root exited 1 because no root `tsconfig.json` was selected; it printed TypeScript help and is not a successful typecheck. An earlier Vue typecheck wrapper also failed before execution due to an incorrect redirection path.
- The earlier real UI run used its isolated primary (`http://127.0.0.1:25690`) and secondary (`http://127.0.0.1:33363`) WebDriver endpoints and confirmed cloud owner identity, but did not record each native window title or app PID. Its screenshots are therefore superseded as native-title identity evidence. Commit `38013d6c3` adds a shared, bound-session native-title reader using Tauri `plugin:window|title` (not WebDriver's `document.title` route); `a85e69ffb` makes this graph/refusal case require each instance's exact task/worktree title before database reset or UI interaction. A new run with that guard and a durable owning-process exit file remains required.

## Visual verification

The refreshed focused two-instance native WebDriver captures rendered the expected viewer UI: `remote-graph.png` shows the remote marker and the owner-created `remote graph visual proof` commit; `remote-local-action-refusal.png` shows the translated “This action is not available for a task on another machine.” warning. The test settles the WebDriver-only toast enter transition before capture; it does not alter production behavior. The current captures have the canonical native identity preflight described above.

## Focused paired controls (run 2026-09-10)

The controls were run sequentially with `CARGO_BUILD_JOBS=1`, with each owned
stack stopped before the next. The canonical runner verified the bound native
task/worktree/commit/title before each target:

- Default main `90fd52ee4`, `task-3726e419-14`: `mock/modal-tear-off.test.ts`
  exited 0, **1 file / 2 tests passed**; log and exit are
  `.tmp/3726e419-main-modal-tear-off-control.log` and `.exit`.
- Published PR head `9d834b3c9`, `task-3726e419-15`: the same modal target
  exited 0, **1 file / 2 tests passed**; artifacts are
  `.tmp/3726e419-pr-head-modal-tear-off-cache-retry.log` and `.exit`.
- Default main: `mock/terminal-output-performance.test.ts` exited 0,
  **1 file / 3 tests passed**; artifacts are
  `.tmp/3726e419-main-terminal-output-performance-control.log` and `.exit`.
- Published PR head: the same terminal target exited 0, **1 file / 3 tests
  passed**; artifacts are `.tmp/3726e419-pr-head-terminal-output-performance-control.log`
  and `.exit`.

The first PR-head modal launch failed before a native window (Ghostty clone,
DNS failure, exit 128); its runner failed to return and was stopped by its
tracked process group. That raw log and status note remain at
`.tmp/3726e419-pr-head-modal-tear-off-control.log` and `.status`. A completed
default-main Ghostty checkout at the pinned `665a03f...` revision was then
explicitly supplied through `GHOSTTY_SOURCE_DIR` for the successful PR-head
retry. This is infrastructure history, not a test result. The optional
keyboard-native lane was not started: pressure was normal but only about 699 MB
was available after the paired controls. These paired passes resolve neither
the historical full-gate failure's root cause nor merge approval; approval
remains held.

## Final paired failure controls

The two targets that failed the latest canonical gate were then run together,
sequentially, under the same canonical runner/configuration on the actual
merge-base `90fd52ee4` and published PR head `9d834b3c9`. Both runners verified
their exact task/worktree/commit native titles before input; both exited **0**:
sidebar-task-state passed **1/1** and terminal-output-performance passed
**3/3**. Retained artifacts are
`.tmp/3726e419-merge-base-two-failure-control.{log,exit}` and
`.tmp/3726e419-pr-head-two-failure-control.{log,exit}`. Both owned stacks and
temporary worktrees were removed. The paired focused result is
non-deterministic reproduction evidence, not a branch-causal finding and not a
replacement for the failed canonical gate. The sole remaining acceptance gap is
one released canonical `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 ./kd test all`
result; merge remains held pending that gate's reconciliation.

## Final lane reconciliation at `2722f0304`

The original canonical `./kd test all` remains an **exit 1** result; it is not
retroactively recorded as passing. Its Rust lane failed at
`daemon_lifecycle::tests::production_spawn_path_publishes_identity_for_kd_cleanup`,
and its mock-E2E lane was never entered. The retained binary reproduced that
unit test successfully (**1/1**), after which the canonical replacement Rust
lane, `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 ./kd test rust`, completed with
exit **0** (`Canonical Rust tests passed`).

The separately released full mock-E2E lane then completed **47 of 48** targets
and exited **1** only at `mock/terminal-output-performance.test.ts` with
`terminal buffer not registered`. The exact-head focused replacement was run
once with `CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1` against the canonical
`mock/terminal-output-performance.test.ts` target. Before assertions, the
runner verified task `3726e419`, worktree `task-3726e419-13`, branch
`remote-task-graph-verification`, commit `2722f0304`, and the matching native
title. It exited **0**: **1 file / 3 tests passed**. Artifacts are
`.tmp/3726e419-final-terminal-output-performance-2722f0304.{log,exit}`.

The completed replacement Rust and focused terminal results account for the
only failed or unentered canonical lanes without rerunning the already-passed
workspace or Bazel lanes. All owned mock-E2E resources were removed. This is
ready for independent merge-master acceptance; it is not a claim that the
historical canonical whole-gate invocation itself passed.

## Original focused-control procedure

The unresolved full-gate failures require paired controls on the default branch
and the published PR head. Use fresh, isolated worktrees and retain one log and
an explicit shell-exit file per target/ref; do not reuse the historical full-gate
result as either control.

```sh
PR_HEAD=9d834b3c920a07b1b0dbc3338c716b45cef651e9
MAIN_HEAD=$(git rev-parse origin/main)
MAIN_CONTROL="$PWD/.tmp/3726e419-main-control"
git worktree add --detach "$MAIN_CONTROL" "$MAIN_HEAD"

for CONTROL in "$MAIN_CONTROL" "$PWD"; do
  LABEL=$(test "$CONTROL" = "$MAIN_CONTROL" && printf main || printf pr-head)
  (
    cd "$CONTROL/apps/desktop"
    CARGO_BUILD_JOBS=1 pnpm exec tsx tests/e2e/run.ts \
      mock/modal-tear-off.test.ts
  ) 2>&1 | tee "$PWD/.tmp/3726e419-${LABEL}-modal-tear-off.log"
  printf '%s\n' "${pipestatus[1]}" > "$PWD/.tmp/3726e419-${LABEL}-modal-tear-off.exit"
done
```

Repeat that exact loop with `tests/e2e/mock/terminal-output-performance.test.ts`
and `terminal-output-performance` in the two artifact names. Compare the two
actual exits and the failing assertion/output before making any causality claim.

For keyboard-native proof, use the same PR-head isolation and command above
only after first recording the runner's task worktree, tmux session, assigned
WebDriver URL, desktop PID/start-time/executable, and the bound-session native
title. The required title is exactly `Kanna — task 3726e419 · <worktree-branch>
(<version> @ <short-head>)`; abort before input if any identity value differs.
After the target exits, record its test count and shell exit in paired `.log` /
`.exit` files. No native launch, build, or test execution is authorized until
the stated release phrase.
