import { setTimeout as sleep } from "node:timers/promises";
import { execFile } from "node:child_process";
import { realpath, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { promisify } from "node:util";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { cleanupFixtureRepos, createFixtureRepo } from "../helpers/fixture-repo";
import { cleanupWorktrees, importTestRepo, resetDatabase } from "../helpers/reset";
import { pauseForSlowMode } from "../helpers/slowMode";
import { createPrimaryAndSecondaryClients } from "../helpers/twoInstance";
import { pairWithPeerThroughUi, pushSelectedTaskToPeerThroughUi } from "../helpers/transferFlow";
import { callVueMethod, execDb, queryDb, tauriInvoke } from "../helpers/vue";

interface TransferPeer {
  peer_id?: string;
  peerId?: string;
}

interface TransferRow {
  id: string;
  direction: string;
  status: string;
  source_peer_id: string | null;
  source_task_id: string | null;
  local_task_id: string | null;
  payload_json?: string | null;
  error?: string | null;
}

interface PipelineRow {
  id: string;
  stage: string;
  closed_at: string | null;
}

interface RepoRow {
  path: string;
}

interface VueCallError {
  __error: string;
}

let testRepoPath = "";
const execFileAsync = promisify(execFile);

const { primary, secondary } = createPrimaryAndSecondaryClients();

function isVueCallError(value: unknown): value is VueCallError {
  return Boolean(
    value &&
    typeof value === "object" &&
    "__error" in value &&
    typeof (value as VueCallError).__error === "string",
  );
}

function readPeerId(peer: TransferPeer): string | null {
  if (typeof peer.peer_id === "string" && peer.peer_id.length > 0) return peer.peer_id;
  if (typeof peer.peerId === "string" && peer.peerId.length > 0) return peer.peerId;
  return null;
}

async function waitForPeer(peerId: string, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    const raw = await tauriInvoke(primary, "list_transfer_peers");
    if (Array.isArray(raw) && raw.some((peer) => readPeerId(peer as TransferPeer) === peerId)) {
      return;
    }
    await sleep(250);
  }

  throw new Error(`timed out waiting for peer ${peerId}`);
}

async function waitForLatestTransfer(
  client: typeof primary,
  direction: "incoming" | "outgoing",
  sourceTaskId: string,
  expectedStatus: "pending" | "completed",
  timeoutMs = 20_000,
): Promise<TransferRow> {
  const deadline = Date.now() + timeoutMs;
  let last: TransferRow | undefined;

  while (Date.now() < deadline) {
    const rows = (await queryDb(
      client,
      `SELECT id, direction, status, source_peer_id, source_task_id, local_task_id, payload_json, error
         FROM task_transfer
        WHERE direction = ? AND source_task_id = ?
        ORDER BY started_at DESC
        LIMIT 1`,
      [direction, sourceTaskId],
    )) as TransferRow[];
    const row = rows[0];
    last = row;
    if (row?.status === expectedStatus) {
      return row;
    }
    await sleep(250);
  }

  throw new Error(
    `timed out waiting for ${direction} transfer ${expectedStatus} for ${sourceTaskId}: ${JSON.stringify(last)}`,
  );
}

async function waitForPrimaryTaskClosed(taskId: string, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    const rows = (await queryDb(
      primary,
      "SELECT id, stage, closed_at FROM pipeline_item WHERE id = ?",
      [taskId],
    )) as PipelineRow[];
    const row = rows[0];
    // closed_at is the sole done indicator — closing never rewrites stage.
    if (typeof row?.closed_at === "string" && row.closed_at.length > 0) {
      return;
    }
    await sleep(250);
  }

  throw new Error(`timed out waiting for source task ${taskId} to close`);
}

async function deleteSessionIfRunning(client: { deleteSession(): Promise<void> }): Promise<void> {
  await client.deleteSession().catch(() => undefined);
}

