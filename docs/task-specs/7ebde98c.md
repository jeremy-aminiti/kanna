# Stage-advance pending-state verification

The desktop stage projection changes rendered UI in `TaskHeader.vue` and
`Sidebar.vue`, but real-app verification is currently blocked by the task's
explicit `HOLD` on native/dev automation until the owner sends the exact
`RESUME STAGE-ADVANCE VERIFICATION` directive. No live UI or E2E automation was
run for that reason.

Once released, start this worktree only through `./kd dev up` and interact only
with the native dev window whose title has the task-specific identity
`Kanna — task 7ebde98c · task-7ebde98c-3 (0.0.68 @ <built commit>)`; the built
commit suffix must match that run's checked-out revision. Do not use a generic
production or staging window.

Capture and inspect both states in the real desktop app:

- Pending `plan` → `in progress`: the selected task header reads
  `plan → in progress…` in the warning treatment, its hover tooltip reads
  `Stage change pending`, and the matching sidebar title has the `...` prefix.
- Settled `in progress`: the header returns to the ordinary `in progress`
  badge with no pending tooltip or warning treatment, and the sidebar prefix
  disappears without selection changing.
- Repeat the pending and settled checks in light and dark themes with macOS
  Increase Contrast enabled, and inspect the accessibility tree/VoiceOver
  output to confirm the text and tooltip communicate pending state without
  relying on warning color alone. Reduce Motion is not applicable because this
  change adds no animation, gesture, or dynamic layout.

Until the hold is lifted, the focused `TaskHeader` and `Sidebar` component
tests plus the workflow/query store tests are the interim coverage. No E2E
automation is required during the hold because the task explicitly prohibits
native/dev UI manipulation.
