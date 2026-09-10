# Item 4 ack-replay gate: real two-peer coverage, and what's still open

Item 4 of the transfer-integrity work (`af02a511`) gates `run_import`'s
re-entry (`crates/kanna-server/src/transfer_engine/import.rs:429-439`) on the
destination server's own durably persisted
`transferred_task_manifest.content_commitment`: once
`verify_persisted_task_bundle` has proven a transfer, a retry must skip it
entirely — no artifact or input-ledger fetch, no dependency on the sidecar's
in-memory artifact cache or on the source machine still holding the artifact
— and go straight to replaying `control::acknowledge_import_committed` with
the exact persisted proof.

## Revision history

An earlier version of this note (and the fixtures it described) used only a
single, unpaired destination sidecar and inferred the skip-path from a
definitive-but-uninformative sidecar error ("missing source peer"), reasoning
that a genuine second peer was out of this checkpoint's bound by analogy to
item 3's own two-node gap. On review, that was correctly rejected: reaching
the ack call is not the same as proving the ack call carried the right
values, or that a real, wire-admitted reservation survives a restart — an
empty, never-admitted sidecar cannot demonstrate either. The fixtures below
replace that reasoning with a real second peer.

## What is now proven for real

`import.rs`'s own `mod tests` now pairs two real `kanna-task-transfer`
sidecar *subprocesses* — a source and a destination — over the sidecar's
existing `KANNA_TRANSFER_DISCOVERY=registry` file-based discovery mode (the
same one `crates/task-transfer/tests/sidecar_control.rs` already uses; no
real mDNS multicast, so this does not depend on the test environment's
network configuration), drives a genuine `start-pairing`/`accept-pairing`
handshake and a genuine preflight+commit from source to destination, and only
then seeds the destination's own durable DB (`transferred_task_manifest`,
`task_transfer`) and calls the real `run_import`:

- `retry_with_persisted_commitment_skips_reverification_and_replays_the_real_ack`
  and `retry_with_persisted_commitment_survives_a_real_sidecar_process_restart`
  both now assert `run_import` returns `Ok(())` — a genuinely completed
  acknowledgment, not merely a specific error shape — and both read back the
  *source* sidecar's own durable `outgoing_transfer_committed` work-queue
  record and assert every field it carries (`transfer_id`, `source_task_id`,
  `destination_local_task_id`, `content_commitment`, `destination_repo_id`)
  against exactly what this test persisted. That is proof of the values that
  crossed the wire, not just that the call was reached.
- The restart test additionally spawns and kills one extra destination
  sidecar incarnation between the real commit and the incarnation
  `run_import` talks to, and `establish_real_transfer_reservation` itself
  always drops its own bootstrap incarnation before returning — so every
  positive test's `run_import` call is against a process whose
  `incoming_reservations` entry was reloaded from disk, never carried over
  warm in memory.
- The no-fetch claim (an artifact/ledger fetch attempted against a sidecar
  that never staged or received one is guaranteed to fail with a distinct,
  fetch-shaped error) still holds and is still exercised implicitly: neither
  positive test ever stages or fetches an artifact, and both still reach a
  successful completion, which a wrongly-unskipped `verify_persisted_task_bundle`
  could not produce (it would fail trying to fetch the input ledger from a
  sidecar that has none staged).
- `retry_with_no_persisted_commitment_still_takes_the_verification_path`
  (absent proof still verifies) and
  `a_commitment_proven_for_another_task_cannot_authorize_this_tasks_ack`
  (existing task-binding guard) are unchanged — neither reaches the sidecar
  at all, so neither needed the paired harness.
- Pure DB-level immutability:
  `db::tests::a_persisted_content_commitment_is_set_once_and_never_overwritten`.

## What is still open

- **Compilation and execution.** These fixtures are source-authored against
  a real subprocess/wire harness that has not yet been built or run — see
  the task's report to its manager for the exact commands and current hold
  status. Real timing (pairing handshake latency, registry-file discovery
  delay) could not be tuned empirically; timeouts are generous (15s) but
  unverified.
- **Mixed old/new server-sidecar versions and the real desktop existing-clone
  E2E.** Unchanged from `2026-09-09-transfer-admission-proof-e2e-gap.md`:
  no fixture here runs two independently versioned builds against each
  other, or drives the real desktop app's transfer path end to end. Out of
  this item's bound.

## The committed-reservation TTL exemption (architect consultation `da6acf0b`, invariant 4)

The consultation flagged, as a risk to verify rather than assume, that an ack
retry delayed past `prune_incoming_reservations`'s TTL could lose the
sidecar-side binding needed to route the replayed ack even though the
server-side proof is fine. Read directly
(`crates/task-transfer/src/runtime/replay_store.rs:330-348`): pruning already
only removes a reservation when `!reservation.committed` — a reservation
marked `committed` (which an admitted incoming transfer's is) is exempt from
the TTL regardless of `created_at_unix_ms` age. The concern is already moot
under the existing code; no sidecar change was made or is needed for it, per
this task's instructions not to invent one for a hypothetical.
