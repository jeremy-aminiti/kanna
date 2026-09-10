import { mkdir } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { cleanupFixtureRepos, createFixtureRepo } from "../helpers/fixture-repo";
import { resolveAppKannaServer } from "../helpers/kannaServer";
import { cleanupWorktrees, importTestRepo, resetDatabase } from "../helpers/reset";
import { createPrimaryAndSecondaryClients } from "../helpers/twoInstance";
import { callVueMethod, tauriInvoke, setPreferencesOpen } from "../helpers/vue";
import type { WebDriverClient } from "../helpers/webdriver";
import { localProcessFetch } from "@kanna/local-process-fetch";
import { formatAppWindowTitle, type AppBuildInfo } from "../../../src/stores/windowTitle";

const { primary, secondary } = createPrimaryAndSecondaryClients();

interface Dimensions {
  cols: number;
  rows: number;
}

interface RenderedTerminal extends Dimensions {
  markerRendered: boolean;
}

interface FocusObservation {
  documentHasFocus: boolean;
  focusEvents: boolean[];
  nativeFocusError: string | null;
  nativeFocusedAfter: boolean | null;
  nativeFocusedBefore: boolean | null;
  terminalHasFocus: boolean;
}

let fixtureRepoPath = "";
let primaryRepoId = "";
let ownerDesktopId = "";
let ownerTaskId: string | null = null;

function expectedWorktreeIdentity(): { taskId: string; worktree: string } {
  const worktree = basename(resolve(process.cwd(), "../.."));
  const match = /^task-(.+?)(?:-\d+)?$/.exec(worktree);
  if (!match?.[1]) {
    throw new Error(`real remote E2E requires a task worktree title, got ${worktree}`);
  }
  return { taskId: match[1], worktree };
}

async function assertTaskSpecificDevWindow(client: WebDriverClient, label: string): Promise<void> {
  const expectedIdentity = expectedWorktreeIdentity();
  const buildInfo = await tauriInvoke(client, "get_app_build_info") as AppBuildInfo;
  expect(buildInfo.taskId).toBe(expectedIdentity.taskId);
  expect(buildInfo.worktree).toBe(expectedIdentity.worktree);

  const expectedTitle = formatAppWindowTitle(buildInfo);
  if (!expectedTitle) {
    throw new Error(`${label} did not report a task-specific dev window title`);
  }
  const actualTitle = await client.getNativeWindowTitle();
  expect(actualTitle).toBe(expectedTitle);
  console.log(`[e2e] ${label} dev window: ${actualTitle}; webdriver=${client.getBaseUrl()}`);
}

async function setSetupState(
  client: WebDriverClient,
  key: string,
  value: unknown,
): Promise<void> {
  await client.executeSync(`
    const state = window.__KANNA_E2E__?.setupState;
    const current = state?.[${JSON.stringify(key)}];
    if (current?.__v_isRef) current.value = ${JSON.stringify(value)};
    else if (state) state[${JSON.stringify(key)}] = ${JSON.stringify(value)};
  `);
}

async function signIn(client: WebDriverClient): Promise<void> {
  await setPreferencesOpen(client, true);
  await client.click(await client.waitForElement('[data-testid="preferences-account-tab"]'));
  await client.sendKeys(await client.waitForElement('[data-testid="account-email"]'), "upvote.sieve.7t@icloud.com");
  await client.sendKeys(await client.waitForElement('[data-testid="account-password"]'), "password123");
  await client.click(await client.waitForElement('[data-testid="account-sign-in"] .primary-button'));
  await client.waitForText(".prefs-panel", "upvote.sieve.7t@icloud.com", 15_000);
  await callVueMethod(client, "associateDesktopCloudCredential");
  await setPreferencesOpen(client, false);
  await setSetupState(client, "maximized", false);
  await setSetupState(client, "sidebarHidden", false);
}

async function waitForOwnerDesktopId(): Promise<string> {
  const deadline = Date.now() + 30_000;
  let latest: unknown = null;
  while (Date.now() < deadline) {
    latest = await tauriInvoke(primary, "mobile_server_status");
    const status = latest as { state?: string; desktopId?: string };
    if (status.state === "running" && status.desktopId) return status.desktopId;
    await sleep(250);
  }
  throw new Error(`owner desktop did not publish a cloud identity: ${JSON.stringify(latest)}`);
}

async function ownerDimensions(taskId: string): Promise<Dimensions> {
  const state = await tauriInvoke(primary, "get_session_recovery_state", {
    sessionId: taskId,
  }) as { cols?: unknown; rows?: unknown } | null;
  if (typeof state?.cols !== "number" || typeof state.rows !== "number") {
    throw new Error(`owner dimensions unavailable: ${JSON.stringify(state)}`);
  }
  return { cols: state.cols, rows: state.rows };
}

