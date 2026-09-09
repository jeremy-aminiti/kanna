# Task-pull PTY fixture coverage and remaining gap (2026-09-09)

The focused task-pull verification covers each existing boundary, but a single
real server→daemon→PTY finalizer fixture would require a new cross-package test
harness. The boundaries remain separate:

- `crates/daemon/tests/reconnect.rs` starts the real daemon and a real PTY
  child. Its submission tests prove that the daemon writes the preparation
  bytes, including the carriage-return boundary, before acknowledging the
  write. They do not invoke the server finalizer or observe a provider reply.
- `crates/kanna-server/src/transfer_engine/finalize.rs` exercises the shared
  push/pull finalizer through deterministic fake daemon connections. Those
  tests reproduce the legacy missing-Enter response, prove it cannot release a
  quit, require a post-submission `Busy` → `Idle` lifecycle, and cover
  preclaim/uncertain delivery, PID fencing, replacement/absence, provider
  lookup, and quit suppression. They have no real child PTY.
- `tests/cli-contract/tests/live/opencode-injected-input.test.ts` drives a real
  provider through the current one-buffer submission shape, waits for a
  provider-produced preparation response that cannot be confused with the
  echoed prompt, then sends the registry's quit command separately. The Codex
  and Copilot quit fixtures use that same current submission shape.
- Server HTTP tests likewise use fake daemon sockets, so they cannot establish
  a real finalizer→daemon→PTY chain or observe a distinct quit command after a
  preparation response.

The concrete missing seam is that the server finalizer accepts an already-registered
session id and daemon directory (`finalize.rs:176-219` and
`http_api/task_input.rs:145-176`), while the real-daemon fixture must first
drive the daemon's private `Spawn` registration and retain its observed PTY
pid. The only existing real-daemon setup is the private test-local
`DaemonHandle`/spawn protocol in `crates/daemon/tests/reconnect.rs`; it is not
exported to the `kanna-server` test crate. Copying that setup alone would still
need server `AppState`/DB task registration and the finalizer's transfer-work
rows before `finalize_source_session` can be invoked. No production API is
missing, but wiring those private registrations is the bounded integration
fixture work still required.
The raw daemon receipt remains only a byte-delivery acknowledgement; it is not
evidence that a provider parsed Enter. Provider command parsing and the
server-side lifecycle remain separate even though both now have regression
coverage. The missing fixture must cover preparation text+CR at the child,
provider-observable preparation response, separate quit ordering, and the
shared push/pull entry point before it can close this gap.
