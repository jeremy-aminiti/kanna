# Terminal active-view verification — 2026-09-09

## Focused mobile relay journey — passed

Command (exit 0):

```sh
CARGO_BUILD_JOBS=2 ./kd test remote-e2e --mobile-relay-terminal-control
```

The real simulator/Appium relay journey recorded the rendered phone measurement
as `50x36`, then observed the daemon at `50x36`, and then verified the rendered
terminal at the same dimensions. It subsequently performed the separate
authenticated protocol-viewer handback and verified the fixture grid at
`132x20`. This is not evidence of a real desktop UI restoration.

The command's complete nested output is retained at
`.tmp/mobile-relay-terminal-control.log`. The simulator screenshots were
visually inspected:

- `docs/task-screenshots/5c82e022-screenshots/02-terminal-fitted-after-phone-open.png`
  shows the populated compact terminal and composer, with no Take/Release
  control button.
- `docs/task-screenshots/5c82e022-screenshots/03-terminal-restored-after-protocol-handback.png`
  shows the populated, widened post-handback grid, likewise with no control
  button.

## Real desktop target — unclassified failure

The precise runner selection was:

```sh
CARGO_BUILD_JOBS=2 pnpm --dir apps/desktop test:e2e -- real/remote-visual-companion.test.ts
```

It exited 1 after `337.96s` (six failures, one pass, one skip). The restoration
assertion did not obtain a valid remote task, so it did not establish or refute
the no-input desktop foreground handback. The failing matrix is:

| Test area | First failure |
| --- | --- |
| Relay full-screen terminal | terminal buffer not registered for session `870d25f6` |
| Active-view sizing/restoration | cloud task prompt projection mismatch |
| Paired LAN companion | account sign-in timeout |
| Paired LAN full-screen terminal | LAN task projection timeout |
| Navigation boundary | LAN task projection timeout |
| LAN input semantics | early submission observed before its expected boundary |

Relay output also recorded canonical desktop credential rejections. These are
preserved as unclassified: this task did not modify production relay or account
authentication behavior. Complete output is retained at
`.tmp/desktop-active-view-restoration.log`.

One initial command form (`test:e2e:real -- ...`) accidentally selected the
entire `real/` directory and was terminated before test execution with exit
143; it is not a test result. Its exact owned tmux session was then stopped.

## Dedicated real desktop proof — failed at the local active-view boundary

Command (exit 1):

```sh
CARGO_BUILD_JOBS=2 pnpm --dir apps/desktop test:e2e -- real/remote-active-view-restoration.test.ts
```

The isolated two-instance runner started both desktops, Firebase emulators, and
the relay, then ran only
`real/remote-active-view-restoration.test.ts`. The private owner identity
projection succeeded. The remote foreground phase passed its daemon/buffer-
dimension equality and captured `remote-active-view-controls-grid.png`, but
visual inspection found that image blank rather than a rendered terminal. It
is therefore not visual proof of the remote grid. The focus-only primary
handback then timed out at the assertion that the daemon dimensions equal the
primary rendered dimensions. No primary-restoration screenshot was produced,
so restoration is not proven.

The causal boundary is the active-view notification, not task discovery or
relay setup: `CloudTerminalView` calls `subscription.activate()` when the
remote view starts, while the owning local lifecycle only calls
`activateTerminalViewer` during attach, reconnect, and resize. Moving terminal
focus back to the already-attached local view does not emit an
`ActiveViewer` command, so the daemon correctly retains the remote controller.
This needs a real local-terminal focus-to-active-viewer notification, plus a
rendered-cell assertion that rejects the blank capture, before a fresh native
retry; no fake endpoint or authentication workaround is involved.

Complete nested output is retained at
`.tmp/desktop-active-view-restoration-isolated.log`. The runner cleaned its
owned desktop, relay, and emulator processes after its exit.

### Native-window identity audit

The failed desktop capture above predates the exact native-title guard. It is
already rejected as blank, and it also cannot establish that its WebDriver
session was bound to this task's dev window rather than a staging or production
window. It is not desktop visual proof for either reason. The phone relay
screenshots in `docs/task-screenshots/5c82e022-screenshots/` remain mobile
render records only; they make no native desktop-window identity claim.

Before any future two-instance desktop interaction, the dedicated target now
checks each already-bound WebDriver endpoint independently: compiled task id
and worktree, then the native Tauri title read through that same endpoint. The
title must exactly equal `formatAppWindowTitle(buildInfo)` and therefore name
this task worktree; an absent or mismatched identity stops the test before
reset, sign-in, focus, or capture.

## Reconciled dedicated desktop proof — renderer-precondition failure

Command (exit 1):

```sh
CARGO_BUILD_JOBS=1 KANNA_E2E_SCREENSHOT_DIR="/Users/jeremyhale/.kanna/repos/kanna-7/.kanna-worktrees/task-5c82e022/docs/task-screenshots/5c82e022-screenshots" pnpm --dir apps/desktop test:e2e -- real/remote-active-view-restoration.test.ts
```

At reconciled head `743f4076c`, the canonical runner independently verified
both bound native windows before reset: task `5c82e022`, worktree
`task-5c82e022`, branch/commit `task-5c82e022` / `743f4076c`, and title
`Kanna — task 5c82e022 (0.0.68 @ 743f4076c)`. The target then failed at its
first owner renderer precondition (before remote selection, focus handback, or
capture): daemon/buffer dimensions did not converge with a visible rendered
`ACTIVE_VIEW:` row within 30 seconds. No new screenshot was written.

The complete nested log and actual exit record are retained at
`.tmp/desktop-active-view-restoration-743f4076c.log` and
`.tmp/desktop-active-view-restoration-743f4076c.exit`. The runner reported its
own tmux session, relay, and Firebase emulator stopped; no owned process
remained afterward.

The target previously read xterm rows without invoking the existing E2E
terminal-buffer refresh used by the real visual-terminal helpers. It now
refreshes the actual terminal before each rendered-cell read and reports the
last daemon/rendered state on a failed convergence. That correction is
source-checked but not yet native-verified; the desktop restoration behavior
remains unproven.

## Reconciled renderer retry — initial local geometry mismatch

Command (exit 1):

```sh
CARGO_BUILD_JOBS=1 KANNA_E2E_SCREENSHOT_DIR="/Users/jeremyhale/.kanna/repos/kanna-7/.kanna-worktrees/task-5c82e022/docs/task-screenshots/5c82e022-screenshots" pnpm --dir apps/desktop test:e2e -- real/remote-active-view-restoration.test.ts
```

At `c03c5d68d`, both independently bound desktop windows again passed the
canonical task/worktree/commit/native-title identity check before reset. The
refresh correction produced a populated rendered `ACTIVE_VIEW:` marker, but
the first local owner convergence still failed: the daemon measured `140x50`
while the rendered local terminal measured `102x27`. This occurred before
remote-task selection, remote focus, no-input local handback, or screenshot
capture. Thus it demonstrates neither remote ownership nor desktop
restoration; it only rejects the earlier blank-render hypothesis as the sole
precondition failure.

The complete nested log and actual exit record are retained at
`.tmp/desktop-active-view-restoration-c03c5d68d.log` and
`.tmp/desktop-active-view-restoration-c03c5d68d.exit`. The runner reported its
tmux session, relay, and Firebase emulator stopped; no owned process remained
after exit. No additional retry was run.
