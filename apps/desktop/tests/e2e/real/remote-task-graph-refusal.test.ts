import { execFile } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { promisify } from "node:util";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { cleanupFixtureRepos, createFixtureRepo } from "../helpers/fixture-repo";
import { resolveAppKannaServer } from "../helpers/kannaServer";
import { buildGlobalKeydownScript } from "../helpers/keyboard";
import { cleanupWorktrees, importTestRepo, resetDatabase } from "../helpers/reset";
import { createPrimaryAndSecondaryClients } from "../helpers/twoInstance";
import { callVueMethod, queryDb, tauriInvoke, setPreferencesOpen } from "../helpers/vue";
import { localProcessFetch } from "@kanna/local-process-fetch";
import { formatAppWindowTitle, type AppBuildInfo } from "../../../src/stores/windowTitle";

const { primary, secondary } = createPrimaryAndSecondaryClients();
const execFileAsync = promisify(execFile);

let fixtureRepoPath = "";
let primaryRepoId = "";
let ownerDesktopId = "";

function expectedWorktreeIdentity(): { taskId: string; worktree: string } {
  const worktree = basename(resolve(process.cwd(), "../.."));
  const match = /^task-(.+?)(?:-\d+)?$/.exec(worktree);
  if (!match?.[1]) {
    throw new Error(`real remote E2E requires a task worktree title, got ${worktree}`);
  }
  return { taskId: match[1], worktree };
}

async function assertTaskSpecificDevWindow(client: typeof primary, label: string): Promise<void> {
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

async function setSetupState(client: typeof primary, key: string, value: unknown): Promise<void> {
  await client.executeSync(`
    const current = window.__KANNA_E2E__?.setupState?.[${JSON.stringify(key)}];
    if (current?.__v_isRef) current.value = ${JSON.stringify(value)};
    else if (window.__KANNA_E2E__?.setupState) window.__KANNA_E2E__.setupState[${JSON.stringify(key)}] = ${JSON.stringify(value)};
  `);
}

async function signIn(client: typeof primary): Promise<void> {
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
  while (Date.now() < deadline) {
    const status = await tauriInvoke(primary, "mobile_server_status") as { state?: string; desktopId?: string };
    if (status.state === "running" && status.desktopId) return status.desktopId;
    await sleep(250);
  }
  throw new Error("primary desktop did not publish its cloud identity");
}

async function waitForRemoteTask(ownerTaskId: string): Promise<string> {
  const deadline = Date.now() + 90_000;
  let latest: unknown = null;
  while (Date.now() < deadline) {
    latest = await secondary.executeSync(`
      const snapshot = window.__KANNA_E2E__?.setupState?.cloudSnapshot;
      const read = (value) => value?.__v_isRef ? value.value : value;
      const value = read(snapshot) || {};
      const match = Object.entries(value.terminalRefs || {}).find(([, ref]) =>
        ref.ownerDesktopId === ${JSON.stringify(ownerDesktopId)} &&
        ref.ownerLocalTaskId === ${JSON.stringify(ownerTaskId)} &&
        (ref.transport || "cloud") === "cloud"
      );
      return match ? { itemId: match[0], ref: match[1] } : {
        taskIds: Object.keys(value.terminalRefs || {}),
      };
    `);
    const candidate = latest as { itemId?: string };
    if (candidate.itemId) return candidate.itemId;
    await sleep(250);
  }
  throw new Error(`remote owner task was not indexed: ${JSON.stringify(latest)}`);
}

async function selectRemoteTask(itemId: string, ownerTaskId: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  let latest: unknown = null;
  while (Date.now() < deadline) {
    latest = await secondary.executeSync(`
      const row = Array.from(document.querySelectorAll('.sidebar .workflow-item[data-task-id]'))
        .find((candidate) => candidate.dataset.taskId === ${JSON.stringify(itemId)} && candidate.getClientRects().length > 0);
      if (row instanceof HTMLElement) row.click();
      const diagnostics = window.__KANNA_E2E__?.setupState?.remoteTaskDiagnostics;
      const read = (value) => value?.__v_isRef ? value.value : value;
      const entry = (read(diagnostics) || []).find((candidate) => candidate.itemId === ${JSON.stringify(itemId)});
      return entry ? JSON.parse(JSON.stringify(entry)) : null;
    `);
    const state = latest as { selectedTerminalTransport?: string; ownerDesktopId?: string; ownerLocalTaskId?: string } | null;
    if (state?.selectedTerminalTransport === "cloud" && state.ownerDesktopId === ownerDesktopId && state.ownerLocalTaskId === ownerTaskId) return;
    await sleep(200);
  }
  throw new Error(`remote task selection did not retain owner identity: ${JSON.stringify(latest)}`);
}

async function createOwnerTask(): Promise<{ taskId: string; worktreePath: string }> {
  const prompt = "Remote graph and local-action refusal fixture";
  const { baseUrl } = await resolveAppKannaServer(primary);
  const response = await localProcessFetch(`${baseUrl}/v1/tasks`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      repoId: primaryRepoId,
      prompt,
      displayName: prompt,
      baseRef: "origin/main",
      agentProvider: "codex",
      agentType: "pty",
      setupCmds: ["printf 'REMOTE_GRAPH_READY\\n'; while :; do sleep 30; done"],
    }),
  });
  if (!response.ok) throw new Error(`owner fixture creation failed: ${response.status} ${await response.text()}`);
  const { taskId } = await response.json() as { taskId?: string };
  if (!taskId) throw new Error("owner fixture creation returned no task id");
  const rows = await queryDb(primary, "SELECT path FROM worktree WHERE pipeline_item_id = ? ORDER BY created_at DESC LIMIT 1", [taskId]);
  const worktreePath = (rows[0] as { path?: string } | undefined)?.path;
  if (!worktreePath) throw new Error(`owner fixture has no worktree: ${JSON.stringify(rows)}`);
  return { taskId, worktreePath };
}

