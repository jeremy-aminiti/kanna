# Transfer admission/refusal proof: two boundaries with no E2E yet

Item 3 of the transfer-integrity work (`af02a511`) binds admission/refusal to
a contract-specific proof through the existing durable sidecar exchange:
`mark_incoming_transfer_refused` gives the destination a fast, zero-I/O way to
tell the source "no" before the admission timeout, `outgoing_committed`
refuses to close a source without a destination-computed content commitment
matching what the source shipped, and `run_push` re-drives an unresolved
transfer under its own transfer id instead of reporting phantom success. Two
parts of that chain have no single end-to-end test yet.

## 1. `run_push`'s redrive path

`crates/kanna-server/src/transfer_engine/push.rs::run_push` now consults
`active_outgoing_transfer_for_source` and, for an unsettled row belonging to
this same push, re-issues `control::commit` against the stored `transfer_id`
and `payload_json` instead of starting a fresh preflight. That branch is
unit-covered for its *settlement* half
(`a_refused_outcome_settles_the_row_so_retry_converges`,
`an_unresolved_outcome_settles_nothing_and_stays_retriable`,
`an_admitted_outcome_settles_nothing_and_reports_success`, all in
`push.rs`'s own `mod tests`, exercising the real `settle_commit_outcome`
function against a real DB) and for the wire-level unresolved/redrive
semantics themselves
(`unresolved_admission_survives_and_can_still_land_after_the_submit_response`
in `crates/task-transfer/tests/runtime.rs`, two real `TransferRuntime`s over
real sockets). What is not covered is the *whole* path in one test: a real
`run_push` invocation, through a real git repository and staged session
artifacts, hitting the duplicate-active-row branch and actually re-driving.

### Why not yet

`run_push` requires the full source-task fixture other push tests build
(`crates/kanna-server/src/transfer_engine/git.rs`'s
`a_task_bundle_imports_unpublished_multi_commit_history_into_an_existing_clone`
and `payload.rs`'s task-bundle tests are the closest existing harnesses), plus
a live `state.transfer_sidecar()` to drive the actual `control::preflight` /
`control::commit` round trips — none of the existing `push.rs` unit tests
spin one up (`test_state_with_seed`'s sidecar client is deliberately dead,
per `import.rs`'s own tests). Building that harness is a duplicate of the
two-node `task-transfer` harness one layer up, through a real kanna-server
process on each side, which is out of this checkpoint's bound (it starts
edging into the held native/multi-instance gate).

### What would make it testable

A fixture that pairs two real `kanna-server` processes (or in-process
`AppState`s) each with a real, paired `task-transfer` sidecar attached, so
`run_push` can drive an actual reservation through an actual unresolved ->
redrive -> admitted sequence. That is the same missing piece
`2026-08-07-duplicate-push-transfer-e2e-gap.md` and the refused-pull gap doc
both point at: a two-desktop harness. It would unlock this test and several
others, not just this one.

### What covers it meanwhile

- `push.rs::settle_commit_outcome` unit tests (above) prove the DB-visible
  effect of each outcome is correct.
- `runtime.rs`'s two-runtime tests prove the wire-level unresolved/refused
  distinction and that a delayed admission is observable without a second
  full timeout.
- The duplicate-row detection query itself
  (`active_outgoing_transfer_for_source`) is covered by
  `db::tests::active_outgoing_transfer_lease_is_unique_across_connections`
  and `http_api::tests::transfers::duplicate_outgoing_transfer_insert_answers_409_with_the_transfer_in_flight`.

## 2. Mixed old/new server-sidecar version combinations, and the real
   existing-independent-clone desktop E2E

Both remain out of reach for the same underlying reason: there is no fixture
that runs two independently versioned `kanna-server`/`task-transfer` builds
against each other, and no fixture that drives the real desktop app's
existing-clone transfer path end to end. The architect's original verdict
(`ac2d0739`) already named this gap for items 1/2; item 3 inherits it
unchanged, and the additive-field/serde-default design throughout this
checkpoint (`admitted` keeps its wire meaning, `refusal_reason` and the
content-commitment fields are all `#[serde(default, skip_serializing_if)]`)
is what lets an old peer degrade to today's unresolved-timeout behavior
without a compatibility test proving it end to end.

### What covers it meanwhile

- `crates/task-transfer/tests/protocol.rs`'s
  `outgoing_transfer_committed_event_without_proof_decodes_as_none` and the
  `SidecarEvent`/`ControlResponse` round-trip tests prove the wire shapes
  decode safely with the new fields absent.
- `crates/kanna-server/src/transfer_engine/push.rs`'s
  `an_acknowledgment_with_no_content_commitment_refuses_to_close` proves the
  source-side consumer of an old-shaped acknowledgment refuses rather than
  trusting it.
- Reasoning, not a test: every new field added in this checkpoint is
  additive with `#[serde(default)]` on the read side and
  `skip_serializing_if = "Option::is_none"` on the write side, mirroring the
  idiom already used throughout `protocol.rs` for every other backward-
  compatible field in this file.
