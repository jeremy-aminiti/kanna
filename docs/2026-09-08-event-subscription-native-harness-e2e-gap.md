# Native harness wake coverage

The event subscription layer has deterministic coverage through its HTTP
routes, persistent mailbox, server-owned observer, and a scripted daemon
socket. The parked-worker regression starts observation after settled idle
without recording a stage verdict and asserts an immediate synthetic event.
It also proves cursor continuation does not repeat that observation. Mailbox
tests cover acknowledgement, retained pending work, restart, and run/stage
binding. The input-adapter test checks the actual fenced logical-input command
and reserved engine provenance. Existing daemon tests cover that command's
submission and draft handling in a real PTY.

The remote-harness E2E case `services an already parked worker and wakes its
subscriber without a harness watcher` also passes through real kanna-server,
daemon, and scripted PTYs. It starts after a manual-stage worker is idle,
acknowledges that initial observation, drives another worker turn, and verifies
the supervisor message in a trace written by the manager's actual PTY process.
The worker's run remains running throughout: no complete_stage call or process
exit supplies a verdict. Run it with:

```sh
pnpm --dir tests/remote-e2e exec vitest run --no-file-parallelism --maxWorkers=1 --maxConcurrency=1 --hookTimeout=240000 --testTimeout=120000 src/terminal-flow.e2e.test.ts -t 'services an already parked worker'
```

The Codex adapter's protocol tests exercise initialize, thread/read, and
turn/start with standalone toolOutput, including refusal of a mismatched
worktree. MCP stdio dispatch tests hold a wait response while another request
completes, proving mailbox access need not queue behind a watcher.

A live Codex TUI wake through its shared app server is not yet an unattended
E2E assertion. It needs an authenticated provider, a version exposing the
shared app-server proxy and standalone tool output, and an isolated live
root thread whose worktree matches the Kanna run. Fresh runs discover that
thread through `thread/loaded/list`; zero or multiple matching roots are
refused, and subagents and unloaded history are never selected. This adapter is explicit
`delivery: codex_app_server`; the default remains the portable input adapter.
No delivery silently changes adapters after an uncertain submission.

The missing live test should launch an isolated Codex manager, subscribe,
let the manager park, settle a worker without calling complete_stage, and
assert the manager resumes, reads the mailbox, and acknowledges the batch.
Repeat with the manager busy: the output must queue for its thread without
interrupting the active turn. Verify the existing TUI shares that thread and
records tool output rather than a fabricated user message. Unsupported proxy
or thread identities must retain the pending page with an observable error.
This belongs in the operator provider-compatibility lane; the same behavioural
fixture can be reused for other native adapters as they are added.

Claude Fable versus Opus background-watcher behavior has not been isolated to
model versus prompting. Neither is a dependency of the server-owned watch.
Cursor retention and cross-machine recovery remain task f63b3698's coverage
and deployment prerequisite for unattended continuity.

## Timing follow-up, 2026-09-09

Work item `6b153714` adds a shared subscription policy (manager-adopted 1s trailing
quiet, 5s max collection, 5s admission floor). New paused-clock worker fixtures use
an isolated scripted executable for the actual Codex proxy path and scripted
daemon I/O for input. They measure admissions, native toolOutput/thread identity,
engine provenance and mailbox continuity. All twelve timing fixtures passed in
the authorized focused Rust slot; see the exact commands and remaining holds in
[the actionable-events verification note](2026-09-09-actionable-events-e2e-gap.md).
They do not establish live Codex TUI timing: authenticated
shared app-server root-thread compatibility, busy-harness consumption and actual
operator-visible wake behavior remain operator E2E gaps. Do not infer a universal
event-creation latency from the conditional scheduler bounds.
