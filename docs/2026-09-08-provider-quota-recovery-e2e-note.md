# Provider quota recovery: why its E2E coverage lands in two halves

**Date:** 2026-09-08
**Subject:** `docs/specs/provider-quota-recovery.md`

## What the behavior crosses

A provider quota rejection travels: real CLI chrome → PTY → daemon classifier →
`ProviderNotice` broadcast → `kanna-server`'s terminal-state watcher → the
durable rejection record → the stage-run preparation → a `Spawn` back across the
daemon socket. Every boundary in `AGENTS.md`'s E2E expectation is on that path.

## Why there is no single lane that runs all of it

No lane in `./kd test all` starts a real `kanna-server` **and** a real
`kanna-daemon` together:

- `crates/daemon/tests/*` start the real `kanna-daemon` binary
  (`CARGO_BIN_EXE_kanna-daemon`, available only inside the daemon package) with
  real PTYs, and no server.
- `crates/kanna-server/tests/*` start a real `kanna-server` process against a
  *fake* daemon socket; the daemon binary's path is not resolvable from that
  package.
- The lane that does run both — `apps/desktop/tests/e2e/real/` — is
  deliberately **not** part of `./kd test all` (see `docs/dev/testing.md`), and
  a quota rejection needs an exhausted account or a substituted CLI, which the
  unattended real tier does not provide.

## What is covered instead, and where the seam is

The path is cut once, at the `ProviderNotice` event, and both halves are real
on their own side of it:

| Half | Suite | Real | Substituted |
|---|---|---|---|
| CLI → daemon → notice | `crates/daemon/tests/provider_quota_notice.rs` | daemon process, PTY, classifier, version probe, broadcast | the provider CLI (a script printing the measured chrome) |
| notice → server → spawn | `crates/kanna-server/src/task_creator/tests/quota_recovery.rs` | watcher, recovery, rejection record, stage-run preparation, git worktrees, spawn command | the daemon socket |

The event at the seam is not hand-written twice: the daemon suite asserts the
exact fields the daemon emits (`kind`, `agent_provider`, `rule_id`, `scope`,
`text`, `cli_version`, `session_kind`), and the server suite constructs
`Event::ProviderNotice` from the same typed protocol struct, so a field change
breaks compilation on both sides rather than passing on one.

The chrome itself is version-tagged in
`tests/cli-contract/fixtures/provider-quota-rejection.json` and read by both the
daemon's classifier tests and the daemon E2E, so the patterns and the frames
they were measured against cannot drift apart.

## What would make a single-lane test possible

A harness that starts `kanna-server` against a real `kanna-daemon` binary —
`tests/remote-e2e/` already runs both as headless binaries, but is an opt-in
lane rather than part of the merge gate. Adding a merge-gated
server+daemon+PTY harness is worth doing on its own terms; it is not this
change's to add.

## Narrower coverage added meanwhile

- `crates/daemon/src/detection/rules.rs` (`quota_notice_tests`) — the measured
  captures against the real classifier: both providers, the narrow-terminal
  wrap, scope extraction, the cross-provider negative, the unmeasured-version
  refusal, and that a refused frame still classifies `idle`.
- `crates/kanna-agent-protocol/tests/claude_adapter.rs` (`rate_limit`) —
  allowed, warning, rejected, sparse, missing and unknown payloads.
- `tests/cli-contract/tests/offline/provider-quota-rejection-contract.test.ts`
  — the captures carry their CLI version, origin, wrapped form, and negatives.
