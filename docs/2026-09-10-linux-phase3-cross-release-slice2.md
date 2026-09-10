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

## Hosted apt/GnuPG proof — 2026-09-10

Task Manager (declared source `manager`), within the owner's broader goal,
explicitly authorized pushing clean `task-e43677ab` at
`e1b42f9e067805db05a3e2e9ea3d1363051f58a6` and dispatching only the prepared
interop job. Push and dispatch exited 0.
[Run 34520900452, attempt 1](https://github.com/tampopogk/kanna/actions/runs/34520900452)
tested that exact SHA and concluded **success**. `gh run watch --exit-status`
exited 0. The native build, installed prerequisite and installed-check jobs
were all **skipped**, as requested by `apt_interop_only=true`.

| Hosted runner | Architecture | Job | Result |
| --- | --- | --- | --- |
| `ubuntu-24.04` | amd64 (`linux/x64`) | [103017870013](https://github.com/tampopogk/kanna/actions/runs/34520900452/job/103017870013) | success, 24 seconds |
| `ubuntu-24.04-arm` | arm64 (`linux/arm64`) | [103017870291](https://github.com/tampopogk/kanna/actions/runs/34520900452/job/103017870291) | success, 25 seconds |

Both hosts reported Node **22.23.2**, apt **2.8.3** and GnuPG/gpgv **2.4.4**.
On each host, tsup, the guarded `apt-bundle-smoke.mjs` and
`tsx tests/linux-apt-interop.ts` exited **0**. Both architecture candidates and
SHA256 by-hash requests were asserted on each host. The actual negative-case
exit codes, identical on both hosts, were:

| Case | gpgv exit | apt-get update exit |
| --- | --- | --- |
| Valid | 0, full `VALIDSIG` fingerprint matched | 0 |
| Tampered signed text | 1 (`BADSIG`) | 100 (`BADSIG`) |
| Wrong verification key | 2 (`NO_PUBKEY`) | 100 (`NO_PUBKEY`) |
| Expired metadata, cryptographically valid | 0 | 100 (expired Release) |

Logs included Node's temporary-output module-type warning and apt's warning
that the intentionally disabled `/etc/apt/-/` config directory does not exist.
Neither was suppressed or interpreted as a failure. No real keys, package
installation, apt publication or release command was used. This proves the
test-key signature/metadata boundary and actual Node 22 bundle closure, not
release-graph package acceptance or the two-version upgrade lane.

Before the dispatch, `origin/main` was fetched at Recovery merge
`0a758b63d31bb94d0005a5b9adbd3fd1ffcbee4d`. Its merge base with the dispatched
head is `bcc52f9916`; there are no intervening main changes in
`tools/kd`, `tests/linux-installed`, `pnpm-lock.yaml` or
`.github/workflows/linux-release-check.yml`. The tested SHA was preserved.
Reconcile newer main again before later integration.

## Remaining obligations and holds

The upgrade harness's known SQL404 was repaired at its observation boundary:
repo/task identities now come from production CLI responses, delivered input
from `/v1/tasks/{id}/inputs`, and completion from task detail and the scoped
`/v1/task-events` feed. It pins run id, branch and worktree across restart in
addition to the existing agent pid/start-time assertions. The installed-only
harness no longer exposes a SQL helper or requests the test-only SQL route.
On the Studio, `pnpm --dir tests/linux-installed exec tsc --noEmit` and
`pnpm --dir tests/linux-installed exec vitest run src/installedLayout.test.ts --maxWorkers=1`
both exited 0 (six layout tests). The actual two-version E2E test remains
unexecuted; this is a source/type correction, not a live-upgrade pass.

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
