# Task-pull PTY fixture gap (2026-09-09)

The focused task-pull verification could not add the requested real
server→daemon→PTY finalizer fixture without creating a new cross-package test
harness. The existing boundaries are separate:

- `crates/daemon/tests/reconnect.rs` starts the real daemon and a real PTY
  child. Its submission tests prove that the daemon writes the preparation
  bytes, including the carriage-return boundary, before acknowledging the
  write. They do not invoke the server finalizer or observe a provider reply.
- `crates/kanna-server/src/transfer_engine/finalize.rs` exercises the shared
  push/pull finalizer through deterministic fake daemon connections. Those
  tests cover preclaim/uncertain delivery, PID fencing, replacement/absence,
  provider lookup, and quit suppression, but have no real child PTY.
- Server HTTP tests likewise use fake daemon sockets, so they cannot establish
  a real finalizer→daemon→PTY chain or observe a distinct quit command after a
  preparation response.

Therefore this run adds no fixture and makes no production or protocol change.
The concrete seam is that the server finalizer accepts an already-registered
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
evidence that a provider parsed Enter. Provider command parsing and the measured
Codex `/model` behavior remain separate, version-recorded coverage still
required. The missing fixture must cover preparation text+CR at the child,
provider-observable preparation response, separate quit ordering, and the
shared push/pull entry point before it can close this gap.
