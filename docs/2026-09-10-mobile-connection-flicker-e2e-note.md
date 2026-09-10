# Mobile connection redraw flicker and missing-Enter — source findings, on-device pass pending

Date: 2026-09-10 · Task: 41d28cff · Host: Mac Studio

## Reconciled with main (Android dev-client PR #1419)

PR #1419 (`3f9520ae2`, parent `90fd52ee4`, reviewed `9beb64caa`) merged into
main independently of this task and was reconciled in here by merging
`origin/main` (commit `a2ee21013`) after committing the candidate above — no
conflicts. Its diff (`apps/mobile/src/screens/TaskScreen.tsx`,
`taskComposerKeyboard.ts`, `crates/kanna-server/src/http_api/ksp.rs`, plus
`tools/kd/` and docs) touches `TaskScreen.tsx` only in two places: routing
`Keyboard.addListener` event names through `taskKeyboardEventNames(Platform.OS)`
(Android/iOS use different keyboard event names) and making the composer
chrome's `onLayout` idempotent (`setComposerTop` only fires on an actual `y`
change). Neither hunk is near, or changes props passed to,
`<TerminalWebView>` — confirmed by reading the diff directly, not inferred.
No touch to `TerminalWebView.tsx`, `TerminalWebView.test.tsx`,
`terminalReconnectPresentation.ts`, or `buildTerminalDocument.ts`. The
overlay-gating fix's lifecycle assumptions (`taskChanged` detection via
`previousTaskIdRef`, the props `TerminalWebView` consumes) are unaffected by
this merge.

This PR (native Android/emulator work, merged 2026-09-10) has no bearing on
the owner's iOS OTA `2.2.3` (manifest `2026-09-09T09:33:19.447Z`) symptoms
report — it did not exist, let alone ship, at that time; nothing in this note
attributes either reported symptom to it.

Re-verified after the merge on the reconciled tree: `pnpm exec tsc --noEmit`
→ exit 0; full mobile `vitest` suite → exit 0, 1952 passed, 3 pre-existing
skips (up from 1945/1945 pre-merge, matching the tests PR #1419 itself
added). Raw logs refreshed in `.tmp/`.

## What this note is for

The owner reported (2026-09-10) quick full-screen redraws for the first few
seconds after connecting to a session on mobile, "new since last upgrade,"
plus a separate report of a missing Enter after submitting input. This note
records what source tracing and existing test coverage actually establish for
each, states plainly what remains unproven, and is explicit that no on-device
or real-renderer reproduction was run this session (native/mobile/emulator
gates are held).

## Flicker: a source-demonstrated overlay bug, not yet a proven cure

**What is established by reading the code, not by watching a device:**
`fix(mobile,relay): keep the terminal grid through a reconnect` (490c9120c /
1f3379eee) changed `TerminalWebView`'s loading overlay from "hide once
`status === 'live' && renderedOutputEpoch === outputEpoch`" to "hide once
anything has ever been painted, and never re-show it." On the very first
attach, `beginTaskTerminal(taskId, "")` seeds an empty, `connecting`-status
buffer; `buildTerminalReplaceScript` has no chunks for that revision, so it
writes `getStatusCopy("connecting")` — the literal text "Connecting to
desktop daemon..." — into xterm as a placeholder and acks
`terminal-content-ready` for it. Under the current overlay logic, that ack
alone flips `hasRenderedTerminalContent` true and hides the overlay. The real
KSP snapshot then lands as a new epoch, and `TerminalWebView` issues a second
full `__replaceTerminalState` (`term.reset()` + full rewrite) with nothing
masking it. This is a real, traceable sequence in
`apps/mobile/src/screens/TerminalWebView.tsx`,
`buildTerminalDocument.ts::getStatusCopy`/`__replaceTerminalState`, and
`state/mobileController.ts::startTaskTerminal`.

**What is a hypothesis, not a proof:** that this exact sequence is *the*
mechanism behind the owner's specific report of "quick full-screen redraws"
on their device. No on-device or WebView-renderer reproduction was performed
— only unit tests against a mocked React/bridge harness. The fix (below)
removes a real, demonstrated premature-ready transition; whether that is
sufficient to eliminate what the owner is actually seeing on their phone is
unverified. Do not read anything in this note as "fixed" until an isolated
visual verification confirms it.

