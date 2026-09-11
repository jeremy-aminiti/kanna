# Mobile connection flicker + deep-link scheme fix — verification note

Owner report (2026-09-10): quick full-screen terminal redraws for the first
few seconds after connecting to a session on mobile, new since the last
upgrade (mobile staging runtime 2.2.3, OTA
`3b64331c-4609-beba-25f3-d8a3397150d9`). This note covers the two independent
causes found and fixed while tracing that report. The report also included a
missing-Enter-after-Send symptom; that investigation is tracked separately
(commit history preserved at local ref
`archive/missing-enter-investigation-41d28cff`, HEAD `e07e7186b`) and is
**not** covered here — it remains unresolved, not fixed, not disproven.

## Fix 1 — terminal overlay could hide on a placeholder paint, not real content

`apps/mobile/src/screens/TerminalWebView.tsx`'s loading-overlay gate
(`hasRenderedTerminalContent`) could be satisfied by a transport-gap
placeholder repaint (a "connecting" frame with zero output length), not just
real content, so the overlay could drop and reappear as the first few real
frames arrived — the reported redraw flicker.

**Fix** (commit `d3a06a2f1`): added `pendingContentReadyRef`, a single
non-deleting slot that classifies each injected replace as real-content vs.
transport-gap placeholder (`isTransportGapPlaceholder`) before gating
`hasRenderedTerminalContent`. Also fixed a duplicate-acknowledgment bug found
in the same path: a `Map` that deleted its entry on read defaulted a second
acknowledgment of the same outstanding revision to "ready."

**Causal proof:** `apps/mobile/src/screens/TerminalWebView.test.tsx` (new,
117 lines) — both cases confirmed failing without the fix, passing with it.

## Fix 2 — deep-link trust/pair seam hardcoded the wrong scheme

`apps/mobile/src/e2eTrustSeed.ts` hardcoded the literal `"kanna:"` protocol
for its `e2e-trust`/`e2e-pair` deep links. DEV's actual resolved build scheme
is `kanna-dev` (prod's is bare `kanna`) — DEV deep links never matched.

**Fix** (commit `6fee7e01c`): `matchesE2eProtocol(protocol, scheme)` accepts
either the literal `"kanna:"` (legacy/Appium compatibility) or the resolved
build scheme. `installE2eTrustSeedHandler` takes an optional `scheme`,
threaded from `resolveMobileAppEnvironment(extra?.appEnv).scheme` in
`apps/mobile/src/appModel.ts`.

**Causal proof:** `apps/mobile/src/e2eTrustSeed.test.ts` (new, 67 lines).

## On-device verification (real Appium/XCUITest session, own isolated dev stack)

Both fixes are JS-only (no `runtimeVersion` bump). Verified together in one
bounded session against this worktree's own isolated dev
stack (desktop, `kanna-server`, Metro — never the installed staging/production
app), simulator UDID `1CCF3E78-D553-46A9-A4A3-74F4D1BDE0A4` ("iPhone 16 Pro
Max"), bundle `build.kanna.app.dev`. Evidence preserved at
`.tmp/flicker-verify/` (screenshots + the real WebDriver session log
`run7-full.log`; gitignored, not committed — copied from `/private/tmp` after
the run, checksum-verified identical).

- **Deep-link/pairing proof:** e2e-trust then a real `POST /v1/pairing/sessions`
  against the isolated dev server, then e2e-pair — followed by navigating to
  the actual Machines screen and confirming the paired row exists
  (`22-machines-screen.png`: "Jeremy's Mac Studio — PAIRED — Available on
  this network"). A successful deep-link command return alone was not treated
  as sufficient proof.
- **Flicker proof:** 19 real overlay-presence samples (each an actual
  WebDriver round trip, not a fixed-interval poll) from `run7-full.log`,
  `2026-09-11T03:29:13.445Z` through `03:29:20.269Z` — measured span ≈6.82s.
  Overlay present through `15.323Z`, absent from `15.808Z` onward, never
  reappeared: `[true,true,true,true,false×15]`.
  **Resolution limit, stated explicitly:** the realized sampling interval was
  ~370ms (two sequential round trips per sample, not the 200ms nominal); this
  rules out a reappearance at that granularity, not a flicker faster than it.
  The causal proof remains the red/green unit test above — this is
  corroborating, resolution-bounded, on-device evidence, not a replacement
  for it.
- **Terminal content confirmed live:** `23-terminal-open.png` shows real
  rendered session output for the disposable fixture task created on the
  isolated dev DB for this verification.

**Known gaps in this evidence, stated rather than papered over:** the exact
simulator UDID does not appear in any surviving log (only the device name);
the full multi-step orchestration script that produced `run7-full.log` was
not preserved as a file, only its transcript was (`session-setup.mjs`, the
one surviving script file, is an earlier minimal connectivity probe, not the
full script).

## Checks run

- `pnpm --dir apps/mobile test -- src/screens/TerminalWebView.test.tsx src/e2eTrustSeed.test.ts` — 58/58 passed.
- `pnpm --dir tests/cli-contract test` (offline suite) — 54/54 passed.
- Full mobile stack and simulator torn down after verification: no listening ports, no stray Appium/WebDriverAgent processes, simulator shut down, working tree clean.

No production/daemon code touched by either fix. Both are scoped entirely to
the mobile app.
