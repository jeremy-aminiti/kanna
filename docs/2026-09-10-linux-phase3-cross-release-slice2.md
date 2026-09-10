# Linux Phase 3 slice 2: bounded checkpoints and remaining proof

Date: 2026-09-10. Task: `e43677ab`. This is partial evidence, not release
eligibility or completion of Phase 3.

## Provenance

Fetched `origin/main` at `bcc52f9916e3da6c6b94aad1783cd887b04a5565`.
Its parents are `3f9520ae2403aeab1ccc43d774479402d654ab6a` and reviewed
slice-1 head `78e969079ef433c740886a10e23e7bbad5271f54`; its tree equals the
reviewed head's tree. The three apt commits previously ending at `7513ae96d`
replayed patch-identically as `feeda34f0`, `2f854cec3`, `2815ac18b` (checked
with `git range-diff`). Slice 1's merge prerequisite is satisfied.

## Apt adapter and interoperability fixture

The transaction preserves immutable pool/by-hash writes, verifies stored
bytes before signing and commits InRelease last. Its in-memory ordering,
interruption, retry and conflict tests remain unchanged. The injected signer
uses pinned OpenPGP.js 6.3.1, supplied test keys, v4 RSA3072+, SHA512 and the
full primary fingerprint. It awaits library verification, compares verified
plaintext and checks Release expiry. LF/CRLF is the only normalization;
a leading UTF-8 BOM is preserved and rejected instead of silently stripped.

Source provenance, archive hash and license inventory are in
[`tools/kd/OPENPGP-NOTICE.md`](../tools/kd/OPENPGP-NOTICE.md). The emitted Node
adapter includes the library, maps and notices. The bundle smoke test fences
ESM and CommonJS resolution to emitted files and Node built-ins.

`tools/kd/tests/linux-apt-interop.ts` adds the independent Ubuntu acceptance
fixture. It constructs synthetic amd64/arm64 archive metadata through the real
transaction and emitted signer, with memory-only disposable private keys.
Only public keys and signed metadata reach disk. On Ubuntu 24.04 it checks
GnuPG `VALIDSIG` against the full fingerprint, rejects tampering and wrong
keys, and proves that expired metadata can be cryptographically valid while
apt rejects it. Isolated apt state, explicit Signed-By and a loopback HTTP
server test both architecture candidates and by-hash requests. Nothing is
installed and there is no real archive publication. Synthetic payloads are
not installable Kanna packages or release-graph artifact evidence.

The existing `linux-release-check.yml` now has an `apt-interop` job on both
Ubuntu architectures and an `apt_interop_only` dispatch input. That input
skips the native build and installed prerequisites; the installed job depends
on the skipped build. A focused test checks these guards. No other hosted
release graph was introduced.

## Checks executed on the macOS ARM64 Studio

Runtime: installed Node v24.15.0. No host tools were installed. All commands
below exited 0:

- `pnpm --dir tools/kd exec vitest run tests/linux-apt.test.ts tests/linux-apt-publication.test.ts tests/linux-apt-signature.test.ts tests/ci-workflow.test.ts --maxWorkers=1`
  — 107 tests, four files.
- `pnpm --dir tools/kd exec tsc --noEmit`.
- `pnpm --dir tools/kd exec tsx tests/linux-apt-interop.ts --prepare-only`
  — signed both architecture indexes using the emitted adapter; exercised
  fixture construction and adapter rejection, not Ubuntu apt/GnuPG.
- Prior adapter checkpoint: tsup bundle build and `apt-bundle-smoke.mjs`
  passed on this same Studio/Node version, including actual encrypted-key
  signing, verification and dependency closure with packages blocked.

The actual Ubuntu apt/GnuPG fixture has **not run yet**. No hosted result or
Node 22 execution is claimed by the Studio checks. Dispatching the new
workflow requires making this branch available to GitHub first. No branch
push or release publication has been performed by this task.

## Remaining obligations and holds

- Hermetic Zig 0.15.2 cross-release graph, both glibc-2.39 target triples,
  pinned Ubuntu sysroots, eight atomic crate-universe repins, static Ghostty
  C++ closure and Bazel-produced debs: **not implemented or built here**.
  No sysroot hashes or release artifact builder hosts can yet be reported.
- Two-version installed/live-upgrade proof against distinct release-graph
  packages: pending. Slice 1's installed-only result does not prove upgrade.
- Linux release status/ship/promote integration and test-key storage adapter:
  pending; no release command has run.
- Real key custody remains a later named owner operation on the MacBook Pro:
  protected key source, passphrase access, channel fingerprint pins and
  backup/rotation/revocation procedure. No default keyring or real key created.
- Linux-capable historical release tag, Phase 2 M4 gaps and user-facing
  support docs remain the named obligations in slice 1's §7 and the original
  slice-2 prompt. Neither architecture gates on the unavailable Intel Mac.
- Workspace Cargo/native and full verification remain held until
  `RESUME LINUX SLICE2 HEAVY VERIFICATION`. No held gate is reported passed.