async function renderedDimensions(
  client: WebDriverClient,
  taskId: string,
): Promise<RenderedTerminal> {
  const dimensions = await client.executeSync<RenderedTerminal | null>(`
    const hook = window.__KANNA_E2E__?.terminalBuffers;
    const remoteId = "remote:" + ${JSON.stringify(taskId)};
    const id = hook?.sessionIds?.().includes(remoteId) ? remoteId : ${JSON.stringify(taskId)};
    const cursor = hook?.cursor?.(id);
    const terminal = hook?.element?.(id);
    const host = terminal?.closest?.(".cloud-terminal-cache-entry, .terminal-container")
      ?? terminal;
    const screen = host?.querySelector?.(".xterm-screen");
    const rect = screen?.getBoundingClientRect() ?? host?.getBoundingClientRect();
    const rows = screen?.querySelector?.(".xterm-rows");
    const markerRendered = Array.from(rows?.children ?? []).some(
      (row) => row.textContent?.includes("ACTIVE_VIEW:"),
    );
    return cursor && rect && rect.width > 0 && rect.height > 0
      ? { cols: cursor.columns, rows: cursor.rows, markerRendered }
      : null;
  `);
  if (!dimensions) throw new Error(`rendered dimensions unavailable for ${taskId}`);
  return dimensions;
}

async function refreshRenderedTerminal(client: WebDriverClient, taskId: string): Promise<void> {
  await client.executeSync(`
    const hook = window.__KANNA_E2E__?.terminalBuffers;
    const remoteId = "remote:" + ${JSON.stringify(taskId)};
    const id = hook?.sessionIds?.().includes(remoteId) ? remoteId : ${JSON.stringify(taskId)};
    hook?.refresh?.(id);
  `);
  // xterm's private synchronous refresh schedules DOM-row painting; let that
  // paint land before reading the actual row cells or taking a screenshot.
  await sleep(250);
}

async function waitForOwnerAndRenderer(
  client: WebDriverClient,
  taskId: string,
  expected?: Dimensions,
): Promise<Dimensions> {
  let latest: unknown = null;
  try {
    await expect.poll(async () => {
      try {
        await refreshRenderedTerminal(client, taskId);
        const [daemon, rendered] = await Promise.all([
          ownerDimensions(taskId),
          renderedDimensions(client, taskId),
        ]);
        latest = { daemon, rendered };
        return daemon.cols === rendered.cols && daemon.rows === rendered.rows
          && rendered.markerRendered
          && (!expected || (daemon.cols === expected.cols && daemon.rows === expected.rows));
      } catch (error) {
        latest = error instanceof Error ? error.message : String(error);
        return false;
      }
    }, { timeout: 30_000, interval: 150 }).toBe(true);
  } catch (error) {
    throw new Error(
      `owner/rendered terminal did not converge for ${taskId}: ${JSON.stringify(latest)}`,
      { cause: error },
    );
  }
  return (latest as { daemon: Dimensions }).daemon;
}

async function focusTerminal(client: WebDriverClient, ownerTaskId: string): Promise<void> {
  const focusState = await client.executeAsync<FocusObservation>(`
    const done = arguments[arguments.length - 1];
    void (async () => {
    const internals = window.__TAURI_INTERNALS__;
    const label = internals?.metadata?.currentWindow?.label;
    if (!internals || typeof label !== "string" || label.length === 0) {
      done({ nativeFocusError: "current native window label unavailable" });
      return;
    }
    const focusEvents = [];
    const listeners = [];
    const listen = async (event, focused) => {
      const handler = internals.transformCallback(() => focusEvents.push(focused), false);
      const eventId = await internals.invoke("plugin:event|listen", {
        event,
        target: { kind: "Window", label },
        handler,
      });
      listeners.push({ event, eventId, handler });
    };
    const cleanup = async () => {
      await Promise.all(listeners.map(async ({ event, eventId, handler }) => {
        internals.unregisterCallback?.(handler);
        await internals.invoke("plugin:event|unlisten", { event, eventId });
      }));
    };
    let result;
    try {
      await Promise.all([
        listen("tauri://focus", true),
        listen("tauri://blur", false),
      ]);
      const nativeFocusedBefore = await internals.invoke("plugin:window|is_focused", { label });
      let nativeFocusError = null;
      try {
        await internals.invoke("plugin:window|set_focus", { label });
      } catch (error) {
        nativeFocusError = String(error);
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
      window.focus();
      const remote = document.querySelector(
        ".cloud-terminal-shell[data-owner-task-id=" + JSON.stringify(${JSON.stringify(ownerTaskId)}) + "] .xterm-helper-textarea",
      );
      const local = document.querySelector(".main-panel .terminal-container .xterm-helper-textarea");
      const input = remote instanceof HTMLElement ? remote : local;
      if (input instanceof HTMLElement) input.focus();
      const nativeFocusedAfter = await internals.invoke("plugin:window|is_focused", { label });
      result = {
        documentHasFocus: document.hasFocus(),
        focusEvents,
        nativeFocusError,
        nativeFocusedAfter,
        nativeFocusedBefore,
        terminalHasFocus: document.activeElement === input,
      };
    } catch (error) {
      result = { nativeFocusError: String(error) };
    } finally {
      await cleanup();
    }
    done(result);
    })();
  `);
  if (focusState.nativeFocusError) {
    throw new Error(`native main-window focus failed: ${JSON.stringify(focusState)}`);
  }
  if (!focusState.documentHasFocus || !focusState.terminalHasFocus) {
    throw new Error(`foreground terminal focus was not established: ${JSON.stringify(focusState)}`);
  }
}

