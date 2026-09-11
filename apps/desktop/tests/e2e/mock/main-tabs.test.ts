import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { localProcessFetch } from "@kanna/local-process-fetch";
import { buildGlobalKeydownScript, buildSelectorKeydownScript } from "../helpers/keyboard";
import { WebDriverClient } from "../helpers/webdriver";
import { cleanupFixtureRepos, createSeedFixtureRepo } from "../helpers/fixture-repo";
import { resolveAppKannaServer } from "../helpers/kannaServer";
import { cleanupWorktrees, importTestRepo, resetDatabase } from "../helpers/reset";
import { callVueMethod, getVueState, tauriInvoke } from "../helpers/vue";

/**
 * The main content area hosts a task's views as tabs: the agent session plus
 * whichever of the diff, a file, and the task shell the operator (or an agent
 * through `kanna_open_view`) has opened. These are the boundary-crossing parts
 * that unit tests cannot prove — the real keyboard path, the real xterm buffer
 * surviving a tab switch, and a server route reaching a live window.
 */
async function openTabIds(client: WebDriverClient): Promise<string[]> {
  return await client.executeSync<string[]>(
    `return Array.from(document.querySelectorAll('[data-testid="main-tab-bar"] [role="tab"]'))
      .map((tab) => (tab.getAttribute("data-testid") || "").replace(/^main-tab-/, ""));`
  );
}

async function activeTabId(client: WebDriverClient): Promise<string | null> {
  return await client.executeSync<string | null>(
    `const active = document.querySelector('[data-testid="main-tab-bar"] [role="tab"][aria-selected="true"]');
     return active ? (active.getAttribute("data-testid") || "").replace(/^main-tab-/, "") : null;`
  );
}

async function waitForActiveTab(
  client: WebDriverClient,
  id: string,
  timeoutMs = 8000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let latest: string | null = null;
  while (Date.now() < deadline) {
    latest = await activeTabId(client);
    if (latest === id) return;
    await sleep(150);
  }
  throw new Error(`expected active tab ${id}, got ${latest} of ${JSON.stringify(await openTabIds(client))}`);
}

/**
 * The app has renamed its selection entry point before, and an e2e helper that
 * silently missed it selects nothing while every later assertion still looks
 * plausible. Resolve it by trying each spelling.
 */
const SELECT_SIDEBAR_ITEM_SCRIPT = `
  function selectSidebarItem(ctx, id) {
    const select = ctx.selectSidebarItemById || ctx.handleSelectItem
      || (ctx.store && ctx.store.selectItem && ctx.store.selectItem.bind(ctx.store));
    if (!select) throw new Error("no sidebar selection entry point on setupState");
    return select(id);
  }
`;

async function pressShortcut(
  client: WebDriverClient,
  options: { key: string; meta?: boolean; shift?: boolean; alt?: boolean },
): Promise<void> {
  await client.executeSync(buildGlobalKeydownScript(options));
}

/**
 * ⌘W itself belongs to the native File menu in the packaged app, which
 * dispatches to this action — so a test presses it the way the menu does
 * rather than synthesising a keydown the web handler deliberately ignores.
 */
async function pressCloseTab(client: WebDriverClient): Promise<void> {
  const result = await callVueMethod(client, "keyboardActions.closeTabOrWindow");
  if (result && typeof result === "object" && "__error" in result) {
    throw new Error(String((result as { __error: string }).__error));
  }
  await sleep(200);
}

/** Tabs persist per task, so each test starts from the agent session alone. */
async function closeViewTabs(client: WebDriverClient): Promise<void> {
  for (let attempt = 0; attempt < 8; attempt += 1) {
    const closed = await client.executeSync<boolean>(
      `const close = document.querySelector('[data-testid^="main-tab-close-"]');
       if (!close) return false;
       close.click();
       return true;`
    );
    if (!closed) return;
    await sleep(120);
  }
  throw new Error(`tabs would not close: ${JSON.stringify(await openTabIds(client))}`);
}


/**
 * Arm a one-shot record of the sidebar task search taking focus. The global
 * ⌘F focuses it synchronously while the view focuses its own input a tick
 * later, so the end state hides the second binding having fired at all.
 */
async function watchSidebarSearchFocus(client: WebDriverClient): Promise<void> {
  await client.executeSync(
    `window.__sidebarSearchFocused = false;
     const input = document.querySelector(".sidebar .search-input");
     if (!input) throw new Error("sidebar search input is missing");
     input.addEventListener("focus", function () { window.__sidebarSearchFocused = true; }, { once: true });
     return true;`
  );
}

