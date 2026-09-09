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