async function waitForRemoteTask(taskId: string): Promise<string> {
  const deadline = Date.now() + 90_000;
  let latest: unknown = null;
  while (Date.now() < deadline) {
    latest = await secondary.executeSync(`
      const read = (value) => value?.__v_isRef ? value.value : value;
      const snapshot = read(window.__KANNA_E2E__?.setupState?.cloudSnapshot) || {};
      const match = Object.entries(snapshot.terminalRefs || {}).find(([, ref]) =>
        ref.ownerDesktopId === ${JSON.stringify(ownerDesktopId)} &&
        ref.ownerLocalTaskId === ${JSON.stringify(taskId)} &&
        (ref.transport || "cloud") === "cloud"
      );
      return match ? { itemId: match[0], ref: match[1] } : {
        refs: Object.keys(snapshot.terminalRefs || {}),
      };
    `);
    const candidate = latest as { itemId?: string };
    if (candidate.itemId) return candidate.itemId;
    await sleep(250);
  }
  throw new Error(`remote task did not retain private owner identity: ${JSON.stringify(latest)}`);
}

async function selectRemoteTask(itemId: string, taskId: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  let latest: unknown = null;
  while (Date.now() < deadline) {
    latest = await secondary.executeSync(`
      const row = Array.from(document.querySelectorAll(".sidebar .workflow-item[data-task-id]"))
        .find((candidate) => candidate.dataset.taskId === ${JSON.stringify(itemId)} && candidate.getClientRects().length > 0);
      if (row instanceof HTMLElement) row.click();
      const read = (value) => value?.__v_isRef ? value.value : value;
      const diagnostics = read(window.__KANNA_E2E__?.setupState?.remoteTaskDiagnostics) || [];
      return diagnostics.find((entry) => entry.itemId === ${JSON.stringify(itemId)}) || null;
    `);
    const diagnostic = latest as {
      selectedTerminalTransport?: string;
      ownerDesktopId?: string;
      ownerLocalTaskId?: string;
    } | null;
    if (diagnostic?.selectedTerminalTransport === "cloud"
      && diagnostic.ownerDesktopId === ownerDesktopId
      && diagnostic.ownerLocalTaskId === taskId) return;
    await sleep(200);
  }
  throw new Error(`remote selection lost owner identity: ${JSON.stringify(latest)}`);
}

