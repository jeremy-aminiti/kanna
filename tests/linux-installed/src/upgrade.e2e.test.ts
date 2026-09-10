import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createFixtureRepo, type FixtureRepo } from "@kanna/headless-worker/src/fixtureRepo.ts";
import { DEVELOPER_TOOLS, installedPaths } from "./installedTree.ts";
import {
  InstalledWorker,
  inspectHost,
  installPackage,
  installedPackageVersion,
  processIsAlive,
  processStartTime,
  run,
  waitFor,
  type HostCapability,
} from "./installedWorker.ts";

/**
 * The installed live-session upgrade, which Phase 1 deliberately deferred to
 * this phase and which nothing else in the repository covers.
 *
 * Phase 1 chose a byte-exact executable-path identity rule for daemon handoff
 * with no `(deleted)` tolerance, on the argument that a package replaces
 * binaries by rename-into-place and the operator then restarts the launcher —
 * so a fresh launcher always exists at the same path, and a `(deleted)`
 * inode never has to be trusted. It recorded that the argument was untested
 * against a real installation. This is that test.
 *
 * The sequence is the argument, in order:
 *
 * 1. Version A is installed and running, with a live agent session in a
 *    daemon-owned PTY.
 * 2. Version B is installed *while that session is live*. apt replaces the
 *    files. Nothing restarts. The daemon and the agent must both still be the
 *    same processes — a package that killed them here would destroy an
 *    operator's work during an unattended `apt upgrade`.
 * 3. The operator restarts the unit. `KillMode=process` stops only the
 *    supervisor; the daemon survives and the new supervisor re-adopts it.
 * 4. The agent is the *same process* — pid and start time both — the task and
 *    its session are the same, and an input delivered afterwards is submitted
 *    exactly once and recorded durably.
 *
 * The last one is what makes it an upgrade rather than a restart: terminal
 * bytes are not a record, so the proof is the `task_input` row and the run's
 * durable events, never a screenful of the right characters.
 */

const OLD_DEB = process.env.KANNA_INSTALLED_DEB_OLD;
const NEW_DEB = process.env.KANNA_INSTALLED_DEB_NEW;
const CHANNEL = (process.env.KANNA_INSTALLED_CHANNEL ?? "production") as "production" | "staging";

/** One line, no newline, at the length of the 2026-09-06 incident. Delivered
 *  after the upgrade, so the framing guarantee is re-proven on the new runtime. */
const POST_UPGRADE_MESSAGE = `AFTER-UPGRADE ${"y".repeat(1_047 - 14 - 25)}and this is the tail only`;

let host: HostCapability;
let repo: FixtureRepo;
let worker: InstalledWorker | null = null;
let taskId = "";
let runId = "";
let taskBranch: string | null = null;
let taskWorktree: string | null = null;
let agentPid = 0;
let agentStart: string | null = null;
let daemonBefore = 0;
let versionA = "";
const paths = installedPaths(CHANNEL);

interface TaskDetail {
  id: string;
  runtimeState: string;
  branch: string | null;
  worktreePath: string | null;
  latestRun: { id: string; status: string; summary: string | null };
}

beforeAll(async () => {
  host = await inspectHost(DEVELOPER_TOOLS);
}, 120_000);

afterAll(async () => {
  await worker?.stop();
});

function requireHost(): void {
  if (!OLD_DEB || !NEW_DEB) {
    throw new Error(
      "KANNA_INSTALLED_DEB_OLD and KANNA_INSTALLED_DEB_NEW must both be set: this lane upgrades between two real packages."
    );
  }
  if (!host.usable) throw new Error(`this host cannot run the installed lane: ${host.reason}`);
}

/** The agent process inside the task's PTY, found through the daemon's own
 *  descendant tree rather than by matching a command line. */
async function agentPidForDaemon(daemonPid: number): Promise<number> {
  const listing = await run("ps", ["-eo", "pid,ppid,args"]);
  const rows = listing.stdout
    .split("\n")
    .map((line) => line.trim().split(/\s+/))
    .filter((fields) => fields.length > 2);
  const queue = [daemonPid];
  const seen = new Set<number>();
  while (queue.length > 0) {
    const parent = queue.shift() as number;
    for (const fields of rows) {
      const pid = Number.parseInt(fields[0] as string, 10);
      if (Number.parseInt(fields[1] as string, 10) !== parent || seen.has(pid)) continue;
      seen.add(pid);
      // The scripted provider is exec'd by the PTY's login shell, so the agent
      // is a descendant rather than a direct child.
      if (fields.slice(2).join(" ").includes("claude")) return pid;
      queue.push(pid);
    }
  }
  throw new Error(`no agent process under daemon ${daemonPid}:\n${listing.stdout}`);
}

