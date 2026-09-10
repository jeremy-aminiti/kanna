# Provider quota rejection and bounded recovery

## The incident

`.kanna/workflows/plan-build-review.json` declares
`"agent_provider": ["claude-fable", "codex-gpt-6-astra"]` on its `plan` and `review`
stages. AGENTS.md documents an ordered candidate list as an outage-fallback
chain: "an ordered list like `["claude-fable-hi", "codex-gpt-6-astra-lo"]` gives every
fallback candidate its own coherent model/effort".

On 2026-09-08 the account's Fable allowance was exhausted. Task `6b4a48af`
advanced to `review`, the stage spawned on the leading candidate, and the
session parked on:

```
  ⎿ You've reached your Fable limit. Run /usage-credits to continue or switch models with /model.
```

Nothing classified that. To every surface it looked like an ordinary dead
session. `kanna_rerun_stage` re-spawned it on Fable — correctly, per the
documented rule that a rerun feeds the recorded run's provider back in as an
*explicit* override — and it parked again. The manager ran the review out of
band and swapped the workflow by hand.

Provider *availability* was already a fallback chain, but availability is
resolved by "is the executable on `PATH`", and an exhausted allowance is a
perfectly installed CLI. The missing half is that a refusal is a **runtime**
event, so recovering from it is a runtime contract.

Advisory consultation `796352ef` scoped this increment; its verdict is the
source of the invariants below.

## What a rejection is

**A positive, provider-stated observation, never an inference from silence.**
The same standard `task.awaiting_input` is held to: a refusal is matched
against chrome the provider actually drew, at a CLI version this repository has
measured, or against the headless SDK's own structured status. A quiet session
is never a refusal, and neither is a session that died.

**A refusal is not a status.** The CLI prints its refusal and parks at its
composer — a perfectly healthy `idle` session. Folding the fact into
`SessionStatus` would mean either inventing a runtime state for it or reporting
a live agent as dead. So it travels as its own channel: a `notice`, carried
beside the status verdict and never instead of one.

**A rejection establishes only the scope the provider stated.** Claude names
the model (`Fable`); Codex names only the account. `scope: null` means *the CLI
did not say* — never "this provider is unavailable".

## Classification

### PTY

`crates/daemon/src/detection/rules.json` gains a per-provider `notices` list
beside `rules`. A notice rule has an `id`, a `kind` (`quota-rejection`),
`versions`, a `priority`, a grid `when` predicate, and an optional `scope`
extractor (`{"between": ["reached your ", " limit"]}`) that reads the stated
scope out of the matched text. It reuses the existing matcher and version
machinery; it adds no new matcher vocabulary.

Two differences from a status rule, both deliberate:

- **A version-bounded notice does not apply to an unmeasured CLI version.**
  `VersionRange::admits(None)` is permissive by design for status rules — a
  verdict from an unmeasured CLI beats no verdict at all. A rejection is not a
  verdict about the screen but a claim that drives automatic provider recovery,
  so it gets the opposite default.
- **Notices read their own window** (`noticeRows`, default the status window).
  A refusal is printed into the transcript and the CLI keeps drawing chrome
  beneath it, so the sentence sits further from the bottom of the screen than
  any status row.

The scan runs on the boundary where the classifier already read a settled frame
— one extra pass per throttle window, not one per output chunk — and is latched
per session incarnation, cleared when the session goes busy again. A refusal
stays painted for as long as the session is parked in front of it; one refusal
is one observation.

The daemon broadcasts `Event::ProviderNotice { session_id, kind, session_kind,
agent_provider, rule_id, scope, text, cli_version }`. Broadcast only, like
`InputBlockedChanged`: this is `kanna-server`'s signal, not a terminal client's
— a terminal client is already looking at the sentence.

### Headless (SDK)

The Claude CLI emits `rate_limit_event` whenever a usage window moves; the
checked-in fixture carries `status: "allowed"` mid-conversation. The whole
payload used to be discarded into a bare `"rate limit event"` diagnostic, which
is a large part of why the exhaustion was invisible.