**What is not established:** any claim this is "not network, not WebView
renderer, not provider" — that would require ruling out those causes by
observation, which did not happen. What is established is a distinct,
independently-real defect in the overlay's own state machine; that does not
by itself rule out a second, unrelated contributor also being present. It
also does not establish that the underlying two-phase paint (placeholder,
then real content) is itself an *excessive* redraw that should be eliminated
at the source rather than correctly masked — the placeholder-then-real
sequence is an intentional consequence of the async KSP handshake (there is
no real content to show before it arrives), so gating the overlay's own
"is there something to look at" question correctly is not, on the evidence
read here, papering over a redraw that should not happen at all. If an
on-device pass shows the redraw is still visible, or shows more than the two
phases traced here, that conclusion is wrong and needs revisiting — it is not
being asserted as settled.

**Deployed-OTA provenance — now established by content, not by date.** A
follow-up pass read the actual manifest and bundle, not the timestamp
coincidence: `curl` (public, read-only, the same headers `kd`'s own
`manifestCurl` helper in `tools/kd/src/runtime/mobile-ota.ts` uses — no
gcloud, no publish) against the staging relay's public `/ota/manifest`
endpoint with `expo-runtime-version: 2.2.3` and `expo-channel-name: staging`
returned a manifest whose `id` and `createdAt` are exactly
`3b64331c-4609-beba-25f3-d8a3397150d9` /`2026-09-09T09:33:19.447Z` — the
channel pointer for runtime `2.2.3` has not moved since, so this is the
owner's exact deployed update, not an inference. Its `launchAsset` (Hermes
bytecode, `.hbc`) was then downloaded from the manifest's own asset URL
(also public) and grepped for markers unique to 490c9120c/1f3379eee:
`isTerminalTransportGap`, `resolveTerminalPresentationStatus`,
`TERMINAL_RECONNECT_GRACE_MS`, `renderedOutputEpoch`, `beginTaskTerminal`,
and the literal string `Connecting to desktop daemon...` all appear in the
downloaded bytecode (Hermes keeps string/property-name literals intact even
though local variable names get stripped, which is exactly the pattern
observed: `reattachesSameTask`, a function-local name, does not appear, but
every exported/property-accessed identifier and every literal UI string
does). As a negative control, none of today's not-yet-published fix's own
identifiers (`pendingContentReadyRef`, `isTransportGapPlaceholder`,
`countsAsRenderedGrid`) appear in that same bundle, which is exactly what
should be true for an update published the day before this fix was written.
**This establishes the deployed OTA the owner was running does contain
490c9120c/1f3379eee's reconnect-overlay code** — it does not by itself
establish that this is what the owner saw (see above: unit-test evidence
only, no on-device reproduction). Exact commands, for reproducibility (raw
manifest and a byte count for each marker are in `.tmp/`, not committed):

```
curl --fail --silent --show-error \
  -H 'expo-protocol-version: 1' -H 'expo-platform: ios' \
  -H 'expo-runtime-version: 2.2.3' -H 'expo-channel-name: staging' \
  'https://relay-staging.kanna.build/ota/manifest'
# -> multipart body, JSON part: {"id":"3b64331c-4609-beba-25f3-d8a3397150d9", ...}

curl --fail --silent --show-error \
  'https://relay-staging.kanna.build/ota/assets?key=<launchAsset.key from manifest>&runtimeVersion=2.2.3&platform=ios' \
  -o launchAsset.hbc
grep -ac 'isTerminalTransportGap' launchAsset.hbc   # 1
grep -ac 'pendingContentReadyRef' launchAsset.hbc   # 0 (today's fix, not yet published)
```

## Fix (bounded, no new timer, no masking beyond correcting the overlay predicate)

`apps/mobile/src/screens/TerminalWebView.tsx`: `pendingContentReadyRef` (a
single `{ contentRevision, countsAsRenderedGrid } | null` slot, not a growing
map — see below) records whether the *most recently injected* replace
painted a real grid or only a transport-gap placeholder
(`isTerminalTransportGap` status — `connecting`/`restarting` — with zero
output, reusing the existing predicate from `terminalReconnectPresentation.ts`,
no new protocol field invented). `terminal-content-ready` only raises
`hasRenderedTerminalContent` when the classification says so. No delay, no
timer, no broad harness change — the gate is derived synchronously from state
already available before each injection.

