# Concurrent gates shared one `/tmp`: what was collision, and what is left

**2026-09-08.** This machine runs several tasks' gates at once, each from its
own worktree. `std::env::temp_dir()` is the one directory they all share, and
`kanna-server`'s test paths were built from a label alone — so two runs named
the same database file, and `Db::open_for_tests` deletes the file it is handed.
One run truncated another run's database mid-test.

Reviewers spent a week reading the results as machine load, because that is what
they look like from the inside. This is the measurement, so the next person does
not re-litigate it.

## The measurement

One test binary, `kanna_server-*` (1357 tests), run three ways on the same
machine:

| Run | Result |
|---|---|
| One copy alone, before the fix | 1355/1355 green |
| Two copies concurrently, before the fix | **28 failed / 16 failed** |
| Two copies concurrently, after the fix | 1357/1357 green, both — see the residue below for what later rounds still surfaced |

The 44 failures were not a flaky subset of one list — the two copies failed
mostly *different* tests, which is itself the signature. Their messages:

- `open test db: ... "table repo already exists"` — the other run created the
  schema in the file this one had just emptied.
- `open test db: SqliteFailure(SystemIoFailure, "disk I/O error")` and
  `no such table: worktree` — the other run deleted the database and its WAL
  under a live connection.
- `left: "file:///…/kanna-checkout-happy-35414-…/remote"` vs
  `right: "…-35435-…"` — one run read a row the *other* process's fixture had
  written. Two different pids in one assertion is the proof that the database
  was shared.
- The rest: `500` where a `200`/`204`/`409` was expected, from handlers whose
  database had vanished.

## The fix

`crates/kanna-server/src/test_paths.rs` is the crate's one temp-path mechanism:
a per-process root `$TMPDIR/kanna-test-<pid>/`, with a monotonic counter inside
it. `Db::test_db_path` routes through it, so all 165 of its callers became
unique without touching one of them, and the crate's six near-duplicate ad-hoc
helpers now delegate to it rather than each reimplementing the idea.

A wall clock is deliberately not part of it. A timestamp is what *looked* like
uniqueness in the helpers that had one; two processes started together read the
same nanoseconds.

**Uniqueness without reclamation is a leak, and this one had already bitten.**
A suite that hands out a fresh database per test and never takes one back grows
without bound, and the retired `db/tests.rs` helper named its files
`<nanos>-<counter>` — no pid, so nothing could ever tell whose they were. The
temp directory held 142,452 kanna test entries when this task looked, 42,713 of
them older than a day, and a `./kd test all` here failed with `ENOSPC: no space
left on device` at 978 MiB free. Reclaiming the entries whose owning process was
gone freed 14.9 GB.

One directory per process is what makes that collectable: `sweep_abandoned_roots`
removes sibling roots whose pid no longer answers `kill(pid, 0)`, and only
those, so a gate running right now in another worktree is never touched.

`kanna-daemon`, `task-transfer` and `kd` were audited and already correct —
every temp path in them carries `process::id()`, and `kd`'s helpers use
`mkdtemp`. Four wall-clock-only roots in `apps/desktop/src-tauri` tests got the
pid they were missing.

Two things beyond the paths themselves:

- **The test-sidecar guard's reach did not match its resource.** Tests stage a
  fake `codex` / `kanna-mcp` beside the test executable and serialized on an
  in-process mutex, but two `cargo test` runs *for the same worktree* share that
  directory. One deleted the fixture the other was resolving, and the resolver
  silently fell through to a real `codex` on `PATH`. The guard now also takes an
  `flock`, which is released when the process dies, so a panicking test wedges
  nothing.
- **`tools/kd/tests/rust-test-temp-paths.contract.test.ts`** keeps the class
  dead. It is a source scan in `pnpm test` — the lane that runs every time —
  following the placement argument in
  [2026-09-06-testing-infra-inventory.md](2026-09-06-testing-infra-inventory.md):
  this defect kept coming back one callsite at a time because nothing made the
  rule checkable.

## What the "load flakes" actually were

The tests that had been attributed to load were re-run after the fix: nine full
gate runs, all concurrent with a second heavy gate, on a box at load average
60–75.

**None of the named ones reproduced, once.** Not the readiness timeouts, not the
`terminal_watcher` reconcile deadline, not
`relay_client::tests::connection_error_includes_normalized_real_http_refusal_reason`,
not `ksp::tests::loopback_ksp_delivers_ordinary_input_to_merge_singleton`, and
not `provider_resolution_http::checkout_then_create_task_succeeds_through_running_server`
— its binary was run six more times as two concurrent copies of itself beside a
full gate, and all nine of its tests passed every time. They were the collision.

## What is left, with evidence