`RateLimitMessage` now carries typed `rate_limit_info` (`status`, `resetsAt`,
`rateLimitType`, `model`, `overageStatus`, `isUsingOverage`), and the adapter
distinguishes the states: `allowed` and warning statuses stay diagnostics with
real detail, only `status == "rejected"` becomes
`AgentEvent::QuotaRejected { scope, resets_at, detail }`. The daemon's agent
runtime publishes the same `Event::ProviderNotice` for it, latched per
incarnation, so the server has one path to act on.

### Measured chrome

The version-tagged captures live in
`tests/cli-contract/fixtures/provider-quota-rejection.json` — the repository's
home for provider CLI evidence — and are compiled into the daemon's own
detection tests, so a pattern and the frame it was measured against cannot
drift apart in separate commits. Each capture names the CLI version and where
it came from, carries the narrow-terminal wrap (Codex breaks its own sentence
mid-clause at 80 columns), and the file also keeps negatives that must never
classify — including the Codex banner that announces *available* resets and
means the opposite of a refusal.

The provenance is stated exactly, including where it is weaker: the Claude
capture is from a 2.1.26x session on 2026-09-08 and the rule is scoped
`>=2.1.265`; the Codex capture is from 2026-08-18 and the rule is scoped
`>=0.153` to the installed CLI, whose wording was **not** re-observed —
re-observing it costs a real exhausted account. Both scopes are narrow enough
that an unmeasured release classifies nothing; widening one means capturing it
first.

## Recovery

Everything below is decided once, per refusal, under the same single-flight
task-mutation guard a close, a rerun or a stage change takes.

**The refusal is recorded before anything is decided.** `task_provider_rejection`
holds one row per `(stage_run, provider, stated scope)`. That uniqueness is the
whole de-duplication story: a replayed or re-adopted announcement lands on the
row that already exists and can never become a second attempt.

**Preconditions.** A refusal is acted on only against an open task's own
still-running attempt whose recorded provider is the one that refused.
Anything else describes a run that no longer exists.

**The decision, in order:**

| Condition | Verdict |
|---|---|
| The run carries an explicit single-provider override | `parked-override-binding` |
| The stage names no ordered candidate list | `parked-no-candidate-list` |
| Every candidate has already refused at this stage | `parked-no-candidates` |
| The workspace holds any uncommitted change | `parked-work-observed` |
| Otherwise | `fallback-started` |
| (the chosen candidate could not be started) | `parked-fallback-failed` |
| (the task was already being mutated) | `parked-concurrent-mutation` |

**Scope of the walk.** Recovery walks the candidate list of the stage the task
*occupies*. A refusal inside a stage's **post** is recorded like any other, but
parks as `parked-no-candidate-list` rather than falling back: a post continues
the stage's running session and its own binding is not the list the task's
stage names. Falling a post over to another provider is a larger question than
this increment answers.

**The fallback.** The next candidate runs on the same task, the same stage, the
same workspace and the same session id, with **that candidate's own model and
effort** from its own compact selector — never the refused candidate's, because
`codex -m fable` is rejected by the Codex CLI outright and composing one
candidate's model onto another provider is the cross-layer mistake provider
resolution exists to prevent. Each remaining candidate is tried at most once.
Resolution is re-checked before the spawn: if a higher-precedence layer walks
back to the refused provider, the task parks rather than repeat the refusal.

**Nothing here ever produces a success.** The refused run is closed as `failed`
with the provider's own sentence as its result *before* the replacement is
spawned — never left running for the spawn path to finish as `succeeded` on its
way past. No stage advances, and no completion is fabricated.

**The workspace is never touched.** No reset, no fork, no task recreation. A
committed change survives either way; what the dirty-worktree park guards is
asking a second agent to redo something a first one may have half-finished.

The limit of that test, stated plainly: a refused attempt that committed
everything and left a clean tree reads as untouched, and the next candidate
starts on the same prompt in a workspace already holding that commit. That is
the same position an ordinary `rerun_stage` puts a task in, and the second
agent sees the commit rather than losing it. A stronger test needs a per-run
workspace baseline recorded at spawn; that is a schema change, and it is not
what this increment buys.

**Nobody chose the fallback provider**, so the replacement run records no
explicit `provider_override`. Stamping one would make the next rerun read an
outage detour as a decision.

### The parked state