**Duplicate/stale acknowledgements, using the real bridge behavior:**
`buildTerminalDocument.ts::scheduleTerminalContentReady` dedupes a scheduled
ack by comparing the revision *value* against `latestContentRevision`, not by
call identity. Two `replaceTerminalState` calls for the *same, still-current*
revision (concretely: an empty-buffer `connecting` -> `restarting` ->
`connecting` cycle before any content ever arrives — the `connection` event
in `mobileController.ts` can fire that way, and none of it bumps the output
epoch) can each independently post their own `terminal-content-ready` for
that one revision, so a duplicate ack for the current revision is real, not
invented. A revision-keyed `Map` that deletes its entry on first read gets
this wrong on the *second* ack: the entry is gone, so it falls back to a
"ready" default even for a placeholder. The implemented design is a single
slot, overwritten on every new replace (so it always reflects what the bridge
was last told to paint) and *read without deleting* on ack, so a second ack
for the same outstanding revision resolves identically to the first. A stale
ack for an *older*, already-superseded revision is unaffected and still
rejected by the pre-existing `payload.contentRevision ===
activeOutputEpochRef.current` guard. Because it is one slot rather than a map
keyed by every revision ever seen, nothing accumulates — there is no
unbounded growth to bound.

**Verified, both failure and success side, against the mocked harness:**
- `TerminalWebView.test.tsx` — "keeps the loading overlay up for a
  connecting-status placeholder paint, and only clears it once the live
  snapshot renders": confirmed this fails without the fix (reverted the gate
  check locally, reran, saw the expected failure, restored).
- `TerminalWebView.test.tsx` — "resolves a duplicate acknowledgement for the
  same outstanding revision the same way both times": confirmed this fails
  under a delete-on-read design (temporarily reintroduced a clear-on-read,
  reran, saw the expected failure, restored).
- Full mobile suite and `tsc --noEmit` after restoring the fix: both clean.

Executed this session, exact exits (raw logs in `.tmp/`, not committed):
- `pnpm exec vitest run src/screens/TerminalWebView.test.tsx --maxWorkers=2` → exit 0 (50 passed)
- `pnpm exec vitest run --maxWorkers=2` (full mobile suite) → exit 0 (1945 passed, 3 pre-existing skips)
- `pnpm exec tsc --noEmit -p .` → exit 0

Not executed: any simulator, device, or real WebView render. `cargo`/native/
emulator gates were not invoked in this session per the task's execution
hold.

## iOS simulator lane — executed 2026-09-10, build/launch verified, task-reach blocked

RESUME MOBILE CONNECTION REGRESSION VERIFICATION authorized one bounded
canonical iOS DEV simulator lane; this ran it. Result: **the build, native
runtime identity, and app launch are now verified by execution, not
assumption — but the redraw/overlay video capture itself was not obtained**,
because reaching a task's terminal screen turned out to need tooling this
session does not have and was told not to install. Reported plainly rather
than as a completed capture.

**Simulator targeted by exact UDID, never `booted`:** `1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4`
("iPhone 16 Pro Max", `com.apple.CoreSimulator.SimRuntime.iOS-18-5`) —
deliberately not one of the "iPhone 17 Pro" / iOS 26.x devices used in prior
sessions, to route around the already-diagnosed iOS 26 "Open in…?" SpringBoard
alert (`ios-simulator-builds-fail-on-mac-studio` memory) rather than
re-hitting it. iOS 18.5 was available and already installed; nothing new was
downloaded or licensed.

**Native runtime compatibility — checked before assuming, per instruction, not
assumed:** the just-merged Android PR #1419 bumped `dev.runtimeVersion`
2.2.4 → 2.2.5 in `mobileEnvironments.json` (confirmed by diffing
`90fd52ee4..3f9520ae2`), so "JS-only fix ⇒ no build" was **not** a safe
assumption on its own this time — a stale prior install would carry the old
runtime identity. Ran the full canonical path anyway (`expo prebuild` →
CocoaPods → `xcodebuild` → install → launch, via `mcp__kd-mcp__mobile_run`
with `simulator: "1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4"`), which regenerates
native config every time regardless. The on-screen Expo dev-tools overlay
after launch reads **"Kanna Dev · Runtime version: 2.2.5"** — confirmed
visually in `.tmp/mobile-flicker-capture/screen-01-pre-deeplink.png` (not
committed) — i.e. the installed binary's native identity matches current
main's config exactly, by observation.

