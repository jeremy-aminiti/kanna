# Shortcut-context verification — 2026-09-09

Checkpoint: 5db2cc03197ae5280e0b027c86ff4dc5bbc52e36. No source changes during verification; implementation worktree was clean at that checkpoint. This record is added by the commit post.

## Review handoff

FULL GATE HOLD for the reviewer until the manager sends exactly
`RESUME FULL GATE SHORTCUT-REVIEW`. Source review and reading this evidence may
proceed immediately; do not repeat heavy builds under the hold. Machine resources
(29696721) takes the freed full slot beside event-watch (2c7a34b9).

The aggregate gate is **not green**. The isolated sidebar pass does not establish
a cause for the broad-run failure or waive it. The worker rerun with its database
override removed also does not replace the failed canonical result.

Screenshots are preserved in the implementation worktree under
`docs/task-screenshots/7536d037-screenshots/`, as required by AGENTS.md. They are
gitignored and are not committed or carried into a new stage worktree; the complete
written observations below are the durable evidence after that worktree is removed.

## Observed app behavior

- `mock/main-tabs.test.ts`: 14/14 passed, including actual global keyboard events opening rendered help. Explorer, file preview and diff each show their title and relevant commands. Already-mounted tab switches update context. Closing the active file selects diff, closing diff selects explorer, closing explorer restores the agent/Main context. New Task overrides the underlying explorer and retains its captured context when toggling all/context mode.
- Separate native app launched with `CARGO_BUILD_JOBS=2 KANNA_E2E_NO_ACTIVATE=1 KANNA_E2E_TEST_SQL=1 ./kd dev up --db kanna-test-shortcut-visual.db`, using assigned ports (frontend 1431, server 48131, WebDriver 4467).
- Inspected native screenshots: `shortcut-explorer.png`, `shortcut-preview.png`, `shortcut-diff.png`, `shortcut-main.png`, `shortcut-overlay.png`, and explorer return. Explorer shows Filter, Enter dir / Open file, Go to parent and Yank path; preview shows search, line numbers and Markdown toggles; diff shows search, scope and filter controls; agent tab shows Keyboard Shortcuts/full Main command list; New Task shows New Task Shortcuts above explorer.
- First 1440x1000 physical-pixel captures clipped tall menus in the small Retina viewport. Repeated at 2560x1800 physical pixels and inspected complete menus. No layout changes made; small-window clipping is outside this context-selection fix.

## Checks

- Retained original 41/41 targeted frontend test result; did not rerun those three files.
- Non-desktop workspace: `CARGO_BUILD_JOBS=2 pnpm exec turbo test --concurrency=2 --filter='!@kanna/desktop'`: 18 tasks passed.
- Desktop remainder: `pnpm --dir apps/desktop test --exclude '**/useAppKeyboardActions.test.ts' --exclude '**/useMainTabs.test.ts' --exclude '**/KeyboardShortcutsModal.test.ts'`: 189 files / 1859 tests passed.
- `pnpm --dir apps/desktop exec vue-tsc --noEmit`: passed. Documented root `pnpm exec tsc --noEmit` printed usage (no root tsconfig) rather than checking a project.
- `cargo fmt --all -- --check`: passed.
- `CARGO_BUILD_JOBS=2 bazel build --jobs=2 //crates/daemon:daemon_build_script`: passed. Bazel server shut down afterward.
- `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 ./kd test rust`: protocol types, production frontend build, sidecars and strict workspace Clippy passed. Workspace test summaries: 2481 passed, 2 failed, 10 ignored. Stopped at kanna-worker, so subsequent workspace/doctest targets were not reached.
- Two worker failures: `config::tests::the_default_database_is_the_workers_own_under_its_data_dir` and `unit::tests::the_unit_launches_against_the_resolved_database`. Both received inherited task KANNA_DB_PATH instead of expected /srv/worker/kanna-worker.db. `env -u KANNA_DB_PATH CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 cargo test -p kanna-worker --bin kanna-worker`: all 15 passed. No unrelated worker edits.
- Ran the unreached daemon lane directly: `CARGO_BUILD_JOBS=2 cargo test -p kanna-daemon -- --test-threads=1`: 831 passed, 4 ignored.
- Remaining 47 mock-app files through `pnpm --dir apps/desktop test:e2e mock/...` (main-tabs excluded to preserve its result): 46 files passed / 1 failed; 173 tests passed / 1 failed / 3 skipped. Only failure: sidebar-task-state typography (busyUnread expected italic/400, got normal/700). Fresh isolated rerun `CARGO_BUILD_JOBS=2 pnpm --dir apps/desktop test:e2e mock/sidebar-task-state.test.ts`: 1/1 passed. Original broad-run failure remains recorded; no fully green aggregate gate claimed.
- Initial broad-app invocation mistakenly supplied tests/e2e/mock paths, which the harness prefixed again; no tests collected. Stopped it and its stack, corrected paths, then ran the above valid suite.

## Cleanup and coordination

All owned dev stacks, E2E stacks and build/test processes stopped. Final process scan found no running worktree binaries or owned Node/Cargo/tmux processes; `./kd dev status` reports not running. No push, PR, or stage completion verdict was recorded during implementation verification. The commit post reports repository-bookkeeping completion separately.

The fix remains exactly three files: useAppKeyboardActions.ts, useAppKeyboardActions.test.ts, tests/e2e/mock/main-tabs.test.ts. The mock/main-tabs file overlaps stage-terminal separation task 09b6a82a and needs semantic reconciliation. No stage-terminal changes adopted.
