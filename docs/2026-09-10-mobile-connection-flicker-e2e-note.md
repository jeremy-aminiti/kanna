# Mobile connection redraw flicker and missing-Enter — source findings, on-device pass pending

Date: 2026-09-10 · Task: 41d28cff · Host: Mac Studio

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

**Deployed-OTA provenance — explicitly unknown, not inferred from dates:**
the OTA manifest timestamp (2026-09-09T09:33:19.447Z) and the reconnect
commit's date (2026-09-09) are close, which is suggestive, not proof of
identity. No bundle hash, manifest asset list, or build-source record was
read to confirm the deployed mobile JS at that OTA actually contains
490c9120c/1f3379eee rather than, say, a commit shortly before or after it on
the same day. That provenance check was not done in this session and this
note does not claim it was.

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

## Human/isolated visual pass — next action, held pending authorization

Per `AGENTS.md` this is a UI-feel change and simulator verification is
necessary but not sufficient on its own; it wants an on-device look. That is
the next action for this symptom, not "fixed." This session did not attempt
a simulator run — the task's execution hold covers native/mobile/emulator
gates — and stands ready for an authorized isolated visual verification pass
rather than treating a human pass as a generic, indefinite blocker.

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
paste-framed one) and, by their bodies, assert the current single-write
design does not reproduce it. **This was established by reading the test
source, not by running it** — `cargo test` was not invoked this session
because the native gate is held. Whether these tests currently pass is
therefore unconfirmed by execution; it is inferred from the code being
internally consistent with its own assertions. This is not the same as
verifying the fix — the moment the hold lifts, `cargo test -p kanna-daemon
--test reconnect a_submission_boundary_is_written_even_while_the_terminal_repaints
a_long_single_line_logical_message_survives_the_pty_queue_split
a_never_settling_terminal_takes_every_delivery` (and the mobile-shared HTTP
route's own coverage in `crates/kanna-server/src/http_api/tests/input.rs`)
should be run and its exact exit reported before treating this symptom as
resolved.

Mobile's ordinary composer Send still routes through the identical,
platform-agnostic `POST /v1/tasks/{id}/input` (`TaskScreen.tsx` →
`mobileController.sendTaskInput` → `client.sendTaskInput` →
`lanTransport.ts`/`remoteTransport.ts`), so it inherits whatever this design
provides or lacks — no mobile-specific bypass was found. Mobile's on-screen
direct-typing path (`sendTaskTerminalInput` → raw KSP bytes) is a separate,
literal-keystroke-forwarding mechanism with no synthesized `\r`, so it is not
subject to the same class of race.

**Net position: not fixed, not confirmed reproducing, not ruled out.** The
current design is architecturally different from — not a continuation of —
the mechanism this note previously (incorrectly) credited, and existing
targeted tests, on paper, assert it does not reproduce the reported failure
mode. That is source reconciliation and existing-contract review, not proof
by execution or by device reproduction. No fix was authored for this symptom
because no currently-reproducing defect was located by either means. Closing
this fully needs: (a) running the targeted `cargo test` subset above and
reporting its exact exit, and (b) the owner's affected session/provider/
timestamp so the actual daemon write timeline for that specific delivery can
be inspected — the method PR #1314 itself used to establish causation the
first time.

## Overlap — corrected

Task `b1d685b7` ("Fix task-pull preparation submission and separate quit
command") does **not** touch `crates/daemon/src/session.rs`. Its diff against
main (reviewed directly) is `crates/kanna-server/src/http_api.rs`,
`crates/kanna-server/src/http_api/task_input.rs`,
`crates/kanna-server/src/transfer_engine/{finalize,push}.rs`, and two docs —
six files, none in `crates/daemon/`. An earlier draft of this note incorrectly
said it was actively iterating in `session.rs`; that was wrong and is
corrected here. There is no file-level overlap between that task's diff and
either the flicker fix (`apps/mobile/src/screens/TerminalWebView.{tsx,test.tsx}`,
this session's only edits) or the missing-Enter investigation (which only read
`crates/daemon/src/session.rs` and `crates/daemon/tests/reconnect.rs`, and
edited neither). Task `ed245f68` (Android emulator/pairing) touches
`TaskScreen.tsx` and `taskComposerKeyboard.ts` but not `TerminalWebView.tsx`
or `terminalReconnectPresentation.ts` — also confirmed no overlap.
