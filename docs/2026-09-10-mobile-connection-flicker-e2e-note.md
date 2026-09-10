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
each, states plainly what remains unproven, and is updated in place across
this task's sessions as work landed — a bounded iOS DEV simulator lane was
later authorized and executed (below: build/runtime/launch verified, a real
first-attach redraw capture still not obtained), and a real bug in the
simulator lane's own deep-link seam was found and fixed along the way. Read
each section's own "executed"/"not executed"/"authored, not run" language as
current for its own claim rather than assuming the whole note is still at its
original all-source-only state.

**Status as of this pass:** the `e2e-trust`/`e2e-pair` scheme fix
(`6fee7e01c`) was independently rechecked against the currently-merged
`main` (`3f9520ae2`, the Android PR) — every consumer of its exports
(`apps/mobile/src/e2eTrustSeed.ts`, `.test.ts`, and its one real caller
`appModel.ts`; confirmed by grep, nothing else in the tree references them)
is intact, and the one PR #1419 line that touched a file this fix also
touches (`mobileEnvironment.ts` gaining an unrelated `androidPackageId`
field) leaves the `scheme` field this fix reads untouched. The Codex
reproduction (`tests/live/codex-logical-submission.test.ts`) has now run
five times total across three lane authorizations — **runs 1–4 failed on a
harness readiness precondition (a `codex_apps` MCP-server-boot race, now
narrowed and fixed via `--disable apps`, confirmed via `codex features
list` and verified absent from run 5's own transcript); run 5 still failed,
but its cause is marked unattributed rather than pinned on the same
harness gap — the missing-Enter symptom is not fixed, not reproduced, and
not ruled out by any of the five.** Composer-readiness detection is solid
(runs 2 and 3 each independently falsified a different single-regex
approach); the MCP-boot race is closed. The simulator
first-attach lane has since run for real (attempt 2, below): the
`6fee7e01c` scheme fix is now live-confirmed against an actual OS-level
open, and the real disposable-pairing sequence (`POST /v1/pairing/sessions`
→ `e2e-trust` → `e2e-pair`) works end to end — but the redraw capture itself
is still not obtained, blocked by one precisely-identified new gap: Expo's
own first-run dev-tools tip needs a real tap, and this session's computed
`click at` was refused by macOS with an Accessibility-permission error
(`-25204`), not something this session can grant itself. The still-unknown
original incident provider/session remains a separate information gap
(owner-only) that does not block either of these independent checks. A
separate status investigation (`85cf3efb`) is noted as read-only for now —
this task's flicker/missing-Enter scope is unchanged by it, and any reuse
between the two is a coordination question, not something to fold in here
unilaterally.

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

**Blocked this run, then fixed as a concrete bug: `apps/mobile/src/
e2eTrustSeed.ts` hardcoded the literal protocol `"kanna:"`, which no dev or
staging build ever registers.** Both existing dev seams
(`kanna://e2e-trust?desktopId=…&selectedTaskId=…` and `kanna://e2e-pair`)
checked `parsed.protocol !== "kanna:"`. Read the generated
`ios/KannaDev/Info.plist` directly: this dev build registers `kanna-dev`,
`build.kanna.app.dev`, and `exp+kanna-mobile` as URL schemes — never bare
`kanna` — because `app.config.ts` sets `scheme: appEnvironment.scheme`, which
is `kanna-dev` for dev and `kanna-staging` for staging
(`mobileEnvironments.json`). **Correction to an earlier draft of this note:**
it is not true that no build ever registers bare `kanna` — production's own
`appEnvironment.scheme` *is* the literal `"kanna"`, so this bug was silent
there; it only breaks dev and staging, which is exactly where E2E/simulator
verification happens. So `xcrun simctl openurl <udid> "kanna://e2e-trust?…"`
against this dev build failed outright with `NSOSStatusErrorDomain -10814`
(`kLSApplicationNotFoundErr`, confirmed by direct execution, both before and
after dismissing the dev-tools overlay) — a **different, more basic blocker
than the iOS 26 alert**: LaunchServices had no app to route the scheme to at
all. The E2E/Appium harness's own `seedTrustedDesktopThroughDeepLink`
(`apps/mobile/e2e/helpers/trust-seed.ts`) never hit this because `mobile:
deepLink` hands the URL straight to a named bundle id via XCUITest and never
asks LaunchServices to resolve the scheme — it was masking the bug, not
avoiding a different one.

**Fixed in this task, same session, with causal tests** (see
`apps/mobile/src/e2eTrustSeed.ts`, `apps/mobile/src/e2eTrustSeed.test.ts`,
`apps/mobile/src/appModel.ts`): `installE2eTrustSeedHandler` now takes an
optional `scheme` — the resolved environment's own registered scheme,
threaded from `appModel.ts`'s already-resolved
`resolveMobileAppEnvironment(extra?.appEnv).scheme` — and
`matchesE2eProtocol` accepts either that scheme *or* the legacy literal
`"kanna:"`, never only one. This preserves the Appium/legacy harness exactly
as it worked before (it keeps sending literal `"kanna://"`, still accepted)
while making the same deep link additionally reachable through a real
OS-level open on dev/staging, which the literal alone never was. Preferred
`e2e-pair` over `e2e-trust`-only per instruction: `e2e-trust` alone seeds a
`TrustedDesktopRecord` with no `deviceSecret` — trust without a credential —
while `claimPairingPayloadFromUrl` (`e2e-pair`) calls
`controller.pairMachineByPayload`, the real pairing claim; a genuine
disposable pairing for a future lane should send both. No native scheme was
added — this only changes which of the *already-registered* schemes the
existing JS-level Linking listener accepts, and no second test harness was
created (same `e2eTrustSeed.test.ts` file, extended). Four new tests added,
two confirmed causal (temporarily reverted the fix, reran, saw the expected
failure, restored — same method as the `TerminalWebView.tsx` fix above); full
mobile suite re-run clean (1956 passed, 3 pre-existing skips, up from 1952).
**This was not run against a real simulator this session** (the native/
simulator gate is held pending another task's foreground lane) — the fix is
authored and unit-tested, not yet verified end-to-end against
`xcrun simctl openurl`.

**Separately, and worth being exact about: the `mobile_run` call this lane
actually executed did not set `EXPO_PUBLIC_KANNA_ENABLE_E2E_TRUST_SEED=1` at
all.** It was invoked as `mcp__kd-mcp__mobile_run { simulator: "…" }`, an MCP
tool call with no environment-variable parameter and no prior `export` in a
shell `kd` itself would inherit — so even setting that flag aside, this run
could not have reached the trust-seed handler (gated on that exact env var in
`App.tsx`) or `webviewDebuggingEnabled`, regardless of the scheme bug. An
earlier draft of this note described a *hypothetical* future command with
that flag prefixed on a shell invocation of `kd mobile run`; it must not be
read as describing what this executed run actually did. The next canonical
invocation needs the flag actually set — e.g. `EXPO_PUBLIC_KANNA_ENABLE_E2E_TRUST_SEED=1
./kd mobile run --simulator <udid>` from a shell, or an equivalent explicit
env parameter if driving it through `mcp__kd-mcp__mobile_run` again — and
should confirm it took (Safari → Develop → [simulator] lists the Kanna
WebView only when `webviewDebuggingEnabled` is true) rather than assuming it
did.

Appium's XCUITest driver is still not installed in this environment
(`pnpm exec appium driver list --installed` returns none), and installing it
remains out of scope — but with the scheme fix above, **a next simulator
lane does not need it**: `xcrun simctl openurl <udid> "kanna-dev://e2e-trust?…"`
(the dev build's own real scheme) should now reach the handler on its own,
once the trust-seed flag is actually set for that run.

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
app was checked and found blocked this run. The root cause was not "no tool
exists" but a real, now-fixed bug in the JS deep-link seam itself (above) —
so this does not leave the next attempt waiting on Appium or a human: it
needs the trust-seed flag actually set for that invocation (also above) and
the fixed seam exercised against a real simulator, which this session did not
get to do (native/simulator gate held for another task's foreground lane).

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

**Next simulator lane, prepared and ready to run — held on Studio load, not
on a predecessor task anymore.** The specific predecessor that previously
held the Mac Studio's foreground finished; that hold is lifted. As of this
writing the Studio itself is reported ~90% busy, which blocks starting a new
heavy/native launch this minute regardless — waiting on the next available
foreground lane (manager-tracked; a "MOBILE VERIFICATION LANE CLEAR" signal
releases it), not on any further human input. This procedure supersedes the
pre-execution draft this section used to carry, which named a generic
`"iPhone 17 Pro"` device, `simctl io booted` (exactly the generic `booted`
targeting this task was told never to use), and a blanket `git stash`
baseline switch:

**1. Bring up the simulator dev stack with the trust-seed flag actually set**
(the executed run above did not set it — see the correction two paragraphs
up — this is what fixes that, not a hypothetical):
```
EXPO_PUBLIC_KANNA_ENABLE_E2E_TRUST_SEED=1 \
  ./kd mobile run --simulator 1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4
```
(or the equivalent explicit env parameter if driving it through
`mcp__kd-mcp__mobile_run` again rather than a shell). Verify it actually took
— open Safari → Develop → [simulator] and confirm the Kanna WebView is
listed (`webviewDebuggingEnabled` is gated on the same flag) — before relying
on it, exactly as flagged above.

**2. Reach the task's terminal using the now-fixed seam, with a genuine
disposable pairing — not trust-seeding alone.** `e2e-trust` alone writes a
`TrustedDesktopRecord` with no `deviceSecret`
(`apps/mobile/src/e2eTrustSeed.ts::seedTrustedDesktopFromUrl` — confirmed by
reading its `persistence.save()` call, which has no `deviceSecret` field at
all). A real credential comes only from `e2e-pair`
(`claimPairingPayloadFromUrl` → `controller.pairMachineByPayload` →
`store.upsertTrustedDesktop`, an upsert keyed by `desktopId` — confirmed in
`apps/mobile/src/state/mobileController.ts`), which needs an actual pairing
payload. The dev server issues real, disposable ones on its own local HTTP
API — this is the exact mechanism `apps/mobile/e2e/helpers/relay-harness.ts::
createHarnessPairingSession` already uses, not a new one:
```
curl -s -X POST http://127.0.0.1:<KANNA_MOBILE_SERVER_PORT>/v1/pairing/sessions
# -> {"code": "...", "pairingPayload": "...", ...} — local-process-trusted,
#    no token needed; each call issues a fresh, single-use session.
```
Sequence: `e2e-trust` first (it upserts the initial record and presets
`selectedRepoId`/`selectedTaskId` so the app lands directly on the task,
without a device secret yet), then `e2e-pair` (its upsert adds the real
credential onto the same `desktopId` — sending them in the other order would
have the trust-only write clobber the credential the pairing call just
established):
```
xcrun simctl openurl 1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4 \
  "kanna-dev://e2e-trust?desktopId=<id from /v1/status>&displayName=Dev&lanBaseUrl=http%3A%2F%2F127.0.0.1%3A<port>&selectedRepoId=<repo>&selectedTaskId=<task>"

xcrun simctl openurl 1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4 \
  "kanna-dev://e2e-pair?payload=$(python3 -c 'import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1]))' '<pairingPayload from above>')"
```

**3. Capture frame-accurate evidence of the redraw itself**, targeted at the
same exact UDID, never `booted`:
```
xcrun simctl io 1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4 recordVideo \
  --codec=h264 .tmp/mobile-first-attach-<before|after>.mov &
RECORD_PID=$!
# ... attach to the task's terminal for the first time here ...
sleep 8
kill $RECORD_PID
```

**4. Capture the instrumented counts** (richer than the video alone): Safari's
Web Inspector console, attached to the terminal WebView found in step 1, can
read the accessibility value text nodes `terminal-loading-indications:<N>`
(`TerminalWebView`'s own `loadingIndicationCount`) and the
`terminal-inspection` JSON blob (`frameCount`, from
`buildTerminalDocument.ts`'s existing diagnostics) without adding any new
instrumentation. This step needs a person (or Appium — still not installed
here, per `docs/2026-09-09-mobile-terminal-reconnect-e2e-note.md`) driving
Safari's GUI; there is no CLI-scriptable Safari Web Inspector automation in
this repo today, so it is secondary evidence, not the primary capture.

**5. Before/after, bounded and reversible, not a blanket `git stash`:** the
only file in question is `apps/mobile/src/screens/TerminalWebView.tsx`. Copy
it aside, apply the exact revert this task verified fails without the fix
(see "Verified, both failure and success side" above — the same edit that was
applied and restored to prove the unit tests causal), reload the app fully
(not Fast Refresh — a state-preserving refresh would skip
`beginTaskTerminal`'s fresh-connect path entirely) and repeat steps 3–4
labelled "before"; restore the file from the copy (never `git checkout`/
`stash`/branch switch on a running app's worktree) and reload again for
"after". This task's own committed git state must not move during any of it.

Compare: whether the "before" recording/counts show a visible content
repaint *after* the loading overlay has already cleared (the reported
flicker, and what the fix targets), and whether "after" does not. A result
either way should update the "hypothesis, not proof" language above rather
than being silently treated as confirmation.

## iOS simulator lane, attempt 2 — executed, real progress, new concrete blocker

Lane cleared (Studio ~22% busy) and predecessor hold lifted; ran this for
real, same exact UDID (`1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4`), with
`EXPO_PUBLIC_KANNA_ENABLE_E2E_TRUST_SEED=1` actually set this time (plain
shell prefix on `./kd mobile run --simulator <udid>`, not the MCP tool call —
correcting attempt 1's gap). Build succeeded again (`Build Succeeded`, `0
error(s)`), installed and launched. Same `desktopId:
desktop-c3dc45eb-35ad-41a3-b447-cd02d45b2d2d`, `environment: development` —
this worktree's persistent dev DB, confirmed by direct `/v1/status` read, not
assumed. Registered repo `repo-18d3f3efcc1e6440` (already present from
attempt 1's persisted DB) and created one fresh disposable scratch task
(`a987bd3a`), closed and its worktree/branch removed at the end, same as
attempt 1.

**The `6fee7e01c` scheme fix is now confirmed against a real OS-level open,
not just unit tests:** `xcrun simctl openurl 1CCF3E78-… "kanna-dev://e2e-trust?…"`
exited 0 — no more `NSOSStatusErrorDomain -10814`. This is the first live
confirmation that the fix actually resolves the scheme correctly.

**New, real blocker found and precisely characterized: `xcrun simctl openurl`
raises a native "Open in 'Kanna Dev'?" confirmation dialog that has no
`ctx.skip`-style bypass** — this is a genuine OS-level cross-app-open gate,
and it appeared here **on iOS 18.5**, not only the iOS 26 this task's memory
notes previously attributed it to; that earlier attribution was too narrow
and is corrected here. Screenshot: `.tmp/mobile-flicker-capture/screen-02-after-trust-seed.png`
(not committed).

**Found a real, legitimate way to answer *that specific* dialog, using only
already-present macOS capabilities (no install):** `osascript -e 'tell
application "Simulator" to activate'` then `osascript -e 'tell application
"System Events" to keystroke return'` accepts the dialog's default ("Open")
button — `keystroke` against a native `UIAlertController`-backed system
dialog works without any special permission grant. Used this to dismiss
both the `e2e-trust` and the subsequent `e2e-pair` dialogs. After
`e2e-trust` alone, the app correctly left the "Connect Kanna on your Mac"
pairing screen and showed "Tasks" / "No tasks yet" — trust-seeding visibly
took effect. Sent the real, disposable pairing claim next, exactly as
planned: `POST /v1/pairing/sessions` → `{"code":"1C52D8","pairingPayload":
"KANNA1:DESKTOP-C3DC45EB-…:1C52D8", …}`, then `kanna-dev://e2e-pair?payload=<encoded>`
— exited 0, dialog dismissed the same way.

**Second, different blocker, also now precisely characterized, where the
attempt stopped:** the task list still read "No tasks yet" after pairing,
because Expo's own first-run "Dev tools" tip overlay (unrelated to Kanna's
own code — it is the Expo dev-launcher's onboarding sheet) was still
covering the bottom of the screen and its "Continue" button is a custom
in-app view, not a native alert:
- `keystroke return` against it — no effect (confirmed via screenshot,
  unchanged). Native-alert default-button activation does not extend to
  arbitrary app UI.
- `key code 53` (Escape) — no effect.
- A computed tap — `System Events`' generic `click at {x, y}` UI-element
  scripting — failed outright with `execution error: … error -25204`, a
  macOS Accessibility-permission denial for the process driving this
  session.

**Checked before treating that as human-only, per instruction, rather than
assumed:** searched this session's available tools for a supported native
computer-use/click capability separate from `osascript`/System Events
(`ToolSearch` for `computer`/`screen_click`/`mouse`/`ui_automation`/
`accessibility`). The only "computer use"-shaped tool present is
`mcp__claude-in-chrome__computer` — its own description scopes it to "a
mouse and keyboard to interact with a web browser" and requires a browser
`tabId`; it has no path to a native macOS app window (Simulator.app is not
a Chrome tab). No other session tool exists that could click here without
the same Accessibility gate. Also checked, read-only, which process that
gate applies to, rather than guessing: `ps` on this shell's full ancestry
resolves to `/bin/zsh` → the `claude` CLI → `kanna-daemon` →
`kanna-desktop`, rooted at **`/Applications/Kanna Staging.app`** — macOS's
TCC "responsible process" resolution attributes an Accessibility check for
any subprocess in that tree to that root app bundle, which is the
mechanism, not a guess. Tried to confirm directly from TCC's own records
(read-only: `log show --predicate 'subsystem == "com.apple.TCC"'` returned
nothing observable, and `sqlite3 -readonly` against the per-user
`TCC.db`'s `kTCCServiceAccessibility` rows returned zero entries — exit 0,
genuinely empty, not an error) — consistent with **Kanna Staging.app never
having been added to the Accessibility list at all** (a -25204 refusal for
an app with no entry, rather than one explicitly denied), but not a
line-item confirmation of the exact display name TCC would show. No
permission was requested, granted, or otherwise changed by any of this
checking.

Screenshots preserved (not committed):
`.tmp/mobile-flicker-capture/screen-0{2..7}-*.png` — trust-seed dialog,
post-accept ("Tasks"/"No tasks yet"), post-pair (same, dev-tools overlay
still present), and the three dismissal attempts (return/click/escape), all
showing the identical unchanged overlay except where noted.

**Net for attempt 2 — pairing/scheme outcomes stand as passed, only the
final visual step is blocked:** the scheme fix is live-confirmed (`exit 0`
on a real `simctl openurl`, no more `-10814`), native "Open in…?" dialogs
have a real, repeatable, permission-free dismissal method
(`keystroke return`), and the real disposable-pairing sequence (`POST
/v1/pairing/sessions` → `e2e-trust` → `e2e-pair`) both completed
successfully end to end — none of that is in question. What remains is
exactly one step: dismissing Expo's own first-run dev-tools tip, which
needs a real tap this session's tools cannot deliver. **The dev/simulator
stack was torn down at the end of this attempt** (scratch task closed and
its worktree/branch removed, `dev_down` — zero failures, simulator shut
down, confirmed via `simctl list devices booted` returning empty) — so
there is currently no running dialog for anyone to dismiss. The manual step,
for whenever the stack is next brought up for this same check: either grant
`Kanna Staging.app` Accessibility access (System Settings → Privacy &
Security → Accessibility) so `click at` can complete this and future
capture attempts unattended, or — needing no permission change at all —
have a person tap "Open" on the two "Open in 'Kanna Dev'?" prompts (or let
this session's `keystroke return` continue answering those, as it did here)
and tap "Continue" once on the Expo dev-tools tip; the app then shows the
selected task's terminal directly, since the trust/pair seeding already
presets it. This worktree's own git state was not touched during any of
it.

**Independent confirmation from the coordinating manager session:** its own
attempt at a supported native computer-use-agent call to control the
Simulator app (`getApp Simulator`) also failed — `error -10005
timeoutReached`, no successful control — a second, independent tool hitting
the same underlying wall from a different angle, not just this session's
`osascript`/System Events path. It also attempted a push notification to
the owner about this blocker; the relay call returned no result before its
session stopped, so delivery is **unknown, not confirmed** — recorded here
as such rather than assumed sent, and this session is not sending a
duplicate blind notification on top of an unconfirmed one. Net: the blocker
is durable and doubly independently confirmed — owner can grant `Kanna
Staging.app` Accessibility access, or arrange the three manual taps on the
next DEV launch, whichever is preferred. No permission changes have been
made by either session. Current stack remains shut down, so there is
nothing running to click right now regardless.

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

**Bounded reproduction — authored and unit-verified this session, not run
against a real CLI turn.** Source/test authoring for this was released with
an explicit "no live CLI turns" bound; two minimal cases were prepared
(Claude and Codex — not all four), matching the manager's narrowing. The
owner's provider for this mobile incident is still unknown; preparing Claude
and Codex is not a claim about which one it was, only the two providers this
harness can currently drive interactively at all.

1. **Added** `logicalMessageBytes(text, bracketedPasteMode)` and
   `PtySession.submitLogical(text, bracketedPasteMode)` to
   `tests/cli-contract/helpers/pty.ts`, mirroring
   `crates/daemon/src/session.rs::logical_message_bytes` *and* the
   trailing-newline trim `task_input.rs::task_input_message` applies first —
   one `write()` of paste-begin + text + paste-end + `\r` when framing
   applies (≥256 UTF-8 bytes or a newline, and paste mode advertised), else
   text + `\r`, no intervening wait. The older `submit()` (150ms-then-
   separate-write) is left alone but now carries a doc comment pointing at
   this instead, so a future reader does not mistake it for current
   behavior. **Unit-tested** (pure, no process spawned — fully within the
   "no live CLI turns" bound):
   `tests/cli-contract/tests/offline/pty-logical-message.test.ts`, 8 cases
   covering the empty/short/framed/threshold/newline/trim/UTF-8-byte-width
   boundaries against `crates/daemon/tests/reconnect.rs`'s own equivalents
   where one exists. Ran (offline, no live spawn): `pnpm test` in
   `tests/cli-contract` → exit 0, 62 passed (8 files, up from 7/54).
2. **Added** `tests/cli-contract/tests/live/codex-logical-submission.test.ts`
   and `.../claude-logical-submission.test.ts` — each starts a real
   interactive TUI (`--yolo` for Codex, `--dangerously-skip-permissions` for
   Claude, both matching `crates/kanna-server/src/task_creator/commands.rs::
   get_agent_permission_flags`'s own mapping for `permission_mode` `None`/
   `"dontAsk"`, i.e. the same flags Kanna's own daemon passes for an ordinary
   PTY-mode task — not a weakened or invented flag), puts it into a long busy
   turn, then delivers a second, large (>256-byte, paste-framed) message
   mid-turn via `submitLogical` and asserts the model *acted on it* (wrote a
   marker file with the expected content) — not merely that bytes reached
   the pty, which `crates/daemon/tests/reconnect.rs` already covers and this
   note already found insufficient (above). **Verified these load and
   resolve correctly without spawning anything**: `pnpm exec vitest list
   --config vitest.live.config.ts tests/live/claude-logical-submission.test.ts
   tests/live/codex-logical-submission.test.ts` → both test names collected,
   zero CLI processes started (`vitest list` only collects; it does not run
   `it()` bodies). The Codex file's trust-prompt/composer regexes are carried
   from `codex-tui-quit.test.ts`, which has actually been run and passed; the
   Claude file's equivalents are **an unverified best-effort guess** — this
   repo has no prior live-TUI Claude test to carry a proven pattern from, and
   no live CLI turn was run to check it in this pass. Both files say so in
   their own comments.
3. **One real consumption lane was then run: `tests/live/codex-logical-submission.test.ts`
   only, twice, real CLI turns, real exit codes.** Pinned an explicit
   `-m gpt-5.6-sol -c model_reasoning_effort="low"` rather than an
   unspecified default. Run 1 (`.tmp/codex-logical-submission-run.log`, not
   committed) failed both cases in ~700ms: the busy-phase proof used literal
   marker strings embedded in the instruction text, and codex's own TUI
   echoes a submitted message onto the transcript before running anything —
   both "start" and "end" markers appeared together as an echo, not
   execution, so `isStillInBusyPhase` failed immediately. **Causal fix, not
   a loosened assertion:** replaced the text markers with the file-touch
   pattern `opencode-injected-input.test.ts`'s own mid-turn case already
   uses (`existsSync(startedFile)`/`existsSync(finishedFile)`) — a touched
   file cannot be satisfied by an echo. Re-ran once (the one authorized
   fix/rerun): Run 2 (`.tmp/codex-logical-submission-run-2.log`, not
   committed) still failed both cases, in ~30s each this time, with real,
   attributable evidence — `-m gpt-5.6-sol -c model_reasoning_effort="low"`
   was accepted (`model: gpt-5.6-sol low`, OpenAI Codex v0.153.4, confirmed
   from the transcript, not assumed) — and the actual cause is visible in
   the preserved output: codex was still `Booting MCP server: codex_apps (0s
   • esc to interrupt)` for the whole 30s wait, so the injected instruction
   sat queued (`tab to queue message`) and was never executed; the busy-phase
   start file never appeared. The borrowed `COMPOSER` pattern
   (`/\/modeltochange|Use\/skills/i`, from the already-passing
   `codex-tui-quit.test.ts`) matched `/model to change` — text in the static
   model-info header panel, visible even mid-MCP-boot — so `reachComposer`
   reported ready before codex actually was. **Neither run is evidence about
   CR-swallowing, a CLI parser defect, or the model declining the
   instruction; both are a harness precondition gap**, and that conclusion
   comes from reading the actual transcript, not from guessing a cause for a
   bare timeout. Narrowed `COMPOSER` to `/Use\/skills/i` (dropping only the
   half proven to match early) as a further source correction after run 2,
   not re-run in that pass — the one-fix-then-report bound.
   `codex-tui-quit.test.ts` itself is untouched (out of scope; its own
   environment/usage may not hit this MCP-boot race, and this pass did not
   re-verify it). No auth secrets appear in any preserved log; only this
   test's own PTY children were killed; no other repeated live-suite runs or
   loops. Claude's file was not run this pass (no automatic second provider
   run) and remains authored/unverified.

   **Run 3** (lane cleared, `.tmp/codex-logical-submission-run-3.log`, not
   committed) failed both cases again at `reachComposer` — but the narrowed
   `/Use\/skills/i` never matched at all: that placeholder tip *rotates* and
   is not reliably present. The same transcript showed the real, stable
   composer signal directly: `› Ask Codex to do anything`, codex's actual
   input-box placeholder. Root cause of both `COMPOSER` attempts failing the
   same underlying way: `PtySession.output`/`waitForOutput` search the
   *entire* accumulated byte history, not the current screen — this bridge
   concatenates and ANSI-strips, it does not emulate a terminal grid — so a
   stale `Booting MCP server` banner from seconds earlier stays matchable
   forever regardless of current state. **Fix:** replaced pattern-matching
   against the whole history with `waitForComposerReady`, polling a recency
   window (last 3,000 chars) of output and requiring both the ready
   placeholder present *and* the busy-boot banner absent from that window —
   a fact about the current screen, not the whole session. **Run 4**, the one
   bounded rerun this fix earned: genuine progress —
   `.tmp/codex-logical-submission-run-4.log` (not committed) shows
   `reachComposer` no longer failing, both cases got past it — but the
   busy-phase proof still failed: codex kept printing `Booting MCP server:
   codex_apps` well past the readiness read, and the submitted instruction
   landed as `tab to queue message` rather than executing, so
   `.kanna-busy-phase-start` never appeared in the 30s wait. Checked
   (non-agentic, zero-cost `codex --help` / `codex mcp --help` / `codex mcp
   list` against a bare-fresh `CODEX_HOME` — not a live turn): `codex_apps`
   is not a user-configured MCP server (`codex mcp list` reports none for a
   fresh home) — it is a built-in feature. **Follow-up, source-only, no live
   turn:** `codex features list` (a local, instant, non-agentic
   introspection command — zero API cost) confirmed the exact name:
   `apps    stable    true`, on by default — the feature backing the
   `codex_apps` MCP server. `--disable apps` is now in the test's spawn
   args, **not yet run live this pass**; ready for the next authorized
   Codex lane. **Not attempted a third *live run* this pass** — the
   one-fix-then-report bound on live turns, now spent twice over two lane
   authorizations, is unaffected by this purely local follow-up check. Net
   across all four runs: composer-readiness detection is
   now solid (two real runs each falsified a different single-regex
   approach, which is itself real, evidenced progress) — but the underlying
   question ("does codex's parser treat this CR as submit mid-turn") is
   still untested; every failure so far has been a harness precondition, not
   a result either way.

   **Run 5** (`CODEX INPUT RETRY LANE CLEAR`, `.tmp/codex-logical-submission-run-5.log`,
   not committed): ran the corrected file exactly once, `--disable apps`
   in place, same explicit spawn/options/identity as every prior run
   (`--yolo -m gpt-5.6-sol -c model_reasoning_effort="low" --disable apps`).
   **`--disable apps` is confirmed working — the MCP-boot race is closed:**
   `Booting MCP server: codex_apps` does not appear anywhere in this
   transcript, the first run where it's been absent. Both cases still
   failed, still at the busy-phase-start check (30s), but the actual
   preserved tail shows something new and genuinely ambiguous rather than
   the same boot-race signature: the header/tip render, then the submitted
   instruction text on the composer line (`› Run this exact shell command
   now, and do not reply until it finishes: sh -c '...'`), then nothing —
   no visible turn-start indicator, no tool-call chrome, no response text,
   within the captured window. Read honestly, this does not distinguish
   between two real possibilities: (a) codex's own session/turn startup
   latency, independent of MCP boot, still exceeds the 30s wait on its own,
   or (b) the message shown on the composer line was never actually
   submitted at all — which would be the exact question this file exists to
   answer. **Cause is marked unattributed, per instruction, not guessed
   either way** — the preserved tail (`session.output.slice(-1500)` at
   failure) does not contain enough to tell them apart, and no further live
   run was made to find out (no automatic repeated variants this pass).
   **Caveat, stated explicitly per instruction:** `--disable apps` is
   fixture isolation to remove one confound from this test, not a claim
   about the owner's original environment — Codex ships with `apps` on by
   default, so a future pass here (with or without the flag) says nothing
   about whether the owner's own session had it enabled, and does not by
   itself clear or implicate `apps` in the original report. Cleanup:
   confirmed no leftover processes or temp dirs tied to this fixture
   specifically (`kanna-codex-submission-*`/`kanna-codex-home-*` — an
   initial broad `ps aux | grep codex` incorrectly matched an unrelated,
   concurrent Kanna task's own real Codex session on this shared machine;
   corrected to the fixture's own temp-dir names before concluding anything
   was clean, and left that other task's process untouched).

**Correction, per manager review of run 5's own evidence:** the earlier
re-report after run 5 proposed "check whether the composer clears after the
`\r`" as the next smallest observation before a sixth run. That was wrong and
was corrected before any sixth run was authorized or made: composer-text
clearing alone is not unambiguous submission proof — `session.output` is a
byte-concatenating bridge, not a real terminal-grid emulator (see
`PtySession`'s own class doc), so it cannot distinguish an *echoed* history
line (the TUI redrawing what it already submitted) from an *unsent draft*
still sitting in the composer; a cleared-looking composer is consistent with
either.

**Observation-plan instrumentation added instead (source only, no sixth run
made this pass)**, in `tests/cli-contract/tests/live/codex-logical-submission.test.ts`
and `tests/cli-contract/helpers/pty.ts`:
- `PtySession.rawOutput` — the complete, un-stripped byte stream (every
  ANSI/OSC sequence intact), alongside the existing lossy `output` view.
- A checkpoint recorder with explicit, separately-labeled stages —
  `composer-ready` (initial submission readiness) is now recorded distinctly
  from `busy-start-observed:<bool>` (the mid-turn condition), rather than
  inferred from one conflated signal.
- `findRolloutFiles`/`summarizeRolloutStructurally` — Codex persists each
  session as its own JSONL "rollout" file under
  `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`; a
  `response_item` line with `payload.type === "message" && payload.role ===
  "user"` is Codex's own record of a message it treated as actually
  submitted — direct evidence, not a screen inference, distinguishing typed
  draft from submitted history. Inspected read-only, structure/keys only (no
  message content, tool arguments, or secrets read or reproduced): this
  test's own `CODEX_HOME` is a fresh temp dir it alone owns, so any rollout
  file found under it belongs to this run and nothing else. The summary
  records only `ordinal`/`type`/`payload.type`/`payload.role`/`timestamp`
  and a boolean marker match — never raw content.
- `captureArtifacts`, wired into `teardown()` ahead of the existing temp-dir
  removal, writes full raw/rendered output, checkpoints, and the copied
  rollout file(s) plus their structural summary (never `auth.json`, never
  the whole `CODEX_HOME`) to `.tmp/codex-run-artifacts/<run>/` — gitignored,
  not committed — on every exit, pass or fail.

None of this changed the two cases' pass/fail assertions.

**Run 6** (`CODEX INSTRUMENTED OBSERVATION LANE CLEAR`, authorized for the
short-message case only; artifacts under `.tmp/codex-run-artifacts/`, not
committed): the intended `vitest ... -t "<name>"` invocation did not restrict
to that one case. **Correction** (an earlier account of this run understated
its scope as "both cases ran"; re-read directly from the retained raw log,
`.tmp/codex-logical-submission-run-6.log`, not committed): the invocation's
arg-forwarding defect made it run the entire live suite, not just this
file's two cases — `Test Files 7 failed | 16 passed (23)`,
`Tests 10 failed | 77 passed | 4 skipped (91)`, `Duration 941.62s`,
overall `exit code 1`, including unrelated OpenCode/model-id live tests.
This was a harness/CLI-invocation defect in the run command, not authorized
scope; noted here for the record, not re-attempted; the unrelated failures
it surfaced (OpenCode flags/exec-json/MCP-flags/model-ids) are out-of-scope
and unattributed by this task — no baseline run establishes they predate
this branch's changes, so they are recorded as observed, not claimed
pre-existing, and not investigated here. Both of *this file's* two cases
failed identically, at the same 30s
busy-phase-start
check as run 5, `exit code 1`. Checkpoints for the short case:
`codex-spawned` 19:14:10.819Z, `composer-ready` 19:14:11.829Z — under a
second later, because `--yolo` skips the directory-trust prompt entirely
(confirmed: no trust-prompt bytes appear anywhere in this run's raw
output) — then `busy-start-observed:false` at 19:14:41.866Z (the 30s
timeout). At the moment the busy-phase instruction was submitted, the
composer's own model field still read `model: loading`, not yet
`gpt-5.6-sol low` — raw output shows both frames, `loading` first. Whether
input accepted at the composer while the model is still resolving is
silently dropped is a real, evidence-grounded hypothesis this run raises,
not a conclusion. `rollout-files-found.json` was empty for both cases: no
rollout JSONL was ever written under either isolated `CODEX_HOME`
(cross-checked read-only against the real `~/.codex/sessions` too — nothing
new there either). That is consistent with — but does not prove — no turn
ever starting, since the process was SIGKILLed rather than exited cleanly,
so a started-but-unflushed turn cannot be ruled out from this alone.
Process-tree cleanup verified for both cases: each session's real `codex`
child (found by exact pid via `pgrep -P <bridgePid>`, not a name match) was
already dead after the bridge kill, before any force-kill was needed
(`forceKilled: []`, `stillAlive: []`); machine-wide read-only checks after
teardown found no leftover fixture processes or temp dirs, including from
the unintended second case. Cause remains unattributed — this narrows the
search (a UI-ready-but-runtime-not-ready window is now a concrete,
evidence-backed candidate alongside plain startup latency) without
resolving it, and per instruction no further live run was made this pass.

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
was located by any means available here. The real Codex consumption lane has
now run six times across four lane authorizations (run 6 unintentionally
covered both cases instead of the one authorized — see above; not
re-attempted). Runs 1–4 all failed on
the same class of harness precondition (a `codex_apps` MCP-server-boot race,
narrowed run over run from "wrong composer pattern" matched too early, to
that pattern never matching at all, to composer detection working but the
boot itself outlasting the wait window) — never on anything resembling a
swallowed submission boundary or a CLI parser defect. **Run 5, with the
MCP-boot race fix (`--disable apps`) confirmed working and the boot text
genuinely absent from its transcript, still failed — but this time the
preserved evidence does not clearly point back to a harness precondition
either; it is marked unattributed** between ordinary session-startup
latency and the actual submission question, because the captured tail does
not contain enough to tell the two apart, and no further live run was made
to find out. The CLI-side parser question this whole reproduction exists to
answer is therefore still genuinely open — not "probably fine because the
daemon-byte tests pass," not "reproduced," and — as of run 5 — no longer
confidently "just a harness bug" either. `--disable apps` is fixture
isolation, not a claim about the owner's own environment (which ships with
`apps` on by default); it does not by itself implicate or clear that
feature in the original report. Closing
this fully needs, in order of what it would actually settle: (a) the
owner's affected session/provider/timestamp — still pending, not invented
here — so the real daemon write timeline for that delivery can be read
directly; (b) running the corrected `cargo test` subset above; (c) a sixth
Codex run — the observation-plan instrumentation above is now in place and
ready for it, so that run would for the first time have a full timestamped
raw/rendered record plus Codex's own rollout evidence to read, rather than
another 1500-char tail guess; and/or a first real run of the
authored-but-unverified Claude case, narrowed to whichever provider the
owner's answer implicates once it arrives.

## Overlap — corrected

Task `b1d685b7` ("Fix task-pull preparation submission and separate quit
command") does **not** touch `crates/daemon/src/session.rs`. Its diff against
main (reviewed directly) is `crates/kanna-server/src/http_api.rs`,
`crates/kanna-server/src/http_api/task_input.rs`,
`crates/kanna-server/src/transfer_engine/{finalize,push}.rs`, and two docs —
six files, none in `crates/daemon/`. An earlier draft of this note incorrectly
said it was actively iterating in `session.rs`; that was wrong and is
corrected here.

Re-checked after this session's later edits (`apps/mobile/src/e2eTrustSeed.ts`,
`apps/mobile/src/e2eTrustSeed.test.ts`, `apps/mobile/src/appModel.ts`,
`tests/cli-contract/helpers/pty.ts`, and the two new
`tests/cli-contract/tests/{offline,live}/*.test.ts` files) against both
sibling branches' full diffs from their merge-base
(`git diff --stat $(git merge-base origin/main task-b1d685b7-4) task-b1d685b7-4`
and the equivalent for `task-ed245f68-4`, scoped to `apps/mobile` and
`tests/cli-contract`): **both come back empty — no edit overlap with either
sibling on any file this session touched**, including the ones edited after
the original check above. (An earlier pass of this same re-check briefly
showed a spurious `appModel.ts` diff against a stale local `main` ref before
`git merge-base` was used explicitly; the merge-base comparison is the
authoritative one and it is clean.)

The one *read* overlap already on record still stands and is unaffected by
this session's later edits: `b1d685b7` is actively iterating on
`crates/kanna-server/src/http_api/task_input.rs` itself, so
`try_submit_task_input_to_session`'s current body, read here to establish the
"no separate CR write, no wait" finding above, may already be mid-change
under that task's own work — worth re-confirming against `b1d685b7`'s
eventual committed state rather than assuming this note's reading of that
file stays current. Task `ed245f68` (Android emulator/pairing) touches
`TaskScreen.tsx` and `taskComposerKeyboard.ts` but not `TerminalWebView.tsx`
or `terminalReconnectPresentation.ts` — also confirmed no overlap.
