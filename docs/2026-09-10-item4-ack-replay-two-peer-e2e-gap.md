# Item 4 ack-replay gate: what the real-sidecar fixture proves, and what it doesn't yet

Item 4 of the transfer-integrity work (`af02a511`) gates `run_import`'s
re-entry (`crates/kanna-server/src/transfer_engine/import.rs:429-439`) on the
destination server's own durably persisted
`transferred_task_manifest.content_commitment`: once
`verify_persisted_task_bundle` has proven a transfer, a retry must skip it
entirely — no artifact or input-ledger fetch, no dependency on the sidecar's
in-memory artifact cache or on the source machine still holding the artifact
— and go straight to replaying `control::acknowledge_import_committed`. The
new tests in `import.rs`'s own `mod tests`
(`retry_with_persisted_commitment_skips_reverification_and_reaches_real_ack_replay`,
`retry_with_persisted_commitment_survives_a_real_sidecar_process_restart`,
plus the negative controls
`retry_with_no_persisted_commitment_still_takes_the_verification_path` and
`a_commitment_proven_for_another_task_cannot_authorize_this_tasks_ack`) prove
this against a real destination SQLite DB and a real `kanna-task-transfer`
sidecar *subprocess* — not a helper predicate standing in for either.

## What is proven for real

Whether `verify_persisted_task_bundle` runs at all — and therefore whether
any artifact/ledger fetch is attempted — is directly observable without a
real source peer: a fetch attempted against a sidecar that was never told
about the transfer, or whose in-memory `transfer_artifacts` cache was just
emptied by a process restart, is guaranteed to fail with a distinct,
fetch-shaped error *before* the ack call is ever reached. The skip-path's ack
replay is then genuinely driven through
`control::acknowledge_import_committed` against that same real subprocess;
without a paired source peer it cannot complete, but it fails with the
sidecar's own, specific `"missing source peer for import acknowledgment"`
answer (`crates/task-transfer/src/runtime/transfers.rs:608-613`) — which is
only reachable *after* the artifact-fetch step was skipped, and is never the
fetch-shaped error a wrongly-unskipped verification would produce instead.
That is the fixture's actual observable: reaching a specific, later failure
mode is proof the earlier one (a fetch) did not happen.

## What is not yet proven: the exact ack values on the wire

`acknowledge_import_committed`'s destination-side reservation lookup
(`transfers.rs:597-613`) fails before it ever touches
`content_commitment`/`destination_repo_id` when there is no
`incoming_reservations` entry for the transfer — and that entry is only ever
created by a real admitted commit arriving over the wire from an actual
paired source peer. Proving the *values* replayed on the wire — that
`content_commitment` and `destination_repo_id` reach the source's
`OutgoingTransferCommitted` event unchanged — needs a genuine second,
paired `kanna-task-transfer` peer process, real LAN pairing
(`start-pairing`/`accept-pairing`), and a real preflight+commit to seed that
reservation.

This is deliberately not attempted here, for the same reason
`docs/2026-09-09-transfer-admission-proof-e2e-gap.md` gives for the
analogous gap in item 3's `run_push` coverage: pairing two real sidecar
subprocesses through the full LAN discovery/pairing handshake is a
two-node harness, one layer up from a single destination-side gate fixture,
and out of this checkpoint's bound.

### What would make it testable

The same two-desktop harness `2026-09-09-transfer-admission-proof-e2e-gap.md`
names: two paired real `kanna-server` (or in-process `AppState`) + real
`kanna-task-transfer` sidecar pairs, so a source-side `run_push` can drive an
actual reservation through to a destination-side `run_import` retry and the
resulting `OutgoingTransferCommitted` event can be read back and asserted on
the source. It would unlock this test and the several others both docs name.

### What covers it meanwhile

- The four tests above prove the skip/no-skip control flow for real, plus
  content-commitment immutability
  (`db::tests::a_persisted_content_commitment_is_set_once_and_never_overwritten`)
  and the existing task-binding guard.
- `crates/task-transfer/tests/runtime.rs`'s
  `destination_can_acknowledge_import_commit_back_to_source` and
  `destination_reloads_awaiting_ack_reservation_after_sidecar_restart` prove,
  at the sidecar-to-sidecar wire level with two real paired
  `TransferRuntime`s, that `acknowledge_import_committed` replays the correct
  values end to end and survives a sidecar restart — the piece this gap
  leaves unconnected to `run_import`'s own retry entry point.

## The committed-reservation TTL exemption (architect consultation `da6acf0b`, invariant 4)

The consultation flagged, as a risk to verify rather than assume, that an
ack retry delayed past `prune_incoming_reservations`'s TTL could lose the
sidecar-side binding needed to route the replayed ack even though the
server-side proof is fine. Read directly
(`crates/task-transfer/src/runtime/replay_store.rs:330-348`): pruning already
only removes a reservation when `!reservation.committed` — a reservation
marked `committed` (which an admitted incoming transfer's is) is exempt from
the TTL regardless of `created_at_unix_ms` age. The concern is already moot
under the existing code; no sidecar change was made or is needed for it, per
this task's instructions not to invent one for a hypothetical.
