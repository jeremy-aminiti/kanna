# `pipeline_item.base_ref` after a transfer ref retry/conflict

Item 5 of the transfer-integrity work (`af02a511`) replaced
`import_task_bundle_refs`'s deterministic staging refs and per-ref CAS writes
with object-only bundle ingestion plus one atomic `git update-ref --stdin`
transaction (`crates/kanna-server/src/transfer_engine/git.rs`). The architect
verdict for this item asked for "an import/task-creation integration
regression proving `pipeline_item.base_ref` remains the first transfer's
private base and still resolves to its original OID after retry/conflict."

## What is covered

`import_task_bundle_refs`'s `(head_ref, base_ref)` pair reaches the DB column
as a straight pass-through with no further transformation:

- `import_verified_task_bundle` (`transfer_engine/import.rs`) returns the pair
  unchanged as `imported_refs`.
- `build_create_request` (`transfer_engine/import.rs`) maps it to
  `CreateTaskRequest.diff_base_ref` unchanged
  (`imported_refs.map(|(_head, base)| base)`).
- `prepare_create_task_for_api` (`task_creator/mod.rs:2014`) forwards
  `request.diff_base_ref` as `TaskCreationRequest.stored_base_ref` unchanged.
- `prepare_task_spawn` (`task_creator/mod.rs:3420`) writes
  `resolved.stored_base_ref` into `pipeline_item.base_ref` unchanged.

`crates/kanna-server/src/http_api/tests/transfer_preparation_gate.rs::a_persisted_base_ref_survives_a_conflicting_and_an_identical_replay`
now drives this whole chain for real: it runs the actual production
`import_task_bundle_refs` against the fixture's real repo to get a genuine
private ref pair, feeds it through the real HTTP router
(`PUT /v1/tasks/{id}`) with the exact `baseRef`/`diffBaseRef` wiring
`build_create_request` produces, lets a real git worktree fork and a fake
Unix-socket daemon accept the spawn (the same boundary
`transfer_import_with_a_prepared_manifest_reaches_the_daemon_spawn` in the
same file already exercises), and then reads `pipeline_item.base_ref` back
out of a real SQLite `Db` — asserting it is exactly the private base ref
`import_task_bundle_refs` returned, and that it still resolves to the
original base OID after both a refused conflicting re-import (same transfer
and head, different base) and an identical retry of the original import.

`crates/kanna-server/src/transfer_engine/git.rs`'s real-Git regressions cover
the git-layer half of the same invariant, plus the cases the verdict called
out that do not need a task-creation round trip: cross-transfer independence
sharing a head, a corrupt/mismatched bundle touching no ref, and (as of this
revision) a genuinely conflicting-contract concurrent publication —
`concurrent_competing_publication_converges_on_one_complete_pair_and_refuses_the_loser`
synchronizes racers at the actual publication boundary
(`wait_at_publication_seam`, a `#[cfg(test)]`-only hook compiled to nothing in
production, called from the same code path production uses) so every racer's
read lands before anyone has published, then races two genuinely different
base contracts for the same transfer and head: exactly one contract's racers
converge on success and the other's are refused, never a hybrid.

## What remains open

The architect verdict's fallback clause — "if that harness cannot express it
now, record the remaining boundary" — still applies to one narrower thing:
extending the *real two-sidecar* transfer fixture
(`crates/kanna-server/src/transfer_engine/import.rs`'s `Item4RealTransfer`,
`establish_real_transfer_reservation`) itself to carry a task-bundle repo
import and assert the same same-head-collision behavior over the actual wire
protocol between two sidecar subprocesses, rather than by calling
`import_task_bundle_refs`/the HTTP router directly as the tests above do.
That fixture currently proves the acknowledgment/retry seam, not a
task-bundle repo import; wiring real repo acquisition through it is a
materially larger addition than this bounded item's fix, and is not needed to
prove the `pipeline_item.base_ref` invariant itself, which the tests above
already cover end to end through the real production code path.