describe("an installed upgrade with a live agent session", () => {
  it("installs version A and runs a task in it", async () => {
    requireHost();
    expect((await installPackage(OLD_DEB as string)).code).toBe(0);
    versionA = (await installedPackageVersion(paths.packageName)) as string;
    expect(versionA).toBeTruthy();

    repo = await createFixtureRepo();
    worker = await InstalledWorker.start({ channel: CHANNEL, providerBinDir: repo.providerBinDir });

    const added = await worker.cli(["repo", "add", "--path", repo.path]);
    expect(added.code, `${added.stdout}${added.stderr}`).toBe(0);
    const addedRepo = JSON.parse(added.stdout);
    expect(addedRepo.path).toBe(repo.path);
    expect(addedRepo.id).toEqual(expect.any(String));
    expect(addedRepo.id.length).toBeGreaterThan(0);
    const repoId = addedRepo.id;
    const created = await worker.cli([
      "task", "create", "--repo-id", repoId, "--prompt", "installed upgrade task", "--workflow-name", "gate",
    ]);
    expect(created.code, created.stderr).toBe(0);
    const createdTask = JSON.parse(created.stdout);
    expect(createdTask.repoId).toBe(repoId);
    expect(createdTask.taskId).toMatch(/^[a-f0-9]{8,64}$/);
    taskId = createdTask.taskId;

    await waitFor(
      async () => (await worker!.cli(["task", "logs", "--task-id", taskId])).stdout.includes("SCRIPT_READY"),
      "the scripted agent never announced itself before the upgrade"
    );

    daemonBefore = await worker.daemonPid();
    agentPid = await agentPidForDaemon(daemonBefore);
    agentStart = await processStartTime(agentPid);
    expect(agentStart).toBeTruthy();
    const detail = await worker.json<TaskDetail>(`/v1/tasks/${taskId}`);
    expect(detail.id).toBe(taskId);
    expect(detail.latestRun.status).toBe("running");
    runId = detail.latestRun.id;
    taskBranch = detail.branch;
    taskWorktree = detail.worktreePath;
    expect(runId).toBeTruthy();
    expect(taskBranch).toBeTruthy();
    expect(taskWorktree).toBeTruthy();
  });

  /**
   * The property an unattended `apt upgrade` depends on. Replacing files must
   * not touch a running daemon or the agent sessions it owns; a maintainer
   * script that stopped the daemon here would kill an operator's work with no
   * warning.
   */
  it("installs version B underneath the live session without disturbing it", async () => {
    requireHost();
    expect((await installPackage(NEW_DEB as string)).code).toBe(0);
    const versionB = await installedPackageVersion(paths.packageName);
    expect(versionB).not.toBe(versionA);

    expect(processIsAlive(daemonBefore)).toBe(true);
    expect(processIsAlive(agentPid)).toBe(true);
    expect(await processStartTime(agentPid)).toBe(agentStart);
    expect((await worker!.status())?.state).toBe("running");
  });

  /**
   * The restart is what applies the new runtime, and the identity rule is what
   * makes it safe: the new supervisor is a fresh, readable executable at the
   * same installed path, so the daemon's launcher-identity recheck passes
   * without ever having to tolerate a `(deleted)` inode.
   */
  it("survives the operator's unit restart with the same agent process", async () => {
    requireHost();
    const supervisorBefore = await worker!.supervisorPid();
    await worker!.restartUnit();
    const supervisorAfter = await worker!.supervisorPid();
    expect(supervisorAfter).not.toBe(supervisorBefore);

    // A pid alone is not identity — pids are reused. The pair is, which is the
    // same pairing the daemon's own authorizer uses.
    expect(processIsAlive(agentPid)).toBe(true);
    expect(await processStartTime(agentPid)).toBe(agentStart);

    const detail = await worker!.json<TaskDetail>(`/v1/tasks/${taskId}`);
    expect(detail.id).toBe(taskId);
    expect(detail.latestRun.id).toBe(runId);
    expect(detail.latestRun.status).toBe("running");
    expect(detail.branch).toBe(taskBranch);
    expect(detail.worktreePath).toBe(taskWorktree);
    expect(["busy", "idle", "waiting"]).toContain(detail.runtimeState);
  });

  it("re-adopts the surviving daemon rather than replacing it", async () => {
    requireHost();
    expect(await worker!.daemonPid()).toBe(daemonBefore);
    expect(processIsAlive(daemonBefore)).toBe(true);
  });

  /**
   * The assertion that makes this an upgrade proof rather than a liveness
   * check. Terminal bytes are not a record: the session has to still accept a
   * delivered message, submit it once, and leave a durable row a later stage
   * could read.
   */
  it("delivers input after the upgrade, whole, once, and durably", async () => {
    requireHost();
    const sent = await worker!.cli([
      "task", "send-input", "--task-id", taskId, "--message", POST_UPGRADE_MESSAGE, "--source", "operator",
    ]);
    expect(sent.code, sent.stderr).toBe(0);

    const ledger = await worker!.json<{
      taskId: string; total: number;
      inputs: Array<{ taskId: string; runId: string | null; message: string; source: string }>;
    }>(`/v1/tasks/${taskId}/inputs`);
    expect(ledger.taskId).toBe(taskId);
    expect(ledger.total).toBe(1);
    expect(ledger.inputs).toHaveLength(1);
    expect(ledger.inputs[0]).toMatchObject({ taskId, runId, message: POST_UPGRADE_MESSAGE, source: "operator" });

    await waitFor(
      async () => (await repo.agentInput()).includes(POST_UPGRADE_MESSAGE),
      async () => `the upgraded session never received the message whole: ${JSON.stringify(await repo.agentInput())}`
    );
    expect((await repo.agentInput()).filter((line) => line === POST_UPGRADE_MESSAGE)).toHaveLength(1);
  });

  it("records completion on the upgraded runtime", async () => {
    requireHost();
    const completed = await worker!.cli([
      "stage-complete", "--task-id", taskId, "--status", "success", "--summary", "survived the installed upgrade",
    ]);
    expect(completed.code, completed.stderr).toBe(0);

    const detail = await worker!.json<TaskDetail>(`/v1/tasks/${taskId}`);
    expect(detail.latestRun).toMatchObject({ id: runId, status: "succeeded", summary: "survived the installed upgrade" });
    const batch = await worker!.json<{
      events: Array<{ type: string; taskId: string; payload: { runId?: string; status?: string } }>;
    }>(`/v1/task-events?taskIds=${taskId}&eventTypes=run.finished&localOnly=true&timeoutSecs=0`);
    expect(batch.events).toContainEqual(expect.objectContaining({
      type: "run.finished", taskId, payload: expect.objectContaining({ runId, status: "succeeded" }),
    }));
  });
});
