## Verification Proportional to the Change

Owner feedback (2026-09-10): small terminology and MCP-output changes took
hours through repeated verification and review. Choose checks from the actual
changed behavior and failure modes; do not run `./kd test all` automatically
for every review or revision.

For terminology, documentation, and bounded presentation changes, review the
diff and run the relevant definition, compatibility, or component contracts.
A label change does not by itself require a desktop/mobile appearance matrix
or justify adjacent layout or accessibility behavior changes. For bounded
API output changes, exercise the real affected routes and consumers, including
unknown/error and compatibility cases; unrelated native UI gates add no proof.

Reuse recorded verification when its command, result, and reviewed head are
known and the relevant code is unchanged. Check patch equivalence after a
rebase. A fresh stage worktree alone is not a reason to repeat a full build.
Independent review means independently assessing the code and evidence; it
does not require duplicating every author's test run.

Run `./kd test all` for broad changes or changes whose impact cannot be bounded
by focused checks, and when explicitly required for a release. Keep meaningful
integration tests for changed process, persistence, and protocol boundaries.
After a revision, verify the correction and affected contracts; repeat broader
checks only when the new diff, a failure, or an unresolved risk justifies them.

Request revisions for concrete defects caused by the task. Keep unrelated
failures and improvements as follow-ups. Record actual exits and skipped or
cancelled checks honestly; an accepted review with a qualified gate failure
must never be reported as a full gate pass.