async function createSourceTask(repoId: string, repoPath: string, prompt: string): Promise<string> {
  // Direct task creation is setup-only: the product has no UI path for creating an inert
  // transfer fixture task without also launching a real agent session.
  const createResult = await callVueMethod(
    primary,
    "store.createItem",
    repoId,
    repoPath,
    prompt,
    "agent",
    { workflowName: "single-reviewer", agentProvider: "codex" },
  );
  if (isVueCallError(createResult)) {
    throw new Error(createResult.__error);
  }

  const rows = (await queryDb(
    primary,
    "SELECT id FROM pipeline_item WHERE prompt = ? ORDER BY created_at DESC LIMIT 1",
    [prompt],
  )) as Array<{ id: string }>;
  const sourceTaskId = rows[0]?.id;
  if (!sourceTaskId) {
    throw new Error(`task not found for prompt ${prompt}`);
  }
  await callVueMethod(primary, "store.selectItem", sourceTaskId);
  return sourceTaskId;
}

async function pushAndApproveTransfer(sourceTaskId: string): Promise<TransferRow> {
  await pushSelectedTaskToPeerThroughUi(primary, "Secondary");

  return waitForLatestTransfer(secondary, "incoming", sourceTaskId, "completed");
}

describe("local transfer repo acquisition", () => {
  beforeAll(async () => {
    await primary.createSession();
    await secondary.createSession();
  });

  beforeEach(async () => {
    await resetDatabase(primary);
    await resetDatabase(secondary);
    await waitForPeer("peer-secondary");
    await pairWithPeerThroughUi(primary, "Secondary", "peer-secondary", {
      promptClient: secondary,
      promptPeerId: "peer-primary",
    });
  });

  afterEach(async () => {
    if (testRepoPath) {
      await cleanupWorktrees(primary, testRepoPath).catch(() => undefined);
      await cleanupWorktrees(secondary, testRepoPath).catch(() => undefined);
      await cleanupFixtureRepos([testRepoPath]).catch(() => undefined);
      testRepoPath = "";
    }
  });

  afterAll(async () => {
    await deleteSessionIfRunning(primary);
    await deleteSessionIfRunning(secondary);
  });

  it("imports unpublished multi-stage work and delivered inputs into an existing independent clone", async () => {
    testRepoPath = await createFixtureRepo("local-transfer-reuse-local");
    const repoId = await importTestRepo(primary, testRepoPath, "local-transfer-reuse-primary");
    const originPath = join(
      dirname(testRepoPath),
      `${basename(testRepoPath)}-origin.git`,
    );
    const secondaryRepoPath = join(dirname(testRepoPath), "secondary-existing-clone");
    await execFileAsync("git", ["clone", originPath, secondaryRepoPath]);
    await importTestRepo(secondary, secondaryRepoPath, "local-transfer-reuse-secondary");
    await pauseForSlowMode("reuse-local fixture imported into both instances");

    const sourceTaskId = await createSourceTask(repoId, testRepoPath, "Reuse repo on destination");
    const sourceRows = (await queryDb(
      primary,
      `SELECT pipeline_item.branch, worktree.path
         FROM pipeline_item
         JOIN worktree ON worktree.pipeline_item_id = pipeline_item.id
        WHERE pipeline_item.id = ?
        ORDER BY worktree.created_at DESC
        LIMIT 1`,
      [sourceTaskId],
    )) as Array<{ branch: string; path: string }>;
    const source = sourceRows[0];
    if (!source) throw new Error("source task has no worktree");

    await writeFile(join(source.path, "stage-one.txt"), "unpublished stage one\n");
    await execFileAsync("git", ["add", "stage-one.txt"], { cwd: source.path });
    await execFileAsync("git", ["commit", "-m", "test: unpublished in-progress work"], {
      cwd: source.path,
    });
    await writeFile(join(source.path, "stage-two.txt"), "unpublished review work\n");
    await execFileAsync("git", ["add", "stage-two.txt"], { cwd: source.path });
    await execFileAsync("git", ["commit", "-m", "test: unpublished review work"], {
      cwd: source.path,
    });
    const { stdout: headOutput } = await execFileAsync("git", ["rev-parse", "HEAD"], {
      cwd: source.path,
    });
    const sourceHead = headOutput.trim();
    await expect(
      execFileAsync("git", ["cat-file", "-e", `${sourceHead}^{commit}`], {
        cwd: secondaryRepoPath,
      }),
    ).rejects.toThrow();

    // These are the rows produced by successful logical deliveries. Seeding
    // them directly keeps this repository-acquisition boundary independent of
    // the separate PTY submission/finalization fixture.
    await execDb(
      primary,
      `INSERT INTO task_input (task_id, run_id, stage, source, message, delivered_at)
       VALUES (?, NULL, 'in progress', 'operator', ?, '2026-09-09 10:00:00'),
              (?, NULL, 'review', 'manager', ?, '2026-09-09 10:00:01')`,
      [
        sourceTaskId,
        "Preserve the unpublished implementation exactly.",
        sourceTaskId,
        "Review the second-stage commit as the current tip.",
      ],
    );
    await execDb(primary, "UPDATE pipeline_item SET stage = 'review' WHERE id = ?", [sourceTaskId]);

    const incomingTransfer = await pushAndApproveTransfer(sourceTaskId);
    expect(incomingTransfer.local_task_id).toBeTruthy();

    const repoRows = (await queryDb(
      secondary,
      `SELECT repo.path
         FROM repo
         JOIN pipeline_item ON pipeline_item.repo_id = repo.id
        WHERE pipeline_item.id = ?`,
      [incomingTransfer.local_task_id],
    )) as RepoRow[];
    expect(await realpath(repoRows[0]!.path)).toBe(await realpath(secondaryRepoPath));

    const destinationRows = (await queryDb(
      secondary,
      `SELECT pipeline_item.branch, pipeline_item.stage, worktree.path
         FROM pipeline_item
         JOIN worktree ON worktree.pipeline_item_id = pipeline_item.id
        WHERE pipeline_item.id = ?
        ORDER BY worktree.created_at DESC
        LIMIT 1`,
      [incomingTransfer.local_task_id],
    )) as Array<{ branch: string; stage: string; path: string }>;
    expect(destinationRows[0]?.stage).toBe("review");
    const { stdout: destinationHeadOutput } = await execFileAsync("git", ["rev-parse", "HEAD"], {
      cwd: destinationRows[0]!.path,
    });
    expect(destinationHeadOutput.trim()).toBe(sourceHead);
    const { stdout: historyOutput } = await execFileAsync("git", ["log", "-2", "--format=%s"], {
      cwd: destinationRows[0]!.path,
    });
    expect(historyOutput).toContain("test: unpublished in-progress work");
    expect(historyOutput).toContain("test: unpublished review work");

    const importedInputs = await queryDb(
      secondary,
      `SELECT source, stage, message, delivered_at, origin_peer_id, origin_task_id,
              origin_input_id, origin_run_id
         FROM task_input
        WHERE task_id = ?
        ORDER BY id`,
      [incomingTransfer.local_task_id],
    );
    expect(importedInputs).toEqual([
      expect.objectContaining({
        source: "operator",
        stage: "in progress",
        message: "Preserve the unpublished implementation exactly.",
        delivered_at: "2026-09-09 10:00:00",
        origin_peer_id: "peer-primary",
        origin_task_id: sourceTaskId,
        origin_input_id: expect.any(Number),
        origin_run_id: null,
      }),
      expect.objectContaining({
        source: "manager",
        stage: "review",
        message: "Review the second-stage commit as the current tip.",
        delivered_at: "2026-09-09 10:00:01",
        origin_peer_id: "peer-primary",
        origin_task_id: sourceTaskId,
        origin_input_id: expect.any(Number),
        origin_run_id: null,
      }),
    ]);

    const outgoingTransfer = await waitForLatestTransfer(primary, "outgoing", sourceTaskId, "completed");
    expect(outgoingTransfer.status).toBe("completed");
    const outgoingPayload = JSON.parse(outgoingTransfer.payload_json ?? "{}") as {
      task?: { head_oid?: string };
      repo?: { mode?: string };
      input_ledger?: { count?: number };
    };
    expect(outgoingPayload).toMatchObject({
      task: { head_oid: sourceHead },
      repo: { mode: "task-bundle" },
      input_ledger: { count: 2 },
    });

    await waitForPrimaryTaskClosed(sourceTaskId);
    const { stdout: retainedBranchHead } = await execFileAsync(
      "git",
      ["rev-parse", `refs/heads/${source.branch}`],
      { cwd: testRepoPath },
    );
    expect(retainedBranchHead.trim()).toBe(sourceHead);
  });

  it("restores a task bundle into ~/.kanna/repos when secondary has no matching repo", async () => {
    testRepoPath = await createFixtureRepo("local-transfer-clone-remote");
    const repoId = await importTestRepo(primary, testRepoPath, "local-transfer-clone-remote");
    await pauseForSlowMode("clone-remote fixture imported into primary");

    const sourceTaskId = await createSourceTask(repoId, testRepoPath, "Clone repo on destination");
    const incomingTransfer = await pushAndApproveTransfer(sourceTaskId);
    expect(incomingTransfer.local_task_id).toBeTruthy();

    const repoRows = (await queryDb(
      secondary,
      `SELECT repo.path
         FROM repo
         JOIN pipeline_item ON pipeline_item.repo_id = repo.id
        WHERE pipeline_item.id = ?`,
      [incomingTransfer.local_task_id],
    )) as RepoRow[];
    const importedRepoPath = repoRows[0]?.path;
    expect(importedRepoPath).toBeTruthy();
    expect(importedRepoPath).not.toBe(testRepoPath);
    expect(importedRepoPath).toContain(`${homedir()}/.kanna/repos/local-transfer-clone-remote`);

    const outgoingTransfer = await waitForLatestTransfer(primary, "outgoing", sourceTaskId, "completed");
    expect(outgoingTransfer.payload_json).toBeTruthy();
    const outgoingPayload = JSON.parse(outgoingTransfer.payload_json ?? "{}") as {
      repo?: { mode?: string; remote_url?: string | null };
    };
    expect(outgoingPayload.repo).toMatchObject({
      mode: "task-bundle",
    });
    expect(typeof outgoingPayload.repo?.remote_url).toBe("string");

    await waitForPrimaryTaskClosed(sourceTaskId);
  });

  it("transfers a bundle into ~/.kanna/repos on secondary when no origin remote exists", async () => {
    testRepoPath = await createFixtureRepo("local-transfer-bundle-repo");
    const removeOrigin = await tauriInvoke(primary, "run_script", {
      script: "git remote remove origin",
      cwd: testRepoPath,
      env: {},
    });
    if (isVueCallError(removeOrigin)) {
      throw new Error(removeOrigin.__error);
    }

    const repoId = await importTestRepo(primary, testRepoPath, "local-transfer-bundle-repo");
    await pauseForSlowMode("bundle fixture imported into primary");

    const sourceTaskId = await createSourceTask(repoId, testRepoPath, "Bundle repo on destination");
    await pushSelectedTaskToPeerThroughUi(primary, "Secondary");

    const incomingTransfer = await waitForLatestTransfer(secondary, "incoming", sourceTaskId, "completed");
    const outgoingTransfer = await waitForLatestTransfer(primary, "outgoing", sourceTaskId, "completed");
    const outgoingPayload = JSON.parse(outgoingTransfer.payload_json ?? "{}") as {
      repo?: {
        mode?: string;
        bundle?: {
          artifact_id?: string;
          filename?: string;
        } | null;
      };
    };
    expect(outgoingPayload.repo).toMatchObject({
      mode: "task-bundle",
    });
    expect(outgoingPayload.repo?.bundle?.artifact_id).toBeTruthy();
    expect(outgoingPayload.repo?.bundle?.filename).toContain(".bundle");
    expect(incomingTransfer.local_task_id).toBeTruthy();

    const repoRows = (await queryDb(
      secondary,
      `SELECT repo.path
         FROM repo
         JOIN pipeline_item ON pipeline_item.repo_id = repo.id
        WHERE pipeline_item.id = ?`,
      [incomingTransfer.local_task_id],
    )) as RepoRow[];
    const importedRepoPath = repoRows[0]?.path;
    expect(importedRepoPath).toBeTruthy();
    expect(importedRepoPath).not.toBe(testRepoPath);
    expect(importedRepoPath).toContain(`${homedir()}/.kanna/repos/local-transfer-bundle-repo`);

    await waitForPrimaryTaskClosed(sourceTaskId);
  });
});