async function sidebarSearchWasFocused(client: WebDriverClient): Promise<boolean> {
  return client.executeSync<boolean>(`return window.__sidebarSearchFocused === true;`);
}

/**
 * Where the focused element lives, which is how these tests tell the view's own
 * find from the sidebar's task search: both render `.search-input`.
 */
async function focusedSearchOwner(client: WebDriverClient): Promise<string> {
  return client.executeSync<string>(
    `const active = document.activeElement;
     if (!active || !active.classList.contains("search-input")) return "none";
     return active.closest(".sidebar") ? "sidebar" : "view";`
  );
}

describe("main content area tabs", () => {
  const client = new WebDriverClient();
  let fixtureRepoRoot = "";
  let testRepoPath = "";
  let taskId = "";
  let secondTaskId = "";

  async function createTask(prompt: string): Promise<string> {
    const repoId = await getVueState(client, "selectedRepoId") as string;
    const id = crypto.randomUUID();
    const branch = `task-${id}`;
    const worktreePath = `${testRepoPath}/.kanna-worktrees/${branch}`;

    await tauriInvoke(client, "git_worktree_add", {
      repoPath: testRepoPath,
      branch,
      path: worktreePath,
    });
    await tauriInvoke(client, "run_script", {
      script: "printf '\\n# main tabs e2e\\n' >> README.md",
      cwd: worktreePath,
      env: {},
    });

    const created = await client.executeAsync<string>(
      `const cb = arguments[arguments.length - 1];
       const ctx = window.__KANNA_E2E__.setupState;
       ${SELECT_SIDEBAR_ITEM_SCRIPT}
       const db = ctx.db.value || ctx.db;
       db.execute("INSERT INTO pipeline_item (id, repo_id, prompt, stage, branch, agent_type) VALUES (?, ?, ?, ?, ?, ?)",
         ["${id}", "${repoId}", "${prompt}", "in progress", "${branch}", "agent"])
         // kanna_open_view resolves the file through the task's recorded
         // workspace, so the row has to exist as well as the directory.
         .then(function() {
           return db.execute("INSERT INTO worktree (id, pipeline_item_id, path, branch) VALUES (?, ?, ?, ?)",
             ["wt-${id}", "${id}", "${worktreePath}", "${branch}"]);
         })
         .then(function() { return ctx.loadItems("${repoId}"); })
         .then(function() { selectSidebarItem(ctx, "${id}"); return ctx.refreshAllItems ? ctx.refreshAllItems() : null; })
         .then(function() { cb("ok"); })
         .catch(function(e) { cb("err:" + (e && e.message ? e.message : String(e))); });`
    );
    if (typeof created === "string" && created.startsWith("err:")) {
      throw new Error(`creating task ${prompt} failed: ${created.slice(4)}`);
    }
    await client.waitForText(".sidebar", prompt);
    return id;
  }

  async function selectTask(id: string): Promise<void> {
    const result = await client.executeAsync<string>(
      `const cb = arguments[arguments.length - 1];
       try {
         const ctx = window.__KANNA_E2E__.setupState;
         ${SELECT_SIDEBAR_ITEM_SCRIPT}
         selectSidebarItem(ctx, "${id}");
         setTimeout(function() { cb("ok"); }, 100);
       } catch (e) {
         cb("err:" + (e && e.message ? e.message : String(e)));
       }`
    );
    if (typeof result === "string" && result.startsWith("err:")) {
      throw new Error(`selecting task ${id} failed: ${result.slice(4)}`);
    }
  }

  beforeAll(async () => {
    await client.createSession();
    await resetDatabase(client);

    fixtureRepoRoot = await createSeedFixtureRepo("task-switch-minimal");
    testRepoPath = fixtureRepoRoot;
    await importTestRepo(client, testRepoPath, "main-tabs-test");

    taskId = await createTask("Main tabs task");
    secondTaskId = await createTask("Main tabs other task");
    await selectTask(taskId);
  });

  afterAll(async () => {
    if (testRepoPath) {
      await cleanupWorktrees(client, testRepoPath);
    }
    await cleanupFixtureRepos(fixtureRepoRoot ? [fixtureRepoRoot] : []);
    await client.deleteSession();
  });

  it("opens contextual help for the active mounted tab and preserves dialog precedence", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);
    const ctx = "window.__KANNA_E2E__.setupState";
    async function expectHelp(title: string, command: string) {
      await pressShortcut(client, { key: "/", meta: true });
      await client.waitForText(".shortcuts-modal h3", title);
      const actions = await client.executeSync<string[]>(
        `return Array.from(document.querySelectorAll('.shortcuts-modal .shortcut-action'))
          .map(element => element.textContent.trim());`,
      );
      expect(actions).toContain(command);
      await pressShortcut(client, { key: "Escape" });
      await client.waitForNoElement(".shortcuts-modal");
    }
    async function activate(id: string) {
      await client.executeSync(
        `document.querySelector('[data-testid="main-tab-${id}"]').click();`,
      );
      await waitForActiveTab(client, id);
    }
    try {
      await pressShortcut(client, { key: "E", meta: true, shift: true });
      await waitForActiveTab(client, "tree");
      await expectHelp("Tree Explorer Shortcuts", "Enter dir / Open file");
      await callVueMethod(client, "openFilePreview", "README.md");
      await waitForActiveTab(client, "file:README.md");
      await expectHelp("File Viewer Shortcuts", "Search (alt)");
      await pressShortcut(client, { key: "d", meta: true });
      await waitForActiveTab(client, "diff");
      await expectHelp("Diff Viewer Shortcuts", "Search (alt)");

      // These components stay mounted: their mount order must not select help.
      await activate("tree");
      await expectHelp("Tree Explorer Shortcuts", "Enter dir / Open file");
      await activate("file:README.md");
      await expectHelp("File Viewer Shortcuts", "Search (alt)");
      await activate("agent");
      await expectHelp("Keyboard Shortcuts", "New Task");
      expect(await getVueState(client, "shortcutsContext")).toBe("main");

      await activate("tree");
      await pressShortcut(client, { key: "N", meta: true, shift: true });
      await expect.poll(() => getVueState(client, "showNewTaskModal")).toBe(true);
      await pressShortcut(client, { key: "/", meta: true, shift: true });
      await client.waitForText(".shortcuts-modal h3", "Keyboard Shortcuts");
      // Help itself reports Main for routing; the captured dialog still wins.
      await pressShortcut(client, { key: "/", meta: true });
      await client.waitForText(".shortcuts-modal h3", "New Task Shortcuts");
      await pressShortcut(client, { key: "Escape" });
      await client.waitForNoElement(".shortcuts-modal");
      await pressShortcut(client, { key: "Escape" });
      await expectHelp("Tree Explorer Shortcuts", "Enter dir / Open file");

      await activate("file:README.md");
      await pressCloseTab(client);
      await waitForActiveTab(client, "diff");
      await expectHelp("Diff Viewer Shortcuts", "Search (alt)");
      await pressCloseTab(client);
      await waitForActiveTab(client, "tree");
      await expectHelp("Tree Explorer Shortcuts", "Enter dir / Open file");
      await pressCloseTab(client);
      await waitForActiveTab(client, "agent");
      await expectHelp("Keyboard Shortcuts", "New Task");
      expect(await getVueState(client, "shortcutsContext")).toBe("main");
    } finally {
      await client.executeSync(`${ctx}.showShortcutsModal = false; ${ctx}.showNewTaskModal = false;`);
      await closeViewTabs(client);
    }
  });

  it("opens the diff and the task shell as tabs beside the agent session", async () => {
    await client.waitForElement('[data-testid="main-tab-bar"]', 5_000);
    expect(await openTabIds(client)).toEqual(["agent"]);

    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");
    await client.waitForElement(".diff-view", 8_000);
    // The agent session is still mounted behind it, not torn down.
    expect(await openTabIds(client)).toEqual(["agent", "diff"]);

    await pressShortcut(client, { key: "j", meta: true });
    await waitForActiveTab(client, "shell");
    expect(await openTabIds(client)).toEqual(["agent", "diff", "shell"]);

    // The shortcut that opened a view raises it again rather than toggling it
    // shut — closing is ⌘W's job.
    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");
    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");
    expect(await openTabIds(client)).toEqual(["agent", "diff", "shell"]);

    // ⌘W closes the tab in front and hands over to the one that takes its
    // place.
    await pressCloseTab(client);
    await waitForActiveTab(client, "shell");
    expect(await openTabIds(client)).toEqual(["agent", "shell"]);

    // Escape belongs to whatever runs in the shell, so it does not close it.
    await pressShortcut(client, { key: "Escape" });
    await sleep(400);
    expect(await openTabIds(client)).toEqual(["agent", "shell"]);

    await pressCloseTab(client);
    await waitForActiveTab(client, "agent");
    expect(await openTabIds(client)).toEqual(["agent"]);
  });

  it("closes a diff tab with Escape once no modal wants the key", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");

    await pressShortcut(client, { key: "Escape" });
    await waitForActiveTab(client, "agent");
    expect(await openTabIds(client)).toEqual(["agent"]);
  });

  it("keeps each tab's view alive while another tab is in front", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);
    await waitForActiveTab(client, "agent");

    await pressShortcut(client, { key: "j", meta: true });
    await waitForActiveTab(client, "shell");
    const shellTerminals = await client.executeSync<number>(
      `return document.querySelectorAll(".shell-modal .xterm").length;`
    );
    expect(shellTerminals).toBeGreaterThan(0);

    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");

    // Hidden, but still in the DOM: the xterm buffer is not rebuilt when the
    // shell comes back, which is why tabs use v-show rather than v-if.
    const hiddenShell = await client.executeSync<boolean>(
      `const shell = document.querySelector(".shell-modal");
       if (!shell) return false;
       const overlay = shell.closest(".embedded-view");
       return Boolean(overlay) && getComputedStyle(overlay).display === "none";`
    );
    expect(hiddenShell).toBe(true);

    await pressShortcut(client, { key: "j", meta: true });
    await waitForActiveTab(client, "shell");
  });

  it("gives every task its own tabs", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);
    await waitForActiveTab(client, "agent");
    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");

    await selectTask(secondTaskId);
    await closeViewTabs(client);
    await waitForActiveTab(client, "agent");
    expect(await openTabIds(client)).toEqual(["agent"]);

    // Coming back restores what that task had open — the point of tabs over
    // the ephemeral modals they replaced.
    await selectTask(taskId);
    await waitForActiveTab(client, "diff");
    expect(await openTabIds(client)).toEqual(["agent", "diff"]);
  });

  it("closes the tab in front from the native File menu, and spares the window", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);
    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");

    // ⌘W is the menu's accelerator in the packaged app, so this is the path
    // the key actually takes: the item emits, and the action closes the tab.
    // A synthesized keydown proves nothing here — the web handler skips it.
    const emitNativeClose = async () => {
      const result = await client.executeAsync<string>(
        `const cb = arguments[arguments.length - 1];
         import("/src/emit.ts")
           .then((module) => module.emit("kanna://native-close-window", {}))
           .then(() => cb("ok"))
           .catch((error) => cb("err:" + String(error && error.message ? error.message : error)));`
      );
      if (typeof result === "string" && result.startsWith("err:")) {
        throw new Error(`native close-window emit failed: ${result.slice(4)}`);
      }
      await sleep(500);
    };

    await emitNativeClose();
    await waitForActiveTab(client, "agent");
    expect(await openTabIds(client)).toEqual(["agent"]);
    expect(await client.getWindowHandles()).toHaveLength(1);

    // With a view open behind the agent session, ⌘W declines rather than
    // closing the window out from under it.
    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");
    await client.executeSync(
      `document.querySelector('[data-testid="main-tab-agent"]')?.click(); return true;`,
    );
    await waitForActiveTab(client, "agent");

    await emitNativeClose();
    expect(await openTabIds(client)).toEqual(["agent", "diff"]);
    expect(await client.getWindowHandles()).toHaveLength(1);

    await closeViewTabs(client);
  });

  it("gives a repository with no task selected a tab set of its own", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    // Deselect the task: the main area belongs to the repository now.
    await client.executeAsync<string>(
      `const cb = arguments[arguments.length - 1];
       const ctx = window.__KANNA_E2E__.setupState;
       const store = ctx.store;
       if (store?.selectedItemId?.__v_isRef) store.selectedItemId.value = null;
       else store.selectedItemId = null;
       setTimeout(function() { cb("ok"); }, 200);`
    );
    await sleep(400);

    // A repository has no agent session, so its tab set starts empty.
    expect(await openTabIds(client)).toEqual([]);

    await pressShortcut(client, { key: "g", meta: true });
    await waitForActiveTab(client, "graph");
    await pressShortcut(client, { key: "A", meta: true, shift: true });
    await waitForActiveTab(client, "analytics");
    expect(await openTabIds(client)).toEqual(["graph", "analytics"]);

    // ⌘J has no worktree to run in here, so it opens the repo-root shell.
    await pressShortcut(client, { key: "j", meta: true });
    await waitForActiveTab(client, "shell:repo");

    await closeViewTabs(client);
    expect(await openTabIds(client)).toEqual([]);

    // Back on the task, its own tabs are untouched by any of that.
    await selectTask(taskId);
    await sleep(400);
    expect(await openTabIds(client)).toEqual(["agent"]);
  });

  it("opens the remaining views as tabs of the selected task", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);
    await waitForActiveTab(client, "agent");

    await pressShortcut(client, { key: "E", meta: true, shift: true });
    await waitForActiveTab(client, "tree");
    await pressShortcut(client, { key: ",", meta: true });
    await waitForActiveTab(client, "preferences");
    await pressShortcut(client, { key: "J", meta: true, shift: true });
    await waitForActiveTab(client, "shell:repo");

    // The worktree shell and the repo-root shell are separate tabs.
    await pressShortcut(client, { key: "j", meta: true });
    await waitForActiveTab(client, "shell");
    expect(await openTabIds(client)).toEqual([
      "agent",
      "tree",
      "preferences",
      "shell:repo",
      "shell",
    ]);

    await closeViewTabs(client);
    await waitForActiveTab(client, "agent");
  });

  it("takes the operator to the view an agent opened, and only calls it opened once it is", async () => {
    // Start on the *other* task: the action has to bring this window to the
    // task the agent named, not decorate whichever one happened to be up.
    await selectTask(secondTaskId);
    await closeViewTabs(client);
    await selectTask(taskId);
    await closeViewTabs(client);
    await selectTask(secondTaskId);

    const server = await resolveAppKannaServer(client);
    const openView = async (body: Record<string, unknown>) => {
      const response = await localProcessFetch(`${server.baseUrl}/v1/desktop/views/open`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      expect(response.ok).toBe(true);
      return await response.json() as { opened: boolean; code?: string };
    };

    const file = await openView({
      taskId,
      view: "file",
      target: { path: "README.md", line: 1 },
    });
    // `opened` is a statement about the screen: the route did not answer until
    // this window had the file rendered.
    expect(file).toMatchObject({ opened: true });
    expect(await activeTabId(client)).toBe("file:README.md");
    await client.waitForText(".preview-modal .file-path", "README.md", 8_000);

    // Re-aiming the same file focuses the tab that is already showing it.
    expect(await openView({
      taskId,
      view: "file",
      target: { path: "README.md" },
    })).toMatchObject({ opened: true });
    expect(await openTabIds(client)).toEqual(["agent", "file:README.md"]);

    // The other views the action may open.
    expect(await openView({ taskId, view: "diff" })).toMatchObject({ opened: true });
    await waitForActiveTab(client, "diff");
    expect(await openView({ taskId, view: "graph" })).toMatchObject({ opened: true });
    await waitForActiveTab(client, "graph");
    expect(await openView({ taskId, view: "agent" })).toMatchObject({ opened: true });
    await waitForActiveTab(client, "agent");

    // A nested tree target has to survive the reader looking away. Switching
    // tabs re-renders the panel around the still-mounted explorer, and the
    // contained reader it is handed must not read as a new place to be.
    const worktreePath = `${testRepoPath}/.kanna-worktrees/task-${taskId}`;
    await tauriInvoke(client, "run_script", {
      script: `mkdir -p "${worktreePath}/nested/deep"`
        + ` && printf 'leaf\n' > "${worktreePath}/nested/deep/leaf-marker.txt"`,
      cwd: testRepoPath,
      env: {},
    });
    expect(await openView({
      taskId,
      view: "tree",
      target: { path: "nested/deep" },
    })).toMatchObject({ opened: true });
    await waitForActiveTab(client, "tree");
    await client.waitForText(".tree-modal", "leaf-marker.txt", 8_000);

    // An ordinary tab switch away and back — no reopen, no reveal.
    await client.executeSync(
      `document.querySelector('[data-testid="main-tab-agent"]').click();`,
    );
    await waitForActiveTab(client, "agent");
    await client.executeSync(
      `document.querySelector('[data-testid="main-tab-tree"]').click();`,
    );
    await waitForActiveTab(client, "tree");
    await sleep(500);

    const treeAfterSwitch = await client.executeSync<string>(
      `const tree = document.querySelector('.tree-modal');
       return tree ? tree.textContent : "";`,
    );
    // Still inside the directory the agent named, not back at the root.
    expect(treeAfterSwitch).toContain("leaf-marker.txt");
    expect(treeAfterSwitch).toContain("deep");
    await tauriInvoke(client, "run_script", {
      script: `rm -rf "${worktreePath}/nested"`,
      cwd: testRepoPath,
      env: {},
    });

    // A path outside the task's workspace is refused at the route, so a
    // mistyped path is an error the agent can act on rather than a silent
    // no-op — and nothing reaches a window.
    expect(await openView({
      taskId,
      view: "file",
      target: { path: "../outside.txt" },
    })).toMatchObject({ opened: false, code: "invalid_path" });

    // Neither a shell nor anything else outside the whitelist is reachable.
    expect(await openView({ taskId, view: "shell" }))
      .toMatchObject({ opened: false, code: "unsupported_view" });

    await closeViewTabs(client);
    await waitForActiveTab(client, "agent");
    expect(await openTabIds(client)).toEqual(["agent"]);
  });

  it("refuses a view whose content the window could not load", async () => {
    // A scope with no target is dispatched without the server reading
    // anything, so the window is the only thing that can discover the
    // worktree is gone — which is exactly what makes this a real renderer
    // refusal rather than a fabricated acknowledgement.
    const doomedId = await createTask("Main tabs doomed task");
    const doomedWorktree = `${testRepoPath}/.kanna-worktrees/task-${doomedId}`;
    await tauriInvoke(client, "run_script", {
      script: `rm -rf "${doomedWorktree}"`,
      cwd: testRepoPath,
      env: {},
    });

    const server = await resolveAppKannaServer(client);
    const openView = async (body: Record<string, unknown>) => {
      const response = await localProcessFetch(`${server.baseUrl}/v1/desktop/views/open`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      expect(response.ok).toBe(true);
      return await response.json() as { opened: boolean; code?: string; message?: string };
    };

    const diff = await openView({ taskId: doomedId, view: "diff" });
    expect(diff.opened).toBe(false);
    expect(diff.code).toBe("renderer_failed");
    // The window's own words, carried back to the caller.
    expect(diff.message ?? "").toContain("unavailable");

    const tree = await openView({ taskId: doomedId, view: "tree" });
    expect(tree.opened).toBe(false);
    expect(tree.message ?? "").toContain("unavailable");

    await selectTask(taskId);
    await closeViewTabs(client);
  });

  it("keeps a view an agent opened reading inside the task worktree", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    const worktree = `${testRepoPath}/.kanna-worktrees/task-${taskId}`;
    const outside = `${testRepoPath}/.kanna-worktrees/outside-of-task-${taskId}`;
    await tauriInvoke(client, "run_script", {
      script: `mkdir -p "${worktree}/probe" "${outside}"`
        + ` && printf 'inside\n' > "${worktree}/probe/inside-marker.txt"`
        + ` && printf 'secret\n' > "${outside}/outside-secret.txt"`,
      cwd: testRepoPath,
      env: {},
    });

    const server = await resolveAppKannaServer(client);
    const openView = async (body: Record<string, unknown>) => {
      const response = await localProcessFetch(`${server.baseUrl}/v1/desktop/views/open`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      return await response.json() as { opened: boolean; code?: string };
    };

    expect(await openView({ taskId, view: "tree", target: { path: "probe" } }))
      .toMatchObject({ opened: true });
    await waitForActiveTab(client, "tree");
    await client.waitForText(".tree-modal", "inside-marker.txt", 8_000);

    // The directory the server validated becomes a link out of the worktree,
    // and the explorer is made to read it again — the read that happens after
    // dispatch, which the pre-dispatch check cannot fence.
    await tauriInvoke(client, "run_script", {
      script: `rm -rf "${worktree}/probe" && ln -s "${outside}" "${worktree}/probe"`,
      cwd: testRepoPath,
      env: {},
    });
    // `a` re-lists the current directory under the other visibility, so this
    // is a genuine fresh read rather than a cached column. The explorer owns
    // its keys on its own element, so the event goes there rather than to the
    // window, where it would never reach the handler.
    await client.executeSync(buildSelectorKeydownScript(".tree-modal", { key: "a" }));
    await sleep(800);

    // Positive proof that a read actually happened and was refused, so the
    // absence below is containment rather than a keypress that went nowhere.
    await client.waitForText('[data-testid="tree-explorer-unavailable"]', "unavailable", 8_000);
    const shown = await client.executeSync<string>(
      `const tree = document.querySelector('.tree-modal');
       return tree ? tree.textContent : "";`,
    );
    expect(shown).not.toContain("outside-secret.txt");

    await tauriInvoke(client, "run_script", {
      script: `rm -f "${worktree}/probe" && rm -rf "${outside}"`,
      cwd: testRepoPath,
      env: {},
    });
    await closeViewTabs(client);
  });

  it("shows the anchored diff line as it reads now, not as the tab last rendered it", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    const worktree = `${testRepoPath}/.kanna-worktrees/task-${taskId}`;
    const server = await resolveAppKannaServer(client);
    const openView = async (body: Record<string, unknown>) => {
      const response = await localProcessFetch(`${server.baseUrl}/v1/desktop/views/open`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      return await response.json() as { opened: boolean; code?: string; message?: string };
    };
    const anchoredLine = async (): Promise<number> => {
      const response = await localProcessFetch(
        `${server.baseUrl}/v1/tasks/${taskId}/diff?scope=working`,
      );
      const patch = ((await response.json()) as { patch: string }).patch.split("\n");
      let line = 0;
      for (let index = 0; index < patch.length; index += 1) {
        const header = /^@@ -\d+(?:,\d+)? \+(\d+)/.exec(patch[index]);
        if (header) { line = Number(header[1]); continue; }
        if (patch[index].startsWith("+") && !patch[index].startsWith("+++")) {
          if (patch[index].includes("ANCHOR-")) return line;
          line += 1;
        } else if (patch[index].startsWith(" ")) {
          line += 1;
        }
      }
      throw new Error(`no anchored line in ${patch.join("\n")}`);
    };

    await tauriInvoke(client, "run_script", {
      script: `printf '\nANCHOR-ORIGINAL\n' >> README.md`,
      cwd: worktree,
      env: {},
    });
    expect(await openView({
      taskId,
      view: "diff",
      target: {
        scope: "working",
        path: "README.md",
        side: "new",
        line: await anchoredLine(),
        excerpt: "ANCHOR-ORIGINAL",
      },
    })).toMatchObject({ opened: true });
    await waitForActiveTab(client, "diff");

    // Replace the anchored line in place: same number, still an addition,
    // different text. A tab that does not re-read would scroll to the old
    // text and call it opened.
    await tauriInvoke(client, "run_script", {
      script: `sed -i '' 's/ANCHOR-ORIGINAL/ANCHOR-REPLACED/' README.md`,
      cwd: worktree,
      env: {},
    });
    expect(await openView({
      taskId,
      view: "diff",
      target: {
        scope: "working",
        path: "README.md",
        side: "new",
        line: await anchoredLine(),
        excerpt: "ANCHOR-REPLACED",
      },
    })).toMatchObject({ opened: true });

    const rendered = await client.executeSync<string>(
      `return Array.from(document.querySelectorAll('.diff-file'))
        .flatMap((file) => Array.from(file.querySelectorAll('diffs-container')))
        .map((container) => container.shadowRoot ? container.shadowRoot.textContent : "")
        .join(" ");`,
    );
    // What the reader is looking at is the line the open was answered for.
    expect(rendered).toContain("ANCHOR-REPLACED");
    expect(rendered).not.toContain("ANCHOR-ORIGINAL");

    await tauriInvoke(client, "run_script", {
      script: `sed -i '' '/ANCHOR-REPLACED/d' README.md`,
      cwd: worktree,
      env: {},
    });
    await closeViewTabs(client);
  });

  it("brings a task's tabs back after the app restarts, and forgets a closed task's", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");
    await callVueMethod(client, "openFilePreview", "README.md");
    await waitForActiveTab(client, "file:README.md");
    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");

    // The tab state is written on a debounce, so a reload that beat the write
    // would prove nothing. Reload rather than open a second WebDriver session:
    // a new session attaches to the app that is still running, so the tabs
    // would never have left memory and this would pass with no storage at all.
    await sleep(1_200);

    await client.reload();
    await selectTask(taskId);

    // The agent session is rebuilt rather than stored, and the tab that was in
    // front is the tab that comes back in front.
    await waitForActiveTab(client, "diff");
    expect(await openTabIds(client)).toEqual(["agent", "diff", "file:README.md"]);
    await client.waitForText(".preview-modal .file-path", "README.md", 8_000);

    // The other task never opened a view, so it comes back as it was.
    await selectTask(secondTaskId);
    expect(await openTabIds(client)).toEqual(["agent"]);

    await selectTask(taskId);
    await closeViewTabs(client);
    await sleep(1_200);
  });

  it("gives the less keys to the tab in front, not to the ones behind it", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");
    await pressShortcut(client, { key: "g", meta: true });
    await waitForActiveTab(client, "graph");

    // `q` is a window-level binding, and a tab behind another one is still
    // mounted: without a foreground gate the hidden diff answered this too and
    // closed itself along with the graph.
    await pressShortcut(client, { key: "q" });
    await sleep(400);

    expect(await openTabIds(client)).toEqual(["agent", "diff"]);
    await waitForActiveTab(client, "diff");

    // Same again with a file in front, which has always gated its own keys —
    // what has to survive here is the diff behind it.
    await callVueMethod(client, "openFilePreview", "README.md");
    await waitForActiveTab(client, "file:README.md");
    await pressShortcut(client, { key: "q" });
    await sleep(400);

    expect(await openTabIds(client)).toEqual(["agent", "diff"]);

    await closeViewTabs(client);
  });

  it("runs the IDE once, on the file, when a file tab is in front", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    // The same path createTask made the worktree at.
    const worktreePath = `${testRepoPath}/.kanna-worktrees/task-${taskId}`;
    const ideLog = `${worktreePath}/.ide-invocations`;
    const ideRecorder = `${worktreePath}/.record-ide.sh`;
    // The app runs `${ideCommand} "<path>"`, so a recorder standing in for the
    // editor captures the exact path each binding passed.
    await tauriInvoke(client, "run_script", {
      script: [
        `cat > "${ideRecorder}" <<'SH'`,
        "#!/bin/sh",
        `printf '%s\\n' "$1" >> "${ideLog}"`,
        "SH",
        `chmod +x "${ideRecorder}"`,
        `rm -f "${ideLog}"`,
      ].join("\n"),
      cwd: worktreePath,
      env: {},
    });
    await client.executeSync(
      `const ctx = window.__KANNA_E2E__.setupState;
       const ide = ctx.store.ideCommand;
       if (ide && ide.__v_isRef) ide.value = ${JSON.stringify(JSON.stringify(ideRecorder))};
       else ctx.store.ideCommand = ${JSON.stringify(JSON.stringify(ideRecorder))};
       return true;`
    );

    await callVueMethod(client, "openFilePreview", "README.md");
    await waitForActiveTab(client, "file:README.md");

    await pressShortcut(client, { key: "o", meta: true });
    await sleep(1200);

    const recorded = await tauriInvoke(client, "run_script", {
      script: `cat "${ideLog}" 2>/dev/null || true`,
      cwd: worktreePath,
      env: {},
    }) as string;
    // macOS resolves the temp fixture path through /private; compare the real
    // paths rather than the spelling each side happened to use.
    const realPath = (path: string) => path.replace(/^\/private/, "");
    const lines = String(recorded)
      .split("\n")
      .map((line) => realPath(line.trim()))
      .filter(Boolean);
    // Both the global shortcut and the file view bind ⌘O, and a matched global
    // only calls preventDefault, so the keydown reached both: the editor opened
    // twice, once on the worktree and once on the file.
    expect(lines).toEqual([realPath(`${worktreePath}/README.md`)]);

    await closeViewTabs(client);
  });

  it("gives ⌘F to a file tab's own find, leaving the sidebar search alone", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    // A plain-text file: the preview disables its own find while it is
    // rendering markdown, so README.md would not claim the key.
    await callVueMethod(client, "openFilePreview", "src/index.txt");
    await waitForActiveTab(client, "file:src/index.txt");

    await watchSidebarSearchFocus(client);
    await pressShortcut(client, { key: "f", meta: true });
    await sleep(400);

    // Focus alone is not the assertion: the sidebar focuses synchronously and
    // the view focuses on the next tick, so whoever ends up focused says
    // nothing about whether both fired.
    expect(await sidebarSearchWasFocused(client)).toBe(false);
    expect(await focusedSearchOwner(client)).toBe("view");

    await closeViewTabs(client);
  });

  it("gives ⌘F to a diff tab's own find, leaving the sidebar search alone", async () => {
    await selectTask(taskId);
    await closeViewTabs(client);

    await pressShortcut(client, { key: "d", meta: true });
    await waitForActiveTab(client, "diff");

    await watchSidebarSearchFocus(client);
    await pressShortcut(client, { key: "f", meta: true });
    await sleep(400);

    expect(await sidebarSearchWasFocused(client)).toBe(false);
    expect(await focusedSearchOwner(client)).toBe("view");

    await closeViewTabs(client);
  });
});
