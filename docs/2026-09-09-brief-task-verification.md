# Brief task detail verification — 2026-09-09

Task `9fe6ba82` forked exactly at
`7f54ddbfa7e1d3cc44278ea28c10840b3340229d`. All changes and builds stayed in
its task worktree. No sibling branch was modified.

The initial capacity hold stopped compilation. The owner's later
`RESUME FOCUSED VERIFICATION BRIEF-TASK` explicitly authorized the focused
server, catalog, MCP/CLI tests and scoped clippy with `CARGO_BUILD_JOBS=1`.
Those commands ran sequentially; the filtered integration tests also use one
test thread.
The task-list/filter work in `6543c0b3` remains separate: this catalog diff only
changes `kanna_get_task`, and the adapter changes validate brief-detail responses.

## Measured response sizes

The real HTTP-to-adapter E2E passed against a seeded database containing a
5,000-repeat Unicode task prompt (also the fallback title), a large workflow,
100 ports, a long run summary, provider rejection/override, a durable directive,
and parent/closed-child/blocker links. Full catalog adapter output matched the
full HTTP detail. Brief output retained the operational assertions and contained
one MCP text block, without a second backing JSON payload.

| Measured surface | Full bytes | Brief bytes | Reduction |
| --- | ---: | ---: | ---: |
| Compact task JSON | 256,268 | 2,698 | 98.95% |
| Catalog CLI stdout | 259,244 | 3,009 | 98.84% |
| MCP JSON-RPC stdout | 260,587 | 3,327 | 98.72% |

Typed `task get --brief` also emitted 3,009 stdout bytes. Its pre-existing full
serializer is intentionally unchanged; the catalog CLI is the complete HTTP
view used for the full comparison. These are measured bytes, not token estimates.

Old-peer evidence: both CLI and MCP passed explicit old-local/old-remote peer
checks. An HTTP 200 without the brief version marker becomes
`brief_task_detail_unsupported`; the original terms are not emitted. A missing
or unknown marker version and an old catalog lacking the argument are rejected.
MCP also rejects a confirming re-read that loses the marker. Destination errors
and the cross-machine lookup hint remain actionable errors.

## Added coverage

- `kanna-tool-catalog/tests/brief_task.rs`: explicit/default query behavior,
  machine routing metadata, bad arguments, and missing/unknown version markers.
- `kanna-mcp/tests/stdio_http/brief_task.rs`: real stdio adapter against HTTP
  fixtures; remote query forwarding; old local/remote peer rejection; errors
  and lookup hints; confirmation read capability checks; one content block;
  unchanged full output with long prompt, workflow, and 100 ports.
- `kanna-cli/tests/brief_task.rs`: actual typed and catalog CLI processes;
  exact compact output; remote routing; old-peer and destination errors;
  large full-response content contract.
- `kanna-server/src/http_api/tests/brief_task.rs`: seeded DB through the real
  HTTP router and machine-invoke dispatch. Covers the large prompt-as-title,
  workflow, 100 ports, bounded Unicode summary, provider rejection/override,
  waiting diagnostics, input count, null runtime, parent/closed-child/blocker
  links, closed/runless state, and typed/not-typed/unknown composer semantics.
- `brief_task_http_adapters_e2e` in that server test module runs the real HTTP
  listener and actual CLI/MCP binaries against the seeded database. It checks
  both full and brief responses and emits byte measurements and representative
  brief JSON. It is explicitly ignored until its adapter-binary prerequisites
  are built; it does not silently pass when a binary is missing. Child processes
  use kill-on-drop, and the listener task has a cleanup guard.

## Commands and results

Completed:

```sh
CARGO_BUILD_JOBS=1 cargo test -p kanna-tool-catalog
CARGO_BUILD_JOBS=1 cargo test -p kanna-cli --test brief_task -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test -p kanna-mcp --test stdio_http brief_task -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo test -p kanna-server --bin kanna-server brief_task -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo build -p kanna-cli -p kanna-mcp
CARGO_BUILD_JOBS=1 cargo test -p kanna-server --bin kanna-server brief_task_http_adapters_e2e -- --ignored --nocapture --test-threads=1
CARGO_BUILD_JOBS=1 cargo clippy -p kanna-tool-catalog -p kanna-cli -p kanna-mcp -p kanna-server --all-targets -- -D warnings
CARGO_BUILD_JOBS=1 cargo test -p kanna-cli --bin kanna-cli typed_cli_surfaces_match_catalog_tools_and_params -- --test-threads=1
```

Results: catalog 45 passed; CLI brief integration 4 passed; MCP brief integration
5 passed; server projection 2 passed; explicit real HTTP-to-adapter E2E 1 passed.
The projection filter initially ignores the E2E prerequisite test; the subsequent
explicit invocation ran it successfully. Two initial server fixture failures
were corrected: machine-invoke requests now carry required loopback connection
metadata, and composer assertions respect the existing refusal to update closed
tasks. No production change was needed for those failures.

Scoped clippy passed with `--all-targets -- -D warnings` for all four changed
crates. The typed CLI/catalog parameter surface contract also passed (1 test).

Formatting and whitespace checks passed (`cargo fmt --all`, formatting check,
`git diff --check`). A Python catalog comparison against the fork confirmed that
only `kanna_get_task` changed and existing parameters/defaults/routes stayed
identical. No TypeScript or UI changed. Build/test logs are under the worktree's
ignored `.tmp/brief-*.log`; the measurements and sample below are the durable
record.

Remaining gate: `./kd test all`, full Rust lanes, and workspace-wide builds
remain explicitly held until `RESUME HEAVY VERIFICATION BRIEF-TASK`; the separate
full gates are not claimed here. No app startup or remote MBP work was performed.

## Captured brief output

The following is the actual compact task response from the E2E, pretty-printed
for readability. Its summary and title are prefixes, with truncation flags;
full task terms still require a full detail read plus `kanna_task_inputs`.

```json
{
  "activity": "idle",
  "agentProvider": "codex",
  "agentType": "pty",
  "blockedByTaskIds": [
    "brief-blocker"
  ],
  "branch": "task-brief-task",
  "briefVersion": 1,
  "childTaskIds": [
    "brief-child"
  ],
  "closedAt": null,
  "deliveredInputCount": 1,
  "dirty": false,
  "effort": null,
  "id": "brief-task",
  "latestRun": {
    "agent": "build",
    "finishedAt": null,
    "id": "brief-run",
    "kind": "main",
    "providerOverride": {
      "provider": "codex",
      "source": "operator"
    },
    "resumeFallbackReason": null,
    "resumedFromRunId": null,
    "stage": "in progress",
    "status": "failed",
    "summary": "diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀 diagnostic 🦀",
    "summaryTruncated": true,
    "trigger": "unspecified"
  },
  "machineId": "brief-machine",
  "model": null,
  "parentTaskId": "brief-parent",
  "prUrl": null,
  "providerRejection": {
    "matchedText": "Usage limit reached",
    "observedAt": "2026-09-09 17:33:27",
    "provider": "codex",
    "recovery": "parked-override-binding",
    "rejectedProviders": [
      "codex"
    ],
    "ruleId": "fixture-quota",
    "source": "pty",
    "stage": "in progress",
    "stageRunId": "brief-run"
  },
  "readState": "read",
  "repoId": "repo-brief",
  "revisionLimit": 5,
  "revisionRounds": 0,
  "runtimeSettled": false,
  "runtimeState": null,
  "stage": "in progress",
  "stageTransition": "manual",
  "title": "task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task terms 🦀 task ",
  "titleTruncated": true,
  "view": "brief",
  "waitingPromptSnippet": "Choose a recovery option",
  "workflowName": "single-reviewer",
  "worktreePath": null
}
```
