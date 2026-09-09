# Mobile terminal reconnect quiet-through — E2E written, not yet executed here

Date: 2026-09-09 · Task: c588e9bf · Host: Mac Studio

## What this note is for

The induced-tunnel-drop coverage for the mobile terminal reconnect behaviour
**exists and is wired into the relay lane** — see
`verifyRelayPtyTunnelDropIsInvisible` in
`apps/mobile/e2e/specs/relay/relay-task-flow.e2e.ts`, driven by
`dropRelayTunnels()` in `apps/mobile/e2e/helpers/relay-harness.ts`, and
documented under "Terminal reconnect quiet-through (induced tunnel drop)" in
`apps/mobile/e2e/terminal-streaming-coverage.md`.

It has **not been executed to a pass on this machine**. This note records why,
what was proven instead, and what would make the lane runnable here, so the
next person does not rediscover the same two blockers.

## What was proven on the simulator

`./kd mobile run --simulator "iPhone 17 Pro"` completed: prebuild, CocoaPods,
Xcode build (`Build Succeeded, 0 error(s)`), install, and launch. Metro then
logged `iOS Bundled 7466ms apps/mobile/index.js (1356 modules)` for that
device, which is the only trustworthy proof the app is running this branch's JS
rather than a cached bundle. The app shell rendered (screenshots under the
in-worktree, gitignored `docs/task-screenshots/c588e9bf-screenshots/`).

Reaching a *task terminal* needs a paired desktop, which is what the relay lane
provides and which is exactly what stalled.

## Blocker 1 — Appium boot outruns a 15s wait under load

`waitForLocalAppiumServer` (`apps/mobile/e2e/helpers/appium.ts`) allows 15s.
Measured here at load average ~60: Appium 2.19.0 took **~40s** to answer
`GET /status`. The lane died with:

```
Timed out after 15000ms while waiting for Appium server on port 4733; last attempt: fetch failed
```

Worked around by leaving a server listening on 4733 before starting the lane —
the runner's own spawn then loses the port, `waitForLocalAppiumServer` finds the
existing one, and the run proceeds. A durable fix is to raise that timeout (or
make it configurable); it is left alone here because it belongs to the E2E
runner rather than to this task.

## Blocker 2 — the app shell never becomes visible in the relay lane

With Appium up, no emulator port collisions, and every harness process healthy
(kanna-server, daemon, relay, Firebase auth/firestore all started and served),
the lane still ends at:

```
Kanna's mobile app shell did not become visible after Expo startup overlays were handled.
WebDriverError: An attempt was made to operate on a modal dialog when one was not open when running "alert/text"
```

The `alert/text` errors are the signature of a confirmation living *outside* the
app under test, which Appium cannot see or accept. Note one asymmetry worth
chasing first: `openSimulatorDevelopmentClient` points the dev client at
`http://127.0.0.1:<metroPort>`, while a manual launch on this host only reached
the shell when pointed at the advertised LAN address (the Expo launcher listed
`http://192.168.64.1:8102` as reachable and `http://127.0.0.1:8100` as not).
Whether the lane's failure is the SpringBoard confirmation, the loopback Metro
address, or both is the first thing to determine.

## What covers the behaviour meanwhile

The reconnect presentation is timing, not transport, and is pinned by unit
tests that do not need a device:

- `apps/mobile/src/screens/terminalReconnectPresentation.test.ts` — the pure
  derivation: a sub-threshold gap over a rendered grid presents as `live`; a gap
  past the grace, a gap with nothing rendered, and `closed`/`error` all present
  the truth.
- `apps/mobile/src/screens/TaskScreen.test.tsx` — drives the grace timer with
  fake clocks: a graced gap shows no overlay and no badge and passes `live` to
  the terminal; an outlasting gap raises the badge *over* a still-mounted
  terminal rather than replacing it with a skeleton.
- `apps/mobile/src/screens/TerminalWebView.test.tsx` — a reconnect snapshot
  keeps the rendered grid and raises no second loading indication.
- `apps/mobile/src/state/sessionStore.test.ts` — re-attaching the same task
  keeps the grid and its epoch; a different task still clears them; a compacted
  buffer is still discarded.
- `services/relay/test/router.test.ts` — the tunnel keepalive pings both legs,
  stops on close, and stops on a backpressure teardown.

## Human pass still required

This is a UI-feel change (what the reader sees across a reconnect, and when).
Per `AGENTS.md`, simulator verification is necessary but not sufficient for
feel: it wants a human on-device pass. The change is JS-only —
`runtimeVersion` is unchanged — so it is OTA-deliverable to a staging build for
that pass without a device rebuild.
