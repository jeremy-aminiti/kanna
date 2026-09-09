# Rust build storage on multi-worktree machines

Cargo target directories are private by design. A target directory contains
mutable fingerprints, dependency metadata, build-script output, incremental
state, and final artifacts. Pointing concurrent worktrees at one
`CARGO_TARGET_DIR` does not deduplicate it: Cargo takes a directory lock, and
the layout is still unsound after serialized builds because fingerprints retain
absolute source-root paths. `safe-rust-build-caching.md` reproduces a later
checkout receiving an artifact compiled from an earlier checkout through a
shared `build-dir`; a shared target has the same state plus final binaries.

The 2026-09-09 Studio measurement showed the operational cost: ten open
worktrees accounted for 175 GB of apparent `.build` data (27, 23, 21, 20, 20,
19, 15, 14, 10, and 4.7 GB). One task recreated 10 GB in 25 minutes after its
tree was cleared. The 10 GB kache store accelerates compilation, but is not a
Cargo target directory and cannot make divergent source snapshots share mutable
Cargo state.

The supported arrangement is therefore:

- Keep `target-dir = ".build"` and its Cargo `build-dir` private to the
  worktree. `kd` strips inherited `CARGO_TARGET_DIR` values so an ambient shell
  cannot accidentally defeat that boundary.
- Use kache for content-addressed, cross-worktree compiler reuse. On APFS,
  restored cache blobs can share physical blocks by reflink, so measure volume
  free space rather than summing `du` when evaluating it.
- For a machine whose internal disk cannot hold its active worktrees, use the
  existing per-worktree external-root hook (`.kanna/setup.local.sh`): it safely
  maps each `.build` to `<external-root>/<worktree-name>` and records the exact
  cleanup target. This moves capacity; it does not pretend to deduplicate
  Cargo's mutable state.
- Cap concurrent Rust gates on constrained machines. It bounds the number of
  simultaneously growing private targets and avoids both resource exhaustion
  and Cargo-lock queues. It does not make a raw shared target correct.

Final sidecars and release/package artifacts remain build-private under the
checkout's `.build`; kache does not cache executables. No staging or daemon
launch path may take a final binary from a contested shared Cargo directory.
