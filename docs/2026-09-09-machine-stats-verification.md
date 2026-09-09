# Machine resource snapshot verification

Task `29696721` started at `7f54ddbfa7e1d3cc44278ea28c10840b3340229d`,
exactly the owner-provided main SHA. No ancestry mismatch.

## Acceptance evidence provided with the task

These are owner/manager-provided observations of the old endpoint, not measurements
of this branch:

| Host / time (2026-09-09) | Native observations | Simultaneous old MCP |
| --- | --- | --- |
| MacBook Pro, about 09:19 MDT (15:19 UTC) | Two top samples: user 26.18 / system 73.53 / idle 0.27%; then 28.32 / 69.24 / 2.42%. Load 14.98 / 16.03 / 14.06. WindowServer, simulator, orphan shells, kernel_task among consumers. | Zero recognized build/test processes; about 1.26 GB available. |
| Studio, about 15:29 UTC | Two top samples: user 44.10 / system 55.90 / idle 0%; then 45.82 / 54.0 / 0.17%. Load 29.15 / 29.38 / 31.42. QEMULauncher PID 30750 / parent 83312 at about 424% process CPU, fseventsd 155.7%, multiple shells and WindowServer. PhysMem 63 GB used, about 13 GB compressor, 237 MB unused. | One recognized cargo, five busy tasks, about 13.22 GB available, normal memory pressure. |

The top sample windows were not supplied. Values are rounded. Process CPU uses
100% per logical CPU and cannot be read as whole-machine percent. This evidence
establishes missing CPU visibility, not incorrect load-average calculation.

## Coverage added

- CPU delta normalization, Linux wait/steal partition, invalid/short/stale/zero
  windows, counter reset and topology changes; Apple Silicon versus Intel Mach
  timebase conversion.
- Non-build saturation, per-process normalization, PID reuse/missing baselines,
  bounded CPU-plus-RSS ranking; recognized runner matching gaps.
- Old relay payloads keep absent CPU/freshness/cores/process/storage fields;
  collection errors survive relay aggregation and invalid envelopes fail.
- Collector sharing, cached failure, and ownership after caller cancellation.
- Real HTTP listener through aggregation and authenticated relay dispatch into
  the sibling router/native sampler while local collection fails; browser auth;
  concurrent HTTP provenance; MCP stdio-to-HTTP preservation of new fields/errors.
- Storage backing-volume identity and missing conventional-directory resolution;
  shared catalog's read-only aggregate route contract.

## Verification status

Focused verification was released with `RESUME HEAVY VERIFICATION MACHINE-STATS`.
All Cargo commands below use `CARGO_BUILD_JOBS=1`; tests use `--test-threads=1`.
Commands run sequentially. `./kd test all` remains explicitly held until
`RESUME FULL GATE MACHINE-STATS`; no full gate or app startup has run.

| Command / step | Result |
| --- | --- |
| Initial `cargo test -p kanna-server machine_stats -- --test-threads=1` | Exit 101 at compilation: libc lacks a declaration for `mach_port_deallocate`. Added its native libSystem binding and localized annotations for deprecated libc Mach declarations. No tests ran. |
| Same command after binding fix | Exit 101: 16 passed, one new auth assertion incorrectly expected 401 where the existing browser-origin policy returns 403. Corrected the test, preserving authorization behavior. |
| `cargo test -p kanna-server --bin kanna-server machine_stats -- --test-threads=1` | Exit 0: 17 passed, including CPU/native, HTTP, relay, old payloads, authorization and concurrency. |
| Native HTTP snapshot comparison described below | Exit 0 for branch test, top, vm_stat, sysctl and df. |
| `cargo test -p kanna-tool-catalog -- --test-threads=1` | Exit 0: 44 tests passed. |
| `cargo test -p kanna-mcp --test stdio_http machine_stats -- --test-threads=1` | Exit 0: machine-stats stdio/HTTP contract passed. |
| `cargo test -p kanna-cli -- --test-threads=1` | Exit 0: 126 tests passed. |
| `cargo clippy -p kanna-server -p kanna-tool-catalog -p kanna-mcp -p kanna-cli --all-targets -- -D warnings` | Exit 0, no warnings. |
| `pnpm --dir packages/core exec vitest run src/workflow/qa-assets.test.ts --maxWorkers=1 --minWorkers=1` | Exit 0: 60 tests passed. |
| `pnpm --dir packages/core exec tsc --noEmit` | Exit 0. |
| `cargo fmt --all` and `git diff --check` | Passed. |