**Exact command and exit:**
```
mcp__kd-mcp__mobile_run { simulator: "1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4" }
```
completed (ran long enough to move to background, ~120s+, finished
successfully): `"ok": true`, `"Launched Kanna mobile on iPhone 16 Pro Max
simulator. Bundle ID: build.kanna.app.dev. Metro: http://127.0.0.1:8094.
Profile: build=dev, owner=worktree, cloud=emulators. App: installed and
launched."`, `metroReadiness.afterLaunch.ok: true`. This is the FULL
canonical worktree-scoped dev flow (`kd`'s own dev-window plan starts
Firebase emulators, relay, the desktop Tauri app, `kanna-server`, and Metro
before building/installing the mobile app) — not a shortcut, and it does its
own bounded cargo build for the desktop sidecars/app as an inherent part of
the authorized lane, not an extra one stacked on top.

**Server/app identity used, exactly, for the record:** desktop
`desktopId: desktop-c3dc45eb-35ad-41a3-b447-cd02d45b2d2d`,
`environment: "development"`, `kanna-server` at `http://127.0.0.1:48128`
(this worktree's own `KANNA_MOBILE_SERVER_PORT`) — a fresh, throwaway,
worktree-scoped dev instance, not staging or production. Registered this
repo (`repo-18d3f3efcc1e6440`, `/Users/jeremyhale/.kanna/repos/kanna-2`) and
created one disposable scratch task (`cbf2d141`, a trivial "print 1–20 then
wait" Claude prompt, chosen only to have a live PTY session to attach the
terminal viewer to) directly via that dev server's own local HTTP API
(`POST /v1/repos`, `POST /v1/tasks` — both local-process-trusted, no token
needed, no cloud/account involved). Closed it
(`POST /v1/tasks/cbf2d141/actions/close`, confirmed `closedAt` set) and
removed its worktree/branch (`git worktree remove --force`,
`git branch -D task-cbf2d141`) before finishing.

**Blocked: no working way to seed a trusted desktop + selected task without
Appium.** Both existing dev seams
(`kanna://e2e-trust?desktopId=…&selectedTaskId=…` and `kanna://e2e-pair`,
`apps/mobile/src/e2eTrustSeed.ts`) check for the literal protocol `"kanna:"`.
Read the generated `ios/KannaDev/Info.plist` directly: this build registers
`kanna-dev`, `build.kanna.app.dev`, and `exp+kanna-mobile` as URL schemes —
**never bare `kanna`** — in `app.config.ts`'s scheme handling, for any
environment. So `xcrun simctl openurl <udid> "kanna://e2e-trust?…"` fails
outright with `NSOSStatusErrorDomain -10814` (`kLSApplicationNotFoundErr`,
confirmed by direct execution, both before and after dismissing the dev-tools
overlay) — this is a **different, more basic blocker than the iOS 26 alert**:
LaunchServices has no app to route the scheme to at all. The E2E harness's
own `seedTrustedDesktopThroughDeepLink` (`apps/mobile/e2e/helpers/trust-seed.ts`)
works around exactly this by calling Appium's `mobile: deepLink` with an
explicit `bundleId`, which hands the URL straight to the named app via
XCUITest and never asks LaunchServices to resolve the scheme at all — which
is why it isn't hit by this. Appium's XCUITest driver is not installed in
this environment (`pnpm exec appium driver list --installed` returns none),
and installing it was explicitly out of scope this lane ("no silent SDK/
toolchain install").

**Also tried, also blocked, and genuinely instructive: driving the app's own
JS runtime directly via React Native's built-in remote debugger, no install
needed.** Metro exposes a real CDP (Chrome DevTools Protocol) target at
`ws://127.0.0.1:8094/inspector/debug?device=…&page=1` (found via
`GET /json/list` — this is React Native's standard, always-on Fusebox
debugger, not a new tool). Connected directly with Node's already-present
`ws` package (`node_modules/.pnpm/ws@8.20.0`) and issued `Runtime.evaluate`
calls against the live app — this genuinely works (confirmed executing
arbitrary JS in the real running app's Hermes context; script in
`.tmp/cdp-eval.mjs`, not committed). The goal was to seed the same trusted-
desktop/selected-task state some other way — either by replaying the deep
link's own persisted-storage write (`kanna.mobile.context.v1` via
`@react-native-async-storage/async-storage`, per
`apps/mobile/src/state/sessionPersistence.ts`) or by directly emitting a
synthetic `"url"` event so the app's already-registered `Linking` listener
fires. Neither panned out: this build is React Native's Bridgeless/New
Architecture (`"description": "React Native Bridgeless [C++ connection]"` in
the CDP target listing) — `global.require`, `global.__turboModuleProxy`, and
`global.$$require_external` (Node-builtin shim only) are all absent or
non-functional for reaching app modules; the legacy
`global.__fbBatchedBridge` object still exists but its
`_lazyCallableModules` registry is empty (nothing to dispatch `emit` through)
and `global.__fbGenNativeModule("RNCAsyncStorage")` throws
(`TypeError: undefined is not a function`) — a real, specific vestige of the
old bridge that no longer functions once bridgeless mode is active. The one
real native-module registry reachable, `global.expo.modules`, only lists
Expo-authored modules (`ExponentFileSystem`, `ExpoDevLauncher`, etc.) —
`RNCAsyncStorage` is a community module outside that registry and was not
reachable through it either.

**Net for this lane:** build ✅ (verified, not assumed), native runtime
identity ✅ (verified by direct observation, addressing the exact "don't
assume JS-only=no build" instruction), app launch ✅, real dev
server/task/identity ✅ (created, used, and cleanly disposed of) — **but no
first-attach redraw/overlay video or frame evidence was captured**, because
every available path to programmatically select a task inside the running
app was checked and found blocked, and driving the simulator's screen by
hand was not available in this session. This is a concrete, narrower gap
than "needs a human": specifically, it needs either (a) an authorized Appium
XCUITest driver install, or (b) a human tapping "Pair a Mac" once on the
simulator screen (the QR/pairing flow itself, exercised for real, would also
be a more faithful "genuine disposable dev pairing" than the deep-link seam
this lane tried first) — not a blanket "verification requires a person."

**Cleanup performed, as instructed:** scratch task closed and its worktree/
branch removed; `mcp__kd-mcp__dev_down` stopped the full dev stack (daemon,
tmux windows for emulators/relay/desktop/mobile, terminal-recovery process —
all reported cleaned, zero failures); the simulator was shut down
(`xcrun simctl shutdown 1CCF3E78-…`, confirmed via `simctl list devices
booted` returning empty). This worktree's own git state (`49d7cc169`) was
never touched during any of this — no branch switch, no rebase, no stash —
confirmed clean before and after. Logs/screenshots preserved under `.tmp/`
(gitignored, not committed): `mobile-flicker-capture/screen-01-pre-deeplink.png`,
`cdp-eval.mjs`.

**1. Bring up the simulator dev stack, with the existing E2E terminal
instrumentation enabled** (the same `EXPO_PUBLIC_KANNA_ENABLE_E2E_TRUST_SEED`
flag the E2E harness itself sets in `apps/mobile/e2e/helpers/metro.ts`; it
also gates `webviewDebuggingEnabled`, so without it Safari's Web Inspector
cannot attach to the terminal WebView at all):

```
EXPO_PUBLIC_KANNA_ENABLE_E2E_TRUST_SEED=1 ./kd mobile run --simulator "iPhone 17 Pro"
```

`kd`'s dev-window construction (`tools/kd/src/runtime/dev-plan.ts`) prefixes
specific `EXPO_PUBLIC_*` keys onto the Metro command but never clears the
inherited shell environment first, so an exported var should reach Metro and
get inlined into the served JS the normal way — **unconfirmed by execution;
verify it took before relying on it** (open Safari → Develop → [simulator] →
find the Kanna WebView in the list; if it's not listed, the flag did not
reach Metro and the app needs a reload after fixing that before continuing).

**2. Capture frame-accurate evidence of the redraw itself** — a standard,
always-available Xcode CLI tool, no Appium and no human required for the
capture step itself (only for judging the result, which a person or a later
frame-diff pass can do from the file):

```
xcrun simctl io booted recordVideo --codec=h264 .tmp/mobile-first-attach-<before|after>.mov &
RECORD_PID=$!
# ... attach to a task terminal for the first time here (fresh app launch or
#     a task never opened this session, so beginTaskTerminal's empty/connecting
#     seed genuinely runs) ...
sleep 8
kill $RECORD_PID
```

**3. Capture the instrumented counts** (richer than the video alone):
Safari's Web Inspector console, attached to the terminal WebView found in
step 1, can read `document.querySelector('[data-testid]')`-style state, or —
simpler, since the app already renders it — the accessibility value text
nodes `terminal-loading-indications:<N>` (from `TerminalWebView`'s own
`loadingIndicationCount`, already wired to increment exactly once per
overlay-raise) and the `terminal-inspection` JSON blob (`frameCount` and
other fields from `buildTerminalDocument.ts`'s existing diagnostics) are
visible in the accessibility tree / DOM without adding any new
instrumentation. This step needs a person (or Appium, previously blocked
here per `docs/2026-09-09-mobile-terminal-reconnect-e2e-note.md`) driving
Safari's GUI — there is no CLI-scriptable Safari Web Inspector automation in
this repo today — so it is secondary evidence, not the primary ready-to-run
capture.

**4. Before/after, same app instance, JS-only so no rebuild between states:**

```
git stash push -u -m "mobile-flicker-before-state"   # isolate: pre-fix TerminalWebView.tsx
# reload the app fully (not Fast Refresh — a state-preserving refresh would
# skip beginTaskTerminal's fresh-connect path entirely) and repeat steps 2-3
# labelled "before"
git stash pop
# reload again, repeat steps 2-3 labelled "after"
```

Compare: whether the "before" recording/counts show a visible content
repaint *after* the loading overlay has already cleared (the reported
flicker, and what the fix targets), and whether "after" does not. A result
either way should update the "hypothesis, not proof" language above rather
than being silently treated as confirmation.

## Missing-Enter symptom — correction: the drain-aware fence is gone, not current

An earlier draft of this note claimed task `ed5bce3f`'s drain-aware
settle-wait fence (PR #1314, `crates/daemon/src/session.rs`) was still
current and that mobile shared its protection. **That was wrong.** Task
`d2eb7fa0` (PR #1369, "Always submit delivered task input; remove the
draft-protection hold," owner directive 2026-09-08: "The input protection is
killing me. I'd rather have collisions.") later and deliberately removed it.
Confirmed by reading the current `crates/daemon/src/session.rs` directly, not
by re-reading the older PR: `SubmissionUnproven`, the settle-wait fence, and
its consumption bound no longer exist anywhere in the tree (`grep` for
`SubmissionUnproven`/`settle_wait`/`consumption_bound` across
`crates/daemon/src` and `crates/kanna-server/src` returns nothing); the
remaining `delivery_uncertain` is now scoped to "a lost daemon round trip
only," per that commit's own message, not to terminal-settle uncertainty.

**What the current design actually is**, per `logical_message_bytes`
(`crates/daemon/src/session.rs:72`): one PTY write containing the text —
bracket-paste-framed when the terminal supports it and the message is ≥256
bytes or carries a newline — with its `\r` submission boundary appended
*in the same buffer*, written immediately, with no wait for the terminal to
prove it consumed anything.

**Existing contract coverage read (not executed — `cargo` is under the held
gate) in `crates/daemon/tests/reconnect.rs`:**
- `a_submission_boundary_is_written_even_while_the_terminal_repaints`
  (`session_id: "slow-draining-consumer"`) drives a child that keeps its
  screen continuously busy from before the delivery is made, reads its first
  PTY chunk raw/non-canonical, and asserts the `\r` arrives in the *same read*
  as the message text — i.e., not swallowed by an actively-repainting
  terminal. Its doc comment names this explicitly as coverage for "the owner
  directive of 2026-09-08, at the boundary the removed protection guarded."
- `a_long_single_line_logical_message_survives_the_pty_queue_split` replays
  `incident_shaped_message()` — sized to reproduce the original 2026-09-06
  1,227-byte-class incident — through a reader that fragments it across
  multiple reads (`FRAGMENTING_READ_SIZE`), and asserts exactly one `\r`,
  immediately following the closing paste marker, regardless of where the
  read boundary fell.
- `a_never_settling_terminal_takes_every_delivery`
  (`session_id: "never-settling-consumer"`) asserts two successive deliveries
  into a terminal that never stops drawing both land, each with its own
  boundary.

These three tests target exactly the failure mode reported (Enter lost to a
busy/slow-consuming terminal, for both a short unframed message and a large
paste-framed one), and by their bodies assert the current single-write design
does not reproduce it — **but only for what they actually exercise: that the
right bytes, in the right order, reach the PTY's read side.**
`SLOW_DRAINING_CHILD`/`NEVER_SETTLING_CHILD` are shell scripts using `dd`,
`od`, and `stty -icanon`/`min 1 time 0` — a controlled, synthetic reader that
proves byte *delivery and ordering* survive a busy screen and a fragmented
kernel-queue split. **They do not, and cannot, prove that a real agent CLI's
own input parser treats the CR immediately following the closing paste marker
as a submission while that CLI is itself mid-paste-consumption or
mid-repaint.** A real TUI's bracketed-paste handling, composer/readline state
machine, and redraw scheduling are its own code, not this test's shell
scripts — a bug there (e.g. the parser samples "am I still in paste mode" a
frame late, or the redraw loop transiently ignores stdin) would not show up
in these tests at all. **Even a full pass of all three, run to completion,
does not resolve the owner's symptom** — it only rules out a defect in the
daemon's own byte-framing and write-ordering, which is a real thing to rule
out, not the whole question. This was also **not run this session** — reading
the test bodies is what supports the framing above, not an executed pass;
`cargo test` was not invoked because the native gate is held. The corrected
invocation for when it lifts (the three names must go after `--`; multiple
bare positional `TESTNAME` args before it are not valid `cargo test` syntax):

```
cargo test -p kanna-daemon --test reconnect -- \
  a_submission_boundary_is_written_even_while_the_terminal_repaints \
  a_long_single_line_logical_message_survives_the_pty_queue_split \
  a_never_settling_terminal_takes_every_delivery
```

(libtest runs any test whose name contains any of the given filters — this
matches those three and nothing else in `reconnect.rs`.) The mobile-shared
HTTP route's own coverage in `crates/kanna-server/src/http_api/tests/input.rs`
is a separate, additional check, not a substitute for the above.

**A second, independent gap: the live CLI-contract harness's existing
`submit()` helper no longer matches the current daemon contract at all.**
`tests/cli-contract/helpers/pty.ts::PtySession.submit()` — used today by
`tests/live/opencode-injected-input.test.ts` and
`opencode-tui-status-markers.test.ts`, the only existing tests that drive a
*real* agent CLI's TUI for input submission — implements "write text, wait
150ms, then write `\r` as a separate call," citing
`crates/kanna-server/src/http_api/task_input.rs` and
`LOGICAL_INPUT_SUBMIT_DELAY_MS` in its own doc comment. Read directly:
`task_input.rs::try_submit_task_input_to_session` no longer does any of that
— it forwards the raw message to the daemon via
`send_logical_session_input` with no `\r` appended at all, and the daemon's
`logical_message_bytes` is what appends `\r` and paste-frames, in one buffer,
with no wait. `LOGICAL_INPUT_SUBMIT_DELAY_MS` still exists but is now only
the pacing gap *between two separate delivered messages*, not a per-message
CR delay. So the one existing live-CLI test that pins real submission
behavior is pinning a contract the server stopped implementing on
2026-09-08/09 — it currently proves something about a two-write, delayed-CR
policy that is not what ships. Whether it still happens to pass (OpenCode may
well tolerate either shape) is unconfirmed and beside the point: it is not
evidence about the *current* policy either way.

**Planned, not executed, bounded reproduction — provider unspecified,
runnable across all four live-tested CLIs so the owner's answer (still
pending) selects which result matters rather than this note guessing:**
1. Add `PtySession.submitLogical(text, { bracketedPasteMode })` mirroring
   `logical_message_bytes` exactly — one `write()` of paste-begin + text +
   paste-end + `\r` when framing applies (≥256 bytes or a newline, and the
   terminal advertised the mode), else text + `\r` as one write, no
   intervening wait. This replaces the stale 150ms-then-separate-write
   `submit()` for any new test, rather than extending a helper that already
   contradicts the contract it claims to pin.
2. Reuse the existing per-CLI availability/binary-discovery helpers
   (`findClaudeBinary`, `findCodexBinary`, `findCopilotBinary`,
   `findOpenCodeBinary` — already used by `helpers/*-availability.ts`) and
   `startPtySession` (already generic over `command`/`args`, not
   OpenCode-specific) to drive each installed CLI's interactive TUI, skipping
   any not installed — the same pattern `opencode-injected-input.test.ts`
   already uses, not a new harness.
3. Two scenarios per CLI, both while the CLI is actively busy/mid-turn
   (start a long-running step first, as
   `opencode-injected-input.test.ts`'s "quits immediately when the agent is
   mid-turn" test already does, then inject during it — this is what makes it
   a reproduction of "the terminal is repainting," not an idle-composer
   happy path):
   a. A short (<256 bytes, no newline) message — unframed, matching what a
      short mobile Send actually puts on the wire.
   b. A message shaped like `incident_shaped_message()` from
      `reconnect.rs` (same size class as the owner's original 1,227-byte
      report) — paste-framed, fragmenting across the CLI's own read calls.
   Assert, per CLI, that the message is actually *acted on* (a marker file
   written, or equivalent), the way `opencode-injected-input.test.ts`
   already does — not merely that bytes reached the pty, which is what
   `reconnect.rs` already covers.
4. Not run this session: spawning and waiting on real CLI turns is slow
   (the existing OpenCode tests carry 300s timeouts) and this task's
   execution hold was read as covering exactly this class of "spawn a real
   external process and wait on it" check, not only `cargo`/emulator. Ready
   to write and run on authorization; if the owner names a specific
   provider/session first, scenario (2) narrows to just that CLI rather than
   all four.

**Raw on-screen direct-typing is not ruled out either, and not for the reason
this note previously gave.** `sendTaskTerminalInput` → raw KSP bytes forwards
literal keystrokes with no synthesized `\r`, which is true, but that does not
exempt it: a real CLI's own input queue or composer state machine could still
mis-order or drop a keypress arriving mid-repaint independent of anything the
daemon synthesizes — that is a CLI-side parser risk, not a daemon-synthesis
risk, and synthesizing nothing does not prove the CLI itself handled the
keystroke correctly. This path is untested by everything cited above (all of
it is about the *logical*-input write path) and remains open.

Mobile's ordinary composer Send routes through the identical,
platform-agnostic `POST /v1/tasks/{id}/input` (`TaskScreen.tsx` →
`mobileController.sendTaskInput` → `client.sendTaskInput` →
`lanTransport.ts`/`remoteTransport.ts`) — that much routing is confirmed by
reading the client code — so whatever the daemon's logical-input path
provides or lacks, mobile inherits it; no mobile-specific bypass was found.
That is a routing fact, not a submission-outcome fact.

**Net position: not fixed, not confirmed reproducing, not ruled out.** The
current daemon design is architecturally different from — not a continuation
of — the mechanism this note previously (incorrectly) credited. Existing
`reconnect.rs` tests, on paper, assert the daemon's own byte-framing survives
the reported conditions, which is real but partial: it says nothing about
whether a live CLI's parser actually treats that CR as submit, and the one
existing live-CLI submission test pins a stale, no-longer-shipped contract.
No fix was authored for this symptom because no currently-reproducing defect
was located by any means available here. Closing this fully needs, in order
of what it would actually settle: (a) the owner's affected session/provider/
timestamp — still pending, not invented here — so the real daemon write
timeline for that delivery can be read directly; (b) running the corrected
`cargo test` subset above; (c) the planned live-CLI reproduction against
whichever provider(s) the owner's answer implicates.

## Overlap — corrected

Task `b1d685b7` ("Fix task-pull preparation submission and separate quit
command") does **not** touch `crates/daemon/src/session.rs`. Its diff against
main (reviewed directly) is `crates/kanna-server/src/http_api.rs`,
`crates/kanna-server/src/http_api/task_input.rs`,
`crates/kanna-server/src/transfer_engine/{finalize,push}.rs`, and two docs —
six files, none in `crates/daemon/`. An earlier draft of this note incorrectly
said it was actively iterating in `session.rs`; that was wrong and is
corrected here. There is no file-level *edit* overlap between that task's diff and either the
flicker fix (`apps/mobile/src/screens/TerminalWebView.{tsx,test.tsx}`, this
session's only edits) or the missing-Enter investigation (which read
`crates/daemon/src/session.rs`, `crates/daemon/tests/reconnect.rs`,
`crates/kanna-server/src/http_api/task_input.rs`, and
`tests/cli-contract/helpers/pty.ts`, editing none of them). One *read*
overlap is worth flagging even though it changed nothing: `b1d685b7` is
actively iterating on `crates/kanna-server/src/http_api/task_input.rs`
itself, so `try_submit_task_input_to_session`'s current body, read here to
establish the "no separate CR write, no wait" finding above, may already be
mid-change under that task's own work — worth re-confirming against
`b1d685b7`'s eventual committed state rather than assuming this note's
reading of that file stays current. Task `ed245f68` (Android emulator/
pairing) touches `TaskScreen.tsx` and `taskComposerKeyboard.ts` but not
`TerminalWebView.tsx` or `terminalReconnectPresentation.ts` — also confirmed
no overlap.