async function createOwnerTask(): Promise<string> {
  const script = [
    "select(STDOUT); $| = 1;",
    "sub draw { my $size = `stty size`; $size =~ s/\\s+$//; my ($rows, $cols) = split(/\\s+/, $size); print qq{ACTIVE_VIEW:${cols}x${rows}\\n}; }",
    "$SIG{WINCH} = sub { draw(); };",
    "draw(); while (1) { sleep 1; }",
  ].join(" ");
  const quote = (value: string): string => `'${value.replaceAll("'", "'\\''")}'`;
  await primary.setWindowRect({ width: 2200, height: 1200, x: 40, y: 40 });
  const { baseUrl } = await resolveAppKannaServer(primary);
  const response = await localProcessFetch(`${baseUrl}/v1/tasks`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      repoId: primaryRepoId,
      prompt: "Remote active-view restoration fixture",
      displayName: "Remote active-view restoration fixture",
      baseRef: "origin/main",
      agentProvider: "codex",
      agentType: "pty",
      terminalCols: 140,
      terminalRows: 50,
      setupCmds: [`/usr/bin/perl -e ${quote(script)}`],
    }),
  });
  if (!response.ok) throw new Error(`owner fixture creation failed: ${response.status} ${await response.text()}`);
  const created = await response.json() as { taskId?: unknown };
  if (typeof created.taskId !== "string") throw new Error(`owner fixture returned no task id: ${JSON.stringify(created)}`);
  await callVueMethod(primary, "loadItems", primaryRepoId);
  await callVueMethod(primary, "store.selectItem", created.taskId);
  await primary.waitForElement(".main-panel .terminal-container .xterm-helper-textarea", 30_000);
  await expect.poll(
    () => primary.executeSync<boolean>(`
      return window.__KANNA_E2E__?.terminalBuffers?.lines(${JSON.stringify(created.taskId)})
        ?.some((line) => line.includes("ACTIVE_VIEW:")) ?? false;
    `),
    { timeout: 30_000, interval: 150 },
  ).toBe(true);
  await focusTerminal(primary, created.taskId);
  return created.taskId;
}

async function capture(client: WebDriverClient, name: string): Promise<void> {
  const directory = process.env.KANNA_E2E_SCREENSHOT_DIR;
  if (!directory) return;
  await mkdir(directory, { recursive: true });
  await client.screenshot(join(directory, name));
}

describe("remote active-view restoration", () => {
  beforeAll(async () => {
    if (process.env.KANNA_E2E_NO_ACTIVATE !== "0") {
      throw new Error(
        "remote active-view restoration requires foreground-capable desktop windows; " +
        "the runner must set KANNA_E2E_NO_ACTIVATE=0 for this target",
      );
    }
    expectedNativeWindowIdentity = await resolveExpectedNativeWindowIdentity(
      resolve(process.cwd(), "../.."),
    );
    await primary.createSession();
    await secondary.createSession();
    // Each WebDriver port must independently prove that it is bound to this
    // task's dev window before the test resets state or interacts with it.
    await assertTaskSpecificDevWindow(primary, "primary");
    await assertTaskSpecificDevWindow(secondary, "secondary");
    await resetDatabase(primary);
    await resetDatabase(secondary);
    fixtureRepoPath = await createFixtureRepo("remote-active-view-restoration");
    primaryRepoId = await importTestRepo(primary, fixtureRepoPath, "active-view-owner");
    await importTestRepo(secondary, fixtureRepoPath, "active-view-viewer");
    await signIn(primary);
    await signIn(secondary);
    ownerDesktopId = await waitForOwnerDesktopId();
  }, 180_000);

  afterAll(async () => {
    if (ownerTaskId) {
      await tauriInvoke(primary, "kill_session", { sessionId: ownerTaskId }).catch(() => undefined);
    }
    await cleanupWorktrees(primary, fixtureRepoPath).catch(() => undefined);
    await cleanupWorktrees(secondary, fixtureRepoPath).catch(() => undefined);
    await cleanupFixtureRepos(fixtureRepoPath ? [fixtureRepoPath] : []).catch(() => undefined);
    await primary.deleteSession().catch(() => undefined);
    await secondary.deleteSession().catch(() => undefined);
  });

  it("gives sizing to the foreground remote view and restores the owner view without input", async () => {
    ownerTaskId = await createOwnerTask();
    const ownerInitial = await waitForOwnerAndRenderer(primary, ownerTaskId);
    expect(ownerInitial.cols).toBeGreaterThan(80);
    expect(ownerInitial.rows).toBeGreaterThan(24);

    const remoteItemId = await waitForRemoteTask(ownerTaskId);
    await secondary.setWindowRect({ width: 1600, height: 900, x: 80, y: 80 });
    await selectRemoteTask(remoteItemId, ownerTaskId);
    await focusTerminal(secondary, ownerTaskId);
    const remoteActive = await waitForOwnerAndRenderer(secondary, ownerTaskId);
    expect(remoteActive.cols).toBeLessThan(ownerInitial.cols);
    expect(remoteActive.rows).toBeLessThan(ownerInitial.rows);
    await capture(secondary, "remote-active-view-controls-grid.png");

    // This is the actual local desktop foreground handback. Do not send any
    // terminal bytes: focus alone must restore its measured grid.
    await focusTerminal(primary, ownerTaskId);
    const ownerRestored = await waitForOwnerAndRenderer(primary, ownerTaskId, ownerInitial);
    expect(ownerRestored).toEqual(ownerInitial);
    await capture(primary, "owner-restored-without-terminal-input.png");
  }, 180_000);
});
