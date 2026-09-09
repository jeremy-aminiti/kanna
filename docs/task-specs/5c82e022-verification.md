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

## Next focused desktop proof

The runnable selection should be a dedicated target:

```sh
pnpm --dir apps/desktop test:e2e -- real/remote-active-view-restoration.test.ts
```

It must be added to the existing runner's two-instance, emulator, relay, and
isolated-agent-provider plan, then run only after the dedicated desktop slot is
released. The fixture must create a private owner task and select its viewer
projection by `ownerDesktopId` plus `ownerLocalTaskId` from
`cloudSnapshot.terminalRefs`, as the existing isolated
`remote-task-graph-refusal.test.ts` does. The old companion suite's prompt
matching admits stale projected tasks, which is the direct prerequisite failure
above. The new target should then assert foregrounding the real primary
terminal restores its measured grid without sending terminal input.
