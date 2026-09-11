## Desktop UI Targeting

Owner direction (2026-09-10): dev windows carry the task id in their native
title. Require that exact id plus worktree/build identity before any agent UI
interaction or visual evidence, and recheck after target/session changes.
Generic `Kanna` / `build.kanna` lookups can launch production before inspection;
forbid them for test selection. Use only an explicitly identified running
worktree window or canonical isolated WebDriver endpoint. Missing/mismatched
identity stops that UI path; it does not authorize an installed-app fallback.
Carry this rule into active implementation/review directives immediately;
future agents inherit the canonical rule in `AGENTS.md` once merged. On an
incident, stop owned automation, preserve actual actions/identity evidence,
and leave operator processes untouched. Installed production/staging testing
requires a separate explicit human request naming the environment.

## Kanna Desktop Release Policy

For this repository, never run `./kd release ship` directly in the manager session. Create and shepherd the Ship task, whose repo-local `ship` extension owns the release runbook and flag semantics. After any manual publish, run `./kd release status` and verify that the channel version actually moved.