### Bounded native comparison

Host: **Jeremys-Mac-Studio.local**, Apple Silicon, 20 physical / 20 logical
cores. Comparison ran **2026-09-09 16:38:12–16 UTC**. The branch's native route
test used an isolated test database: task count and repo paths were fixtures;
CPU, process, memory and filesystem counters were real host reads. This avoids
mutating the installed server or any live tasks.

The CPU window was **16:38:12.973–13.474 UTC**, measured as **500 ms**;
collection took **516 ms**, with zero cache age. The route reported:

| Metric | Branch | Native comparison |
| --- | --- | --- |
| Aggregate user / system / idle | 21.185 / 58.936 / 19.880% (80.120% busy) | The overlapping top sample ending about 16:38:14 UTC: 21.38 / 58.90 / 19.70% (rounded). The following samples had 18.1% and 14.91% idle as load changed. Discarded top's first, unprimed process sample. |
| bash PID 5543, parent 1 | 35.65% of one logical CPU over 509 ms | top: 32.5%, then 35.2% over its longer adjacent intervals. Both show a non-build consumer; neither number is machine-normalized. |
| QEMULauncher PID 30750, parent 83312 | 6.78% CPU, 23,434,166,272 resident bytes | A later bounded ps RSS read was 22,888,736 KiB (23,438,065,664 bytes). This is resident memory, not top's MEM/footprint accounting. The owner's earlier 424% CPU observation was over an hour earlier and is not a simultaneous comparison. |
| Compressor physical occupancy | 7,578,583,040 bytes | vm_stat: 462,560 pages × 16,384 bytes, exactly equal. |
| Swap total / used | 3,221,225,472 / 1,458,896,896 bytes | sysctl: 3072.00 / 1391.31 MiB, matching to display precision. |
| Storage total / available | 994,662,584,320 / 80,362,156,032 bytes | df: 971,350,180 / 78,478,604 KiB; total equal, available differs by 64 KiB between reads. Repo/build/temp all mapped to the same Data volume and produced one row. |

The collector enumerated **1123 processes**, sampled **777**, and explicitly
reported **346 unavailable**. `top` also displayed privileged `fseventsd`,
`kernel_task` and WindowServer consumers which this ordinary-user collector
could not sample. Its top list is explicitly partial; aggregate CPU remains
independent and captures the host's busy time. No permissions were escalated and
no unrelated processes were stopped.

Memory was 68,719,476,736 total, 39,766,441,984 legacy used, 3,038,920,704 free,
and 23,840,210,944 estimated available bytes, with normal *memory* pressure.
Top's unused figure includes speculative pages, while the retained free field
subtracts them. Its used/MEM accounting differs from the endpoint's documented
legacy sysinfo formulas and resident process memory. Available includes estimated
reclaimable memory; it is not unused RAM or proof that CPU is idle.

No Linux host was used for the runtime comparison. Installed toolchain targets
on this host are Apple targets only; Linux runtime behavior remains a stated
verification limitation. Shared delta/window and normalization tests did run.

Logs and native JSON are under the owned `.tmp/` directory; this note preserves
the relevant measurements after worktree cleanup. All owned comparison processes
exited. No push, PR or stage advance has been performed.

The focused verification slot is complete. Final lightweight changes clarified catalog timestamp wording and recorded
evidence; no scheduling, model-selection or brief-task-detail
behavior was added. Resource guidance edits affect one existing paragraph;
independent model-triplet commit `cc412ba26` remains separate for eventual
reconciliation. The full gate was not run and is not required to conclude this
explicitly authorized focused slot.
