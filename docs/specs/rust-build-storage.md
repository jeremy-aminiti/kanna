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
- For a machine whose internal disk cannot hold its active worktrees, configure
  `~/Library/Caches/kanna/build-storage.local.json` on macOS (or
  `$XDG_CACHE_HOME/kanna/build-storage.local.json`, defaulting to
  `~/.cache/kanna`, on Linux). Its ignored, per-machine `rustBuildRoot` maps
  each `.build` to `<rustBuildRoot>/<worktree-name>` and records that exact
  workspace-bound cleanup target. For example:

  ```json
  { "rustBuildRoot": "/Volumes/VHS/kanna-builds/kanna-7", "rustGateConcurrency": 2 }
  ```

  `./kd env sync` installs or validates the link and durable record. This moves
  capacity; it does not pretend to deduplicate Cargo's mutable state.
- `rustGateConcurrency` caps simultaneous kd Rust gates on this machine; it
  defaults to 2 and must be a positive integer. When every slot is occupied,
  another `./kd test rust` or `./kd build sidecars` waits until a gate finishes.
  Nested kd commands retain their parent's slot. The cap bounds the number of
  simultaneously growing private targets and avoids resource exhaustion and
  Cargo-lock queues; it does not make a raw shared target correct.

Final sidecars and release/package artifacts remain build-private under the
checkout's `.build`; kache does not cache executables. No staging or daemon
launch path may take a final binary from a contested shared Cargo directory.

## MacBook Pro verification (2026-09-09)

Before the full gate: uptime was 45 days, 19:55 (load averages 118.90, 104.78,
71.24); the internal Data volume had 76 GiB free (82% used) and `/Volumes/VHS`
had 542 GiB free (42% used). Two `./kd test rust` gates started concurrently
with 0 B targets on the configured root. The current workspace reached 11 GB
in 27m50s; the other reached 8.9 GB before it was stopped when the first gate
failed. The first gate compiled and ran 1,360 kanna-server tests successfully,
but three existing KSP timing tests timed out under that load
(`assetful_companion_stream_skips_retained_assetless_snapshot_during_upgrade`,
`retained_admission_rejection_does_not_rematerialize_unchanged_multi_source_bundles`,
and `rejected_companion_admission_retries_after_coalesced_asset_demand_churn`).
The targets remained distinct external paths throughout; no shared Cargo target
was used. Apparent `du` is capacity planning evidence, not physical-block use:
APFS and kache may share blocks.
