import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { basename, join } from "node:path";
import { promisify } from "node:util";

import {
  formatAppWindowTitle,
  type AppBuildInfo,
} from "../../../src/stores/windowTitle";

const execFileAsync = promisify(execFile);

export interface NativeWindowIdentityClient {
  getAppBuildInfo(): Promise<unknown>;
  getBaseUrl(): string;
  getNativeWindowTitle(): Promise<string>;
}

export interface ExpectedNativeWindowIdentity extends AppBuildInfo {
  nativeTitle: string;
}

export interface ExpectedNativeWindowIdentityInput {
  branch: string;
  commitHash: string;
  env: NodeJS.ProcessEnv;
  repoRoot: string;
  version: string;
}

interface RawAppBuildInfo {
  version?: unknown;
  branch?: unknown;
  commitHash?: unknown;
  commit_hash?: unknown;
  taskId?: unknown;
  task_id?: unknown;
  worktree?: unknown;
}

function requiredString(value: unknown, field: string): string {
  if (typeof value !== "string") {
    throw new Error(`get_app_build_info returned invalid ${field}: ${JSON.stringify(value)}`);
  }
  return value;
}

/** Mirrors the frontend's normalization of the native Rust command wire. */
export function normalizeAppBuildInfo(raw: unknown): AppBuildInfo {
  if (!raw || typeof raw !== "object") {
    throw new Error(`get_app_build_info returned invalid data: ${JSON.stringify(raw)}`);
  }
  const value = raw as RawAppBuildInfo;
  return {
    version: requiredString(value.version, "version"),
    branch: requiredString(value.branch, "branch"),
    commitHash: requiredString(value.commitHash ?? value.commit_hash, "commit_hash"),
    taskId: requiredString(value.taskId ?? value.task_id, "task_id"),
    worktree: requiredString(value.worktree, "worktree"),
  };
}

function deriveTaskIdFromWorktree(worktree: string): string {
  const match = /^task-(.+?)(?:-\d+)?$/.exec(worktree);
  return match?.[1] ?? "";
}

function isTaskWorktree(repoRoot: string, env: NodeJS.ProcessEnv): boolean {
  return env.KANNA_WORKTREE === "1" || repoRoot.includes("/.kanna-worktrees/");
}

async function readGit(repoRoot: string, args: string[]): Promise<string> {
  const { stdout } = await execFileAsync("git", args, { cwd: repoRoot });
  return stdout.trim();
}

/**
 * Derives the expected build stamp from the same repository context `kd` uses,
 * independently of whichever native window happens to answer WebDriver.
 */
export async function resolveExpectedNativeWindowIdentity(
  repoRoot: string,
  env: NodeJS.ProcessEnv = process.env,
): Promise<ExpectedNativeWindowIdentity> {
  const [branch, commitHash, tauriConfigText] = await Promise.all([
    readGit(repoRoot, ["rev-parse", "--abbrev-ref", "HEAD"]),
    readGit(repoRoot, ["rev-parse", "--short", "HEAD"]),
    readFile(join(repoRoot, "apps", "desktop", "src-tauri", "tauri.conf.json"), "utf8"),
  ]);
  const tauriConfig = JSON.parse(tauriConfigText) as { version?: unknown };
  return buildExpectedNativeWindowIdentity({
    branch,
    commitHash,
    env,
    repoRoot,
    version: requiredString(tauriConfig.version, "configured version"),
  });
}

export function buildExpectedNativeWindowIdentity(
  input: ExpectedNativeWindowIdentityInput,
): ExpectedNativeWindowIdentity {
  const inTaskWorktree = isTaskWorktree(input.repoRoot, input.env);
  const worktree = inTaskWorktree ? basename(input.repoRoot) : "";
  const taskId = inTaskWorktree
    ? input.env.KANNA_TASK_ID?.trim() || deriveTaskIdFromWorktree(worktree)
    : "";

  if (inTaskWorktree && (!taskId || !new RegExp(`^task-${escapeRegExp(taskId)}(?:-\\d+)?$`).test(worktree))) {
    throw new Error(
      `cannot derive a coherent task window identity from task ${JSON.stringify(taskId)} and worktree ${JSON.stringify(worktree)}`,
    );
  }

  const buildInfo: AppBuildInfo = {
    version: input.version,
    branch: input.branch,
    commitHash: input.commitHash,
    taskId,
    worktree,
  };
  const nativeTitle = formatAppWindowTitle(buildInfo);
  if (!nativeTitle) {
    throw new Error(
      `desktop E2E requires an explicit dev window identity; ${input.branch || "detached"} at ${input.commitHash} has no task, worktree, or non-default branch title`,
    );
  }
  return { ...buildInfo, nativeTitle };
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export async function assertNativeWindowIdentity(
  client: NativeWindowIdentityClient,
  expected: ExpectedNativeWindowIdentity,
  label: string,
): Promise<void> {
  const rawBuildInfo = await client.getAppBuildInfo();
  const actual = normalizeAppBuildInfo(rawBuildInfo);
  const fields = ["version", "branch", "commitHash", "taskId", "worktree"] as const;
  for (const field of fields) {
    if ((actual[field] ?? "") !== (expected[field] ?? "")) {
      throw new Error(
        `${label} WebDriver identity mismatch at ${client.getBaseUrl()}: ${field} expected ${JSON.stringify(expected[field] ?? "")}, got ${JSON.stringify(actual[field] ?? "")}`,
      );
    }
  }

  const actualTitle = await client.getNativeWindowTitle();
  if (actualTitle !== expected.nativeTitle) {
    throw new Error(
      `${label} native window title mismatch at ${client.getBaseUrl()}: expected ${JSON.stringify(expected.nativeTitle)}, got ${JSON.stringify(actualTitle)}`,
    );
  }
  console.log(
    `[e2e] verified ${label} native identity at ${client.getBaseUrl()}: buildInfo=${JSON.stringify(rawBuildInfo)} nativeTitle=${JSON.stringify(actualTitle)}`,
  );
}