Six more concurrent pairs were run after the fix, on a box whose load average
sat between 66 and 143 because four other worktrees were testing at the same
time. **Not one collision signature appeared** — zero occurrences of `table repo
already exists`, `disk I/O error` or `no such table` across all six, against
nine across the two runs before the fix. Six failures remain, in two tests:

| Test | Runs failed | Symptom |
|---|---|---|
| `ksp::tests::assetful_companion_stream_skips_retained_assetless_snapshot_during_upgrade` | 5/6 | `Elapsed(())` on a 10s budget |
| `ksp::tests::retained_admission_rejection_does_not_rematerialize_unchanged_multi_source_bundles` | 1/6 | same shape |

Earlier rounds, before the write-ordering fix below, also surfaced:

| Test | Symptom |
|---|---|
| `http_api::tests::input::merge_handoff_on_close::a_promised_handoff_with_no_pr_refuses_to_close_the_task` | `activity` read as `idle`, expected `unread` |
| `ksp::tests::companion_events_acknowledge_append_validation_and_connection_rate_limit` | `assert!(!accepted)` — a time-windowed rate limit |
| `http_api::tests::revision_status::review_run_binding_refuses_cross_task_pair_and_allows_concurrent_verdicts` | `git`: `failed to read .git/worktrees/task-budget-2-2/commondir` |

**`merge_handoff_on_close` was a real readiness gap and is fixed.** The refusal
path in `http_api/signal_agent.rs` appended `task.merge_handoff_missing` and
*then* set the task unread. The test waits for that event and reads `activity`,
so under load it read between the two writes. The event is supposed to mean
"this task is parked for its human", so it must not be readable before the task
is parked: the writes are now ordered unread-then-event, which also makes the
half-completed state the safer one. Ordinary invariant from AGENTS.md — an event
is appended by the write that changes the state it describes.

The rest are **not fixed, and deliberately not papered over**:

- The `ksp` companion tests have no readiness signal left to wait on: the frame
  they await *is* the signal, and the rate-limit test awaits the decision
  itself. Their 10s budget is an anti-hang guard sized for an idle machine, and
  it is spent scanning a full asset bundle off disk. Choosing a new number is a
  judgement about how loaded this box is allowed to get, not a correctness fix,
  so it is left for its owner.
- `review_run_binding_…` fails inside `git worktree`, reading a `commondir` for
  a *sibling* fixture worktree (`task-budget-2-2` while preparing
  `task-budget-1-2`). That is a fixture-repo lifecycle question — one test's
  cleanup racing another's `git` — not a temp-path name, and it wants its own
  look.

## Still leaking, for somebody else

Only `kanna-server`'s paths are collected. After the six runs above, exactly one
`kanna-test-*` root remained in the temp directory — the sweep took every
finished run's tree. But 26,888 kanna test entries from other naming schemes are
still there, most of them from crates whose helpers pre-date this and name no
collectable root. They are correct (every one carries a pid) and they are not a
collision risk; they simply accumulate. If this machine fills again, that is
where to look.

## Load really is load, in other crates

Five `./kd test all` runs were taken on this box while four other worktrees
tested alongside it, at load averages between 30 and 154. Every one of the 18
TypeScript/JavaScript turbo tasks passed in the runs that reached them. The
rust and kd lanes produced a different single failure each time, always a
latency or resource assertion, always in code this task did not touch, and
**every one of them passed on re-run alone**:

| Test | Verified alone |
|---|---|
| `kanna-cli tests::http_api::notify_mobile_surfaces_only_the_fixed_server_rejection_error` | 5/5, and its whole binary 106/106 |
| `kanna-daemon reconnect::stalled_observer_does_not_delay_healthy_subscriber_or_pty_ingestion` | 5/5 |
| `kanna-server task_creator::lifecycle::teardown_deadline_tests::transient_soft_probe_failure_preserves_teardown_hard_deadline` | 3/3 (a 20ms/50ms deadline) |
| `kanna-server http_api::tests::task_events::aggregate_repo_wait_forwards_exclusions_to_every_machine_leg` | 3/3 |
| `kanna-server workspace_commands::tests::dropping_spawned_command_guard_kills_and_reaps_its_process_group` | 3/3 |
| `kd tests/cli.test.ts > bounds resolver startup and reports a clear timeout` | 3/3 (it asserts a launch under 30s; it took 53s under load) |
| `kd tests/process-inventory.test.ts` | fails only in the runs that started with the disk nearly full |

So the answer to "which of these were really the collision" is: the ones named
in the task were, and none of these are. This second list is the price of
running many gates on one machine, and it is a different problem — one about
absolute time budgets in tests that share a CPU, not about two runs sharing a
file.

`.build/` is the other half of the same pressure: eight worktrees held 14–27 GB
each, over 130 GB, which is why the disk hit 99% twice during this task.
