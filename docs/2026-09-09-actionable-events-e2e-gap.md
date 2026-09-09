# Actionable subscription delivery verification

The owner has held all Rust compilation/check/test/clippy, `./kd test all`,
dev servers and heavy verification until `RESUME HEAVY VERIFICATION ACTIONABLE-EVENTS`.
No release, restart, live subscription manipulation or load generation is authorized.
The implementation is therefore a draft, not a verified delivery claim.

The new `http_api/tests/task_events/subscription_relevance.rs` fixtures exercise
durable producers → native waits → filtered batching/cursors and initial
subscription mailbox/acknowledgement, including both adapter selections. A
fixture peer ignores the optional selection parameter to exercise old-server
responses through the aggregate relay request path. A detached transition test
in `task_actions.rs` covers daemon connection failure → durable lifecycle failure
→ actionable wait. Existing subscription tests retain fault, restart, uncertain
wake and acknowledgement coverage. These Rust tests have not run under the hold.

These fixtures do not prove a real installed Codex app-server turn or PTY input
wake across independently running server/relay processes. After capacity release,
run the focused Rust fixtures, existing task-event/subscription/adapter tests and
catalog contract tests, then required clippy and repository lanes. Verify both
adapters against isolated harnesses with a quiet automatic review → PR transition,
a confirmed question, failure, manual gate and an observation fault. Confirm the
mailbox carries only relevant rows and repeated reads do not submit duplicate
wakes. Stop all owned processes. No shared production/staging probes are needed.

The permit-lifetime fix in task `2c7a34b9` remains an explicit predecessor and
must merge first. Its committed checkpoint
`667476f4d8c0969a70a357b91864b23b4dc5b2e0` was integrated locally after draft
checkpoint `e47e038d6`, with no conflicts. The pinned `event_subscriptions.rs::step`
and `subscription_remote.rs` boundary suite are preserved; this task's `collect`
change and separate `subscription_relevance` module remain alongside them.
No push, PR or handoff is permitted until the predecessor's review/gate clears.

Filtered native-page draining now checks the existing wait deadline before
another read, including after its scheduling yield. Zero-time bootstrap draining
uses the existing aggregate zero-time drain budget. A timeout retains the consumed
checkpoint and reports remaining raw pages without counting them toward actionable
batch thresholds. Aggregate re-arming likewise cannot continue a positive wait
past its deadline. Paused-clock regressions cover both timeout modes and resuming
through the excluded backlog to the next actionable event; they remain unrun.

Other shared surfaces are limited to the subscription/wait catalog descriptions,
subscription section of the server boundary document and manager event-loop
instructions. Preserve independent brief-detail (`9fe6ba82`), machine-resource
(`29696721`) and conversation-queue (`87fe345e`) changes. In particular, preserve
`cc412ba26`'s provider/model/effort section and creation examples when it lands;
this task does not import that independent instruction change.

The separately observed false `awaiting_input` from a quoted fixture belongs to
terminal prompt detection. This task deliberately retains confirmed question
events and makes no speech/transcript classification changes.