async function capture(name: string): Promise<void> {
  const directory = process.env.KANNA_E2E_SCREENSHOT_DIR;
  if (!directory) return;
  await mkdir(directory, { recursive: true });
  await secondary.screenshot(join(directory, name));
}

describe("remote task graph and local action refusal", () => {
  beforeAll(async () => {
    await primary.createSession();
    await secondary.createSession();
    await assertTaskSpecificDevWindow(primary, "primary");
    await assertTaskSpecificDevWindow(secondary, "secondary");
    await resetDatabase(primary);
    await resetDatabase(secondary);
    fixtureRepoPath = await createFixtureRepo("remote-task-graph-refusal");
    primaryRepoId = await importTestRepo(primary, fixtureRepoPath, "remote-graph-owner");
    await importTestRepo(secondary, fixtureRepoPath, "remote-graph-viewer");
    await signIn(primary);
    await signIn(secondary);
    ownerDesktopId = await waitForOwnerDesktopId();
  }, 180_000);

  afterAll(async () => {
    await cleanupWorktrees(primary, fixtureRepoPath).catch(() => undefined);
    await cleanupWorktrees(secondary, fixtureRepoPath).catch(() => undefined);
    await cleanupFixtureRepos(fixtureRepoPath ? [fixtureRepoPath] : []).catch(() => undefined);
    await primary.deleteSession().catch(() => undefined);
    await secondary.deleteSession().catch(() => undefined);
  });

  it("renders the owning desktop graph and refuses viewer-local path actions", async () => {
    const owner = await createOwnerTask();
    await writeFile(join(owner.worktreePath, "remote-graph-proof.txt"), "owned by remote desktop\n");
    await execFileAsync("git", ["add", "remote-graph-proof.txt"], { cwd: owner.worktreePath });
    await execFileAsync("git", ["commit", "-m", "remote graph visual proof"], { cwd: owner.worktreePath });

    const remoteItemId = await waitForRemoteTask(owner.taskId);
    await selectRemoteTask(remoteItemId, owner.taskId);

    await secondary.executeSync(buildGlobalKeydownScript({ key: "g", meta: true }));
    await secondary.waitForElement(".graph-modal", 30_000);
    await secondary.waitForText(".graph-modal", "remote graph visual proof", 30_000);
    await capture("remote-graph.png");

    await secondary.click(await secondary.waitForElement('[data-testid="main-tab-close-graph"]', 10_000));
    await secondary.waitForNoElement(".graph-modal", 10_000);
    await secondary.executeSync(buildGlobalKeydownScript({ key: "o", meta: true }));
    await secondary.waitForText(".toast.warning .toast-message", "This action is not available for a task on another machine.", 10_000);
    // tauri-plugin-webdriver does not advance this TransitionGroup's CSS enter
    // frame. Preserve the actual toast message, but settle the presentation
    // class so the native screenshot records the refusal rather than opacity 0.
    await secondary.executeSync(`
      const toast = Array.from(document.querySelectorAll('.toast.warning')).find((element) =>
        element.textContent?.includes("This action is not available for a task on another machine."));
      if (!(toast instanceof HTMLElement)) throw new Error("remote-action refusal toast disappeared before capture");
      toast.classList.remove("toast-enter-from", "toast-enter-active");
    `);
    await capture("remote-local-action-refusal.png");

    await secondary.executeSync(buildGlobalKeydownScript({ key: "p", meta: true }));
    await secondary.waitForText(".toast.warning .toast-message", "This action is not available for a task on another machine.", 10_000);
    await secondary.waitForNoElement(".picker-modal", 10_000);
    await capture("remote-file-picker-refusal.png");

    await secondary.executeSync(buildGlobalKeydownScript({ key: "j", meta: true, shift: true }));
    await secondary.waitForText(".toast.warning .toast-message", "Shell is only available for local tasks.", 10_000);
    await secondary.waitForNoElement(".shell-modal", 10_000);
    await capture("remote-repo-shell-refusal.png");

    const identity = await secondary.executeSync(`
      const diagnostics = window.__KANNA_E2E__?.setupState?.remoteTaskDiagnostics;
      const value = diagnostics?.__v_isRef ? diagnostics.value : diagnostics;
      return (value || []).find((entry) => entry.itemId === ${JSON.stringify(remoteItemId)}) || null;
    `) as { ownerDesktopId?: string; ownerLocalTaskId?: string; selectedTerminalTransport?: string } | null;
    expect(identity).toMatchObject({
      ownerDesktopId,
      ownerLocalTaskId: owner.taskId,
      selectedTerminalTransport: "cloud",
    });
  }, 180_000);
});
