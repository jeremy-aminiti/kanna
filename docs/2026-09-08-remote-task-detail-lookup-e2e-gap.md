# Remote task detail lookups have no desktop E2E

`MainPanel.vue` used to request `/v1/tasks/{id}` for whichever task was
selected, including a task owned by another machine — which the cloud index
presents under a `cloud:<cloud task id>` id no `kanna-server` anywhere owns.
Every lookup 404'd, and the local server answered its own miss by forwarding
the id over the relay to every reachable peer, so the noise landed in the
*other* machine's log too. On one staging machine a single selected remote task
produced 82,047 lookups and 7,621 forwarded 404s per 200 MB of the peer's log.

The fix (skip the fetch when the panel is showing a remote task) is covered by
a component test in `apps/desktop/src/components/__tests__/MainPanel.test.ts`:
it drives the same `updated_at` / `activity_revision` churn the cloud index
produces and asserts the detail fetch is never called, then asserts a task that
transfers in picks its detail back up.

## Why there is no E2E yet

The real boundary is "the webview does not issue this HTTP request to the local
server", and the mock E2E harness
(`apps/desktop/tests/e2e/helpers/`) has no way to observe the requests the
webview makes. It drives the app through WebDriver and reads state through
`tauriInvoke` and `execDb`; nothing records or asserts on server request
traffic, and the server-side record of the lookups — the 404 lines in
`kanna-server.log` — is not exposed to the harness either.

## What would make it testable

Either of:

- a harness helper that reads the app's own console forwarding
  (`/tmp/kanna-webview-*.log`, written via the `append_log` Tauri command) for
  the instance under test, so a test can assert the absence of
  `[main-panel] failed to load task detail for cloud:`; or
- a request counter on the test server that a test can read over the local API,
  scoped to a path prefix, so any test can assert "the app did not ask for
  this".

`apps/desktop/tests/e2e/mock/remote-terminal-focus.test.ts` already selects a
remote task and would be the natural place to add the assertion once either
exists.
