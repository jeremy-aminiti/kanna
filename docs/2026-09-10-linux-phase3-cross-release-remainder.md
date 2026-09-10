# Linux Phase 3 cross-release remainder: source inventory and slice boundary

Date: 2026-09-10. Task: `f3abc02f`.

This is a source/evidence inventory, not release evidence. It follows the
bounded apt implementation in task `e43677ab` and does not repeat or broaden
that task's transaction, signer, bundle, interoperability, or upgrade-harness
work. The apt branch remains a parent-first dependency for integration.

## Provenance and dependency state

The initial inventory ran at
`60d9aa62102bbe324b20febcf29a5fe01a898412`, exactly the reviewed apt head,
while it was not yet present on main. On the subsequent implementation release,
`git fetch origin` resolved `origin/main` to
`42011e5f60a5544728c87a201971cd47443268e9`, merge commit for PR #1426, and
`git merge-base --is-ancestor 60d9aa621 origin/main` exited 0. This branch
fast-forwarded to that exact main commit; the uncommitted inventory document
survived the rebase. The parent-first dependency is satisfied.

During the final source audit, `origin/main` advanced again to
`a92e68139a41f513ba8d4d33b8cf48619f76ccb3` (PR #1401). Its intervening files
do not overlap this slice. A second `git fetch` followed by
`git rebase --autostash origin/main` completed without conflict, restored all
local source changes and the inventory document, and left this branch exactly
at `a92e68139` before its working-tree delta.

The apt evidence remains exactly what
[`docs/2026-09-10-linux-phase3-cross-release-slice2.md`](2026-09-10-linux-phase3-cross-release-slice2.md)
records: synthetic archive metadata and test keys passed the real Ubuntu 24.04
apt/GnuPG boundary on amd64 and arm64. It is not a cross-built package, an
installed two-version upgrade, release acceptance, storage publication, or a
real-key custody proof.

## Cross-release source inventory

### Bazel platforms, compilers, and Rust universes

- `MODULE.bazel` pins one `rules_zig` extension and Zig 0.15.2. Ghostty build
  scripts already consume its resolved execution toolchain; no second Zig is
  needed or permitted by the approved design.
- `rules_zig` 0.12.3's built-in Zig target toolchains use
  `x86_64-linux-gnu.2.17` and `aarch64-linux-gnu.2.17`. They cannot silently
  stand in for the owner-approved glibc 2.39 floor. Kanna needs two explicitly
  named Zig target toolchains using `x86_64-linux-gnu.2.39` and
  `aarch64-linux-gnu.2.39`, selected by Linux platform constraints while the
  existing resolved Zig compiler remains the execution toolchain. Those
  `rules_zig` targets configure Zig rules; they do not register the
  `@bazel_tools//tools/cpp:toolchain_type` consumed by Rust `-sys` build
  scripts and ordinary C/C++ actions. The same pinned Zig executable also
  needs a repository-owned `cc_toolchain` wrapper whose compiler/linker actions
  call `zig cc`/`zig c++` with the selected `.2.39` target. This is one compiler
  download and two toolchain interfaces, not a second Zig installation.
- `tools/bazel/BUILD.bazel` declares only `macos_x86_64`; the native ARM64
  platform is implicit. There are no repository-owned Linux platform labels,
  ABI constraints, sysroot repositories, or Linux toolchain smoke targets.
- All eight `crate.from_cargo` calls in `MODULE.bazel` list only the two Darwin
  triples. The atomic repin therefore touches `MODULE.bazel`, all eight Cargo
  lock inputs as required by actual resolution, and `MODULE.bazel.lock`; a
  partial repin must not be committed.
- `tools/kd/tests/release-cargo-locks.test.ts` already discovers all eight
  universes and compares requested Cargo features with generated pins, but it
  intentionally unions platform `select()` branches and skips missing
  platform-specific features. Linux work must add an explicit assertion that
  every universe contains both Linux triples and that the Linux-specific
  feature branches are present, without weakening the existing union check.
- `crates/kanna-worker` has no `BUILD.bazel` and is absent from every synthetic
  universe. Its dependencies align with the server universe (`kanna-daemon`,
  `kanna-runtime-defaults`, `kanna-server-process`, Tokio, serde and libc), so
  adding it to `Cargo.server.toml` is the smallest coherent ownership choice;
  creating a ninth universe would duplicate the daemon/server graph and
  contradict the required eight-universe atomic repin.

### Sysroot and native build-script boundary

- The native prototype installs `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, and
  `libsoup-3.0-dev` on the build host. No source currently resolves a Noble
  package closure, records package URLs/hashes, unpacks Debian archives into a
  Bazel repository, or supplies target pkg-config paths.
- The sysroot seed must include the three development packages above and the
  runtime-policy packages needed by their link/runtime closure. Resolution
  must be per Debian architecture (`amd64`, `arm64`) against one pinned Ubuntu
  24.04 snapshot. The committed lock must record snapshot identity, package,
  version, architecture (`all` where applicable), URL and SHA-256 for every
  `.deb`; repository evaluation downloads only those locked bytes and verifies
  every digest before unpacking.
- Extraction must stage each `data.tar.*` payload before overlaying it, retain
  the first lock-ordered copy of an identical duplicate, and reject duplicate
  paths whose type, mode, contents, or normalized symlink target differ.
  Maintainer scripts and package database state are not part of a compile
  sysroot. Absolute Debian symlink targets are rewritten relative to their
  link parent so they name the same path beneath the exported sysroot; relative
  targets that lexically climb above the sysroot are rejected. The overlay's
  audit report records every rewritten symlink and accepted identical file or
  symlink duplicate.
- The `-sys` closure is generated by crate_universe, so target variables belong
  in platform-selected `crate.annotation(build_script_env = ...)` entries:
  `PKG_CONFIG_SYSROOT_DIR`, target-only `PKG_CONFIG_LIBDIR`, and the appropriate
  allow-cross setting. `pkg-config` itself and every other executable build
  helper remain execution-platform tools. No host `/usr/lib`, Homebrew path,
  or host `PKG_CONFIG_PATH` may leak into an action.
- `openssl-src` is annotated only in `desktop_crates`, although CLI/MCP/server
  native-TLS paths are known consumers. Extending the existing vendored source
  annotation to the affected universes belongs with the Rust graph slice, not
  with the sysroot foundation, because it changes generated crate targets.

### Products and packaging

- Root `BUILD.bazel` is a macOS bundle/release graph. Its copy targets embed
  Apple triples in output names, and all app aggregation flows through
  `tauri_macos_app`, signing, DMG and notarization rules. Linux targets must be
  siblings selected by Linux platforms, not conditionals inside the signing
  rules.
- The CLI, MCP, server, daemon, task-transfer, terminal-recovery and desktop
  BUILD files model native ARM64 plus a `platform = macos_x86_64` twin. They do
  not yet expose either Linux architecture. Darwin SDK/framework assumptions
  must remain confined to Darwin targets.
- The prototype `./kd build linux-package` compiles with Cargo and host
  pkg-config, then stages and audits eight executables. Its staging layout,
  runtime-policy audit and `dpkg-deb` assembly are the contract to reuse. A
  release path must accept only declared Bazel outputs; the prototype must be
  explicitly unable to feed publication.
- `release-platform.ts` already defines separate Linux channel tags, series
  branches, apt archive kind and apt key ownership. `release.ts` still owns
  macOS constants and Bazel targets directly. Threading Linux through
  status/ship/promote is a later integration slice after real Bazel `.deb`
  targets exist; doing it first would create a command that promises artifacts
  the graph cannot produce.

### Remaining acceptance surfaces

- `tests/linux-installed` has passed install-only lifecycle checks on both
  hosted Ubuntu 24.04 architectures, and the apt slice corrected its upgrade
  observations to production APIs. No run has installed two distinct versions
  built by the cross-release graph. Both architecture-specific old/new artifact
  pairs remain missing.
- `crates/daemon/tests/support/previous_daemon.rs` still pins
  `v0.1.0-staging.1`, whose daemon cannot start on Linux. The skip is correct.
  The first historical fixture update requires a real tagged Linux-capable
  release; an untagged prototype or synthetic fixture would not prove the
  cross-version release contract.
- Phase 2 evidence still names IME/dead-key composition, native file drag,
  scaling, real Linux desktop lanes, paired-mobile LAN, credential behavior and
  two simultaneous human-observed instances as gaps. None is upgraded to
  passed by this inventory.
- Concrete storage-adapter implementation and a test-key provisioning runbook
  remain source work. Creating or importing the real apt key, choosing its
  protected storage, publishing, promotion and production release remain named
  human operations.

## Next review-sized implementation slice

The next coherent slice is **Linux cross-toolchain and sysroot foundations**,
after PR #1426 merges. It should contain only:

1. repository-owned amd64/arm64 Linux platform labels, custom Zig 0.15.2
   target toolchains fixed to the two `.2.39` triples, and matching
   repository-owned Bazel C/C++ toolchains backed by that same Zig executable;
2. a reproducible Noble sysroot resolver plus two committed content-addressed
   locks and a repository rule that verifies and unpacks them;
3. target-only pkg-config environment plumbing demonstrated by a small GTK /
   WebKitGTK canary action for each architecture; and
4. focused lock/schema, extraction-conflict, symlink-containment, toolchain
   selection and host-path-leak tests.

This slice deliberately stops before Rust toolchain registration, the atomic
eight-universe repin, `-sys` crate generation, `kanna-worker`, full products,
static C++ linkage, `.deb` assembly, installed upgrades, CI or release command
wiring. That boundary gives reviewers a directly testable foundation without
mixing a large generated lockfile with repository-rule and toolchain design.

## Foundation implementation checkpoint

The bounded foundation is now present in source and has completed the focused
Bazel verification recorded below:

- `MODULE.bazel` imports the execution-platform repositories from the existing
  Zig 0.15.2 extension, instantiates the two Noble sysroot repositories and
  registers the repository-owned Linux Zig-target and C/C++ toolchains.
- `packaging/linux/resolve_sysroot.py` resolves `main` and `universe` from the
  immutable Ubuntu snapshot `20260909T000000Z`. Its seeds are every package in
  `runtime-policy.json` plus GTK3, WebKitGTK 4.1, libsoup 3 and Ayatana
  AppIndicator development headers. The committed locks contain the index and
  package URLs, sizes and SHA-256 digests: 400 packages for amd64 and 398 for
  arm64.
- `sysroot_repository.bzl` validates the lock, downloads every `.deb` by hash,
  and uses the execution host's artifact from that same Zig extension (`zig
  ar` and a repository-owned C overlay helper compiled by `zig cc`). Bazel's
  archive extractor expands each `data.tar.zst`/`.xz`/`.gz` into an isolated
  staging tree; the helper then rewrites absolute links into sysroot-relative
  links, rejects escaping relative links and conflicting overlays, and records
  rewrites/accepted identical duplicates before the staging tree is removed.
  This adds no Homebrew or host `ar`, `readlink`, `cmp`, `dpkg`, `tar`, compiler
  or second Zig dependency.
- `linux_cc_toolchain_config.bzl` defines six execution/target combinations:
  both Linux targets from macOS ARM64, Linux amd64 and Linux arm64 execution
  hosts. Every compiler wrapper fixes its target to `.2.39`, supplies only the
  locked sysroot headers/libraries, and carries those files plus the selected
  Zig SDK as declared toolchain inputs.
- The two C canaries include and link GTK3 and WebKitGTK using the architecture
  specific toolchain/sysroot. They are compile/link probes only and are never
  executed on the macOS build host.

The source checkpoint does not add Rust Linux triples, repin a universe, expose
a Linux product target, package a `.deb`, or change CI/release commands. The
later `-sys` annotation slice still owns `PKG_CONFIG_SYSROOT_DIR`,
`PKG_CONFIG_LIBDIR` and execution-platform pkg-config plumbing; no host
pkg-config path was added here.

## Bounded verification checkpoint

The containment/collision correction after review of source head `2053cbefc`
was verified on the Apple Silicon Studio at exact source head
`87e1d6b83a041a3a64ef3acc7be3bb880d2cd6a9`. The owner released a focused
three-command tranche only; all commands ran sequentially with `--jobs=1`:

1. `bazel test //packaging/linux:resolve_sysroot_test --jobs=1` exited 0 in
   43.365 seconds after reevaluating the 400-package amd64 repository. All 14
   tests ran with no skip: the prior eight resolver/toolchain tests, fixtures
   for absolute-target rewrite, rejected relative escape, unchanged in-root
   link, accepted byte/mode-identical duplicate and rejected conflicting
   duplicate, plus inspection of the evaluated repository audit output.
2. `bazel build //tools/bazel:linux_toolchain_canary_x86_64
   --platforms=//tools/bazel:linux_x86_64 --jobs=1` exited 0 in 5.809 seconds.
   The output remains an ELF64 x86-64 executable with interpreter
   `/lib64/ld-linux-x86-64.so.2`.
3. `bazel build //tools/bazel:linux_toolchain_canary_arm64
   --platforms=//tools/bazel:linux_arm64 --jobs=1` exited 0 in 43.946 seconds
   after reevaluating the 398-package arm64 repository. The output remains an
   ELF64 AArch64 executable with interpreter
   `/lib/ld-linux-aarch64.so.1`.

Direct post-build inspection on that Studio found both repositories export
`usr/lib/python3.12/sitecustomize.py` as
`../../../etc/python3.12/sitecustomize.py`. Both audit reports record the
original `/etc/python3.12/sitecustomize.py` target and contain no staging or
host path. Before Bazel was released, the lightweight Python invocation ran
the prior eight tests successfully and reported the Bazel-only integration
class as one skip; it was not counted as integration proof.

The released commands causally exposed and corrected Bazel 9's explicit
`cc_binary` load, the registered external-repository name, use of the pinned
rules_zig macOS SDK wrapper for compiling the repository helper, Bazel's
staging-root rewrite of archive-absolute symlinks, and the need to watch the
helper source as a repository input. Failed attempts produced no accepted
repository or canary evidence. `./kd test all` remains separately queued and
was not run by this focused release.

The owner released only the focused sysroot test and two canary builds on the
Studio, sequentially with `--jobs=1`. The coherent pre-verification source
checkpoint was `a2da9561af41c7898646b3de8c3bd4c5aed53689`. Causal Bazel 9, repository
extraction, wrapper and canary-link defects found by the released commands were
fixed within this slice and committed before their retries. The exact final
source head exercised successfully was
`19281d092` (`19281d09285b87e316a2a5945a769f79b23fa3c6`).

On the Apple Silicon Studio:

- `bazel test //packaging/linux:resolve_sysroot_test --jobs=1` exited 0 at the
  final source head. All eight tests passed in Bazel's `darwin-sandbox`.
- `bazel build //tools/bazel:linux_toolchain_canary_x86_64
  --platforms=//tools/bazel:linux_x86_64 --jobs=1` exited 0. Its first uncached
  sysroot extraction fetched and merged the 400-package amd64 lock. The output
  is an ELF64 x86-64 executable using `/lib64/ld-linux-x86-64.so.2`.
- `bazel build //tools/bazel:linux_toolchain_canary_arm64
  --platforms=//tools/bazel:linux_arm64 --jobs=1` exited 0 in 206.876 seconds.
  Its first uncached sysroot extraction fetched and merged the 398-package
  arm64 lock. The output is an ELF64 AArch64 executable using
  `/lib/ld-linux-aarch64.so.1`.

Apple `/usr/bin/objdump -p` reports the same direct dynamic dependencies for
both outputs: `libgtk-3.so.0`, `libwebkit2gtk-4.1.so.0` and `libc.so.6`. The
emitted execution-host wrappers use
`external/rules_zig++zig+zig_0.15.2_aarch64-macos/zig`, set `ZIG_LIB_DIR` to
that declared SDK's exec-root-relative `lib` directory, select the matching
content-addressed Noble sysroot, and invoke `zig cc` with respectively
`-target x86_64-linux-gnu.2.39` and `-target aarch64-linux-gnu.2.39`. Zig cache
state is per Bazel sandbox; no Homebrew compiler, host GTK, host pkg-config or
second Zig is involved.

The released commands initially exposed and boundedly corrected: Bazel 9's
explicit `rules_cc` rule/config-provider imports, Python test runfiles,
Starlark repository API differences, `/usr/bin/X11` symlink-loop traversal,
private generated-Zig label use, generated tool-path resolution, wrapper shell
quoting and cache setup, the Darwin wrapper's inappropriate SDK injection for
Linux actions, Zig builtin-header declaration, and an unnecessary transitive
GLib reference in the canary. Failed repository attempts did not produce an
accepted artifact.

The earlier source-only checks also remain passed: eight Python unit tests and
both snapshot-backed lock checks exited 0. The canonical lock SHA-256 values
are `edb2c6a0a65bdee0c39189f633045397834ba97352a830bc85b9a63f9a2d1604`
(amd64) and `36cbfd261645a958dd926151e3b3dadcab46dba5d787f6b12d4d65f88498e9e5`
(arm64).

This checkpoint is only macOS-host cross-toolchain/sysroot/canary evidence. It
does not prove the eight-universe Rust repin, any full product or `.deb`, static
Ghostty C++ linkage, native/hosted parity, installed or two-version upgrade
lanes, release integration, apt publication, or real-key custody. All of those
remain held for later explicitly authorized slices and verification releases.
