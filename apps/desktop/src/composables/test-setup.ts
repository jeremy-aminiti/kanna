// Test preload: set up happy-dom globals for composable tests that need DOM APIs.
import { Window } from "happy-dom";

const win = new Window();
// @ts-ignore
globalThis.document = win.document;
// @ts-ignore
globalThis.window = win;
// @ts-ignore
globalThis.localStorage = win.localStorage;
// @ts-ignore — use happy-dom's Event so dispatchEvent instanceof check passes
globalThis.Event = win.Event;

// This suite's keyboard expectations are macOS's: ⌘ bindings and ⌘ glyphs, the
// platform the app shipped on first. Say so rather than inheriting whatever
// happy-dom reports, which would silently switch every shortcut assertion to
// the Linux mapping. That mapping has its own explicit coverage in
// `shortcutPlatform.test.ts`, which asks for each platform by name.
for (const nav of [globalThis.navigator, win.navigator]) {
  if (nav) Object.defineProperty(nav, "platform", { value: "MacIntel", configurable: true });
}

import {
  setDesktopServerClientHandlersForTests,
  setDesktopSnapshotFetcherForTests,
} from "../services/desktopServerClient";

setDesktopSnapshotFetcherForTests(async () => ({
  entries: [],
  taskBlockers: [],
  worktreePaths: {},
  settings: {},
}));

setDesktopServerClientHandlersForTests({
  getSetting: async () => null,
  putSetting: async (key, value) => ({ key, value }),
  deleteSetting: async () => {},
  postOperatorEvents: async () => {},
  fetchRepoAnalytics: async () => ({
    taskBuckets: [],
    bucketSize: "daily",
    hasData: false,
    avgTimeInState: {
      working: 0,
      idle: 0,
      unread: 0,
    },
    operatorMetrics: {
      avgResponseTime: null,
      avgDwellTime: null,
      switchesPerHour: null,
      focusScore: null,
    },
    hasOperatorData: false,
  }),
  fetchRepoAgentProviders: async () => ["claude", "copilot", "codex", "opencode", "antigravity"],
  patchRepo: async () => {},
  fetchClosedTaskIdentities: async () => [],
});