One state, one event, no retry loop. `task.provider_quota_parked` carries the
reason, the providers refused at this stage, and an `action` sentence saying
what a person can do. The task is marked `unread` — it is waiting, which is
what unread already means; nothing new is invented for quota.

## Rerun and resume

The recorded provider is fed back in as an explicit override on a rerun, which
is what re-spawned `6b4a48af` onto an exhausted allowance twice.

**A rerun and a resume are deliberate requests, and a past refusal never
refuses one.** This is the line between them and the automatic fallback: the
fallback is bounded because an unbounded automatic retry is a spin, while a
person asking for this stage again — with `providerRejection` on task detail in
front of them — is a decision. Waiting for an allowance to reset and rerunning
is the documented recovery, so it has to work.

- **`rerun_stage`** prefers a candidate the stage names that has not been
  refused here. With none left un-refused — and with a stage that names no
  candidates at all, which is every built-in workflow but `plan-build-review` —
  it proceeds on the recorded provider. A run started from an explicit
  single-provider override is excluded from the walk entirely — the rerun
  reproduces that override, because it is a caller's decision and only a
  workflow replacement supersedes it. A rerun that walked around a refusal
  records no `provider_override`.
- **`resume`** reopens the recorded provider's *own* conversation and cannot
  walk anywhere; re-pointing it would be a fresh session wearing a resume's
  name. A refusal does not block it, because reopening that conversation once
  the allowance has reset is exactly what the `parked-work-observed` action
  tells the operator to do.

**The gate is keyed to the refused `stage_run`, never to the stage name.** The
stage-name form — "any provider ever refused at this stage" — has no time bound
and no link to the run being rerun, so a refusal under it disables rerun and
resume for the rest of the task's life at that stage. Keyed to the run, it
stops applying the moment the operator acts, because a rerun produces a new
run.

To move a parked task onto a different provider before its allowance resets,
re-point the stage with `kanna_replace_task_workflow` and rerun: a workflow
replacement supersedes the recorded run's execution binding, including an
explicit provider override. `kanna_rerun_stage` itself takes no provider
argument, so no operator-facing message may suggest passing one.

This does **not** disturb the `agentProviders` / `config.local.json` /
frontmatter precedence chain: recovery walks the stage's *own* pinned candidate
list and enters through the same explicit-override slot a rerun already uses.
Re-pointing a stage's candidates is
[runtime workflow replacement](./dynamic-task-workflows.md)'s job, not this
one's.

## Surfaces

- `task.provider_quota_rejected` — every refusal, with provider, model, scope,
  stage, stage run, source (`pty`/`sdk`), rule id, matched text, recovery and
  replacement run.
- `task.provider_quota_parked` — the actionable state only.
- `kanna_get_task` → `providerRejection` — the latest refusal at the stage the
  task currently occupies, including `rejectedProviders`. A quota-refused
  session parks at its composer looking exactly like a healthy idle one, so
  neither `activity` nor `runtimeState` can answer this; the field is why a
  reader does not have to conclude the run simply failed.

## Deliberately not here

Usage/quota dashboards, account switching, and quota-aware scheduling are the
consultation's *second* increment and are out of scope. Nothing here scrapes
arbitrary transcript text, reads a percentage, or predicts a reset: a passed
reset time permits reconsidering, it does not prove replenishment.

## Coverage

- `crates/daemon/tests/provider_quota_notice.rs` — real daemon, real PTY,
  scripted provider printing the measured chrome: the refusal is announced
  once, names the stated scope, the session stays idle and alive, an unprobed
  CLI announces nothing, and prose about limits announces nothing.
- `crates/daemon/src/detection/rules.rs` (`quota_notice_tests`) — the captures
  against the real classifier, including the narrow-terminal wrap, the
  cross-provider negative, and the unmeasured-version refusal.
- `crates/kanna-agent-protocol/tests/claude_adapter.rs` (`rate_limit`) —
  allowed, warning, rejected, sparse, missing and unknown payloads.
- `crates/kanna-server/src/task_creator/tests/quota_recovery.rs` — the
  regression through the real watcher, recovery, preparation and spawn against
  a fake daemon: one fallback with its own model/effort in the same workspace,
  a second refusal parking once, workspace preservation, the binding override,
  the replayed refusal, and the rerun/resume refusals.
