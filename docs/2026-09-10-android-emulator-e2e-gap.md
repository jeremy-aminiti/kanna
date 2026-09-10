# Android emulator E2E gap — 2026-09-10

## Why the bounded smoke is not in the current harness

The repository's mobile Appium runner is iOS-only. `apps/mobile/e2e/run.ts`
accepts only `simulator` and `device`, both meaning Apple targets; its sessions
come from `createSimulatorCapabilities` / `createPhysicalDeviceCapabilities`,
which hard-code Appium's XCUITest driver, and its device lifecycle uses
`xcrun simctl`. The new canonical `kd` Android launch can build, select, boot,
install, and launch an AVD, but it does not provide an Android Appium session
or an E2E-owned Android lifecycle. Consequently the existing runner cannot
drive a real pairing code, inspect the task/terminal UI, isolate an input, or
background and reattach the Android app. Treating the iOS runner as Android
coverage would not test the changed native boundary.

The bounded smoke becomes runnable when the mobile E2E entrypoint has an
Android-emulator target owned by `kd` (for example,
`./kd mobile test --android-emulator Medium_Phone_API_36.1`), an Appium
UiAutomator2 session for the selected AVD, Android equivalents for app launch,
screenshots, and background/foreground, and a task fixture that records one
isolated input. That lane should use the canonical `kd` launch, claim the real
desktop code, open its task and terminal, prove visible input/output, then
background, reattach, and observe new terminal output after the relaunch. This
does not require physical-device discovery or a general migration of the
existing iOS suites.

## Narrower coverage for this slice

- `tools/kd/tests/mobile-android.test.ts` covers SDK/ADB/emulator parsing, exact
  AVD selection, Android-only prebuild, and the task-scoped Expo launch command.
- `tools/kd/tests/dev-plan.test.ts` covers the Android AVD plan and routes the
  server, Metro, relay, and Firebase emulator endpoints through `10.0.2.2`.
- `tools/kd/tests/tasks.test.ts` covers the canonical Android doctor path for
  `Medium_Phone_API_36.1` without booting or building.
- `apps/mobile/src/lib/discovery/explicitDevelopmentServer.test.ts` proves the
  development-only server candidate is obtained from the server's real
  desktop identity and fails closed when that identity is unavailable.
- `apps/mobile/src/lib/pairing/machinePairing.test.ts` proves explicit discovery
  is refreshed before a code claim and that the issued paired-device secret is
  retained. The real-listener pairing/KSP cases in
  `crates/kanna-server/tests/local_client_boundary_http.rs` cover browser-shaped
  loopback upgrade classification plus in-band paired-secret enforcement for
  both stream versions.
- `apps/mobile/src/mobileAppConfig.test.ts` covers the Android package identity,
  development-only cleartext allowance, environment configuration, and native
  runtime version.
- `apps/mobile/src/screens/taskComposerKeyboard.test.ts` covers Android's
  `keyboardDidShow` / `keyboardDidHide` events and composer offset.

## Native evidence and its limit

The authorized emulator run is retained in the task's original workspace at
`/Users/jeremyhale/.kanna/repos/kanna-2/.kanna-worktrees/task-ed245f68/.tmp/android-acceptance/`.
Its evidence indexes are `native-provenance-final.md` and
`live/reconnect-and-cleanup-clarification.md`; final cleanup is recorded in
`live/cleanup-verification-clarified.txt`. The canonical
`./kd mobile run --android-emulator Medium_Phone_API_36.1` exited 0 and installed
the development identity `build.kanna.app.dev` at runtime `2.2.5`. The recorded
real-code claim, task open, terminal snapshot, and isolated input ledger prove
the authenticated functional path.

The reconnect evidence proves authenticated stream readiness followed by a
fresh 10,864-byte, 115-by-40 snapshot. It does **not** prove overlay-free visual
settling or new terminal output arriving after relaunch. In particular,
`live/reconnect-final-settled2.png`, captured at
`2026-09-10T07:38:13Z`, still shows the Connecting/Loading presentation. Those
two presentation/live-tail claims remain for the Android E2E smoke above; they
are not inferred from the successful authenticated reconnect and snapshot.
