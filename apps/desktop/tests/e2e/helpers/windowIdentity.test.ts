import { describe, expect, it, vi } from "vitest";

import {
  assertNativeWindowIdentity,
  buildExpectedNativeWindowIdentity,
  normalizeAppBuildInfo,
  type ExpectedNativeWindowIdentity,
} from "./windowIdentity";

const expected: ExpectedNativeWindowIdentity = {
  version: "0.0.68",
  branch: "task-8ea2e5aa-3",
  commitHash: "c7f89c5",
  taskId: "8ea2e5aa",
  worktree: "task-8ea2e5aa-3",
  nativeTitle: "Kanna — task 8ea2e5aa · task-8ea2e5aa-3 (0.0.68 @ c7f89c5)",
};

function client(overrides: { buildInfo?: unknown; title?: string; endpoint?: string } = {}) {
  return {
    getAppBuildInfo: vi.fn(async () => overrides.buildInfo ?? {
      version: expected.version,
      branch: expected.branch,
      commit_hash: expected.commitHash,
      task_id: expected.taskId,
      worktree: expected.worktree,
    }),
    getBaseUrl: vi.fn(() => overrides.endpoint ?? "http://127.0.0.1:4445"),
    getNativeWindowTitle: vi.fn(async () => overrides.title ?? expected.nativeTitle),
  };
}

describe("native desktop E2E identity", () => {
  it("normalizes the native snake_case wire and the frontend-compatible camelCase shape", () => {
    expect(normalizeAppBuildInfo({
      version: "0.0.68", branch: "task-a", commit_hash: "abc1234", task_id: "a", worktree: "task-a",
    })).toEqual({
      version: "0.0.68", branch: "task-a", commitHash: "abc1234", taskId: "a", worktree: "task-a",
    });
    expect(normalizeAppBuildInfo({
      version: "0.0.68", branch: "task-b", commitHash: "def5678", taskId: "b", worktree: "task-b",
    })).toEqual({
      version: "0.0.68", branch: "task-b", commitHash: "def5678", taskId: "b", worktree: "task-b",
    });
  });

  it("keeps long durable task ids intact across stage-fork worktrees", () => {
    const taskId = "dfd72f7aeacaaeb08e77e99fb5087638766895d6c539117ece361c65fe8d73aa";
    const identity = buildExpectedNativeWindowIdentity({
      branch: `task-${taskId}-12`,
      commitHash: "7785755",
      env: { KANNA_TASK_ID: taskId },
      repoRoot: `/repo/.kanna-worktrees/task-${taskId}-12`,
      version: "0.0.68",
    });

    expect(identity.taskId).toBe(taskId);
    expect(identity.worktree).toBe(`task-${taskId}-12`);
    expect(identity.nativeTitle).toContain(`task ${taskId} · task-${taskId}-12`);
  });

  it("supports a non-task dev checkout only when its repo identity yields an explicit title", () => {
    expect(buildExpectedNativeWindowIdentity({
      branch: "fix/window-guard",
      commitHash: "abc1234",
      env: {},
      repoRoot: "/repo/kanna",
      version: "0.0.68",
    })).toMatchObject({
      taskId: "",
      worktree: "",
      nativeTitle: "Kanna — fix/window-guard (0.0.68 @ abc1234)",
    });

    expect(() => buildExpectedNativeWindowIdentity({
      branch: "main",
      commitHash: "abc1234",
      env: {},
      repoRoot: "/repo/kanna",
      version: "0.0.68",
    })).toThrow("requires an explicit dev window identity");
  });

  it.each([
    ["wrong endpoint", { endpoint: "http://127.0.0.1:4446", buildInfo: { version: "0.0.68", branch: "main", commit_hash: "production", task_id: "", worktree: "" } }, "http://127.0.0.1:4446"],
    ["wrong task", { buildInfo: { version: "0.0.68", branch: expected.branch, commit_hash: expected.commitHash, task_id: "another", worktree: expected.worktree } }, "taskId"],
    ["wrong worktree", { buildInfo: { version: "0.0.68", branch: expected.branch, commit_hash: expected.commitHash, task_id: expected.taskId, worktree: "task-8ea2e5aa-4" } }, "worktree"],
    ["wrong native title", { title: "Kanna" }, "native window title"],
  ])("fails closed for %s before accepting the window", async (_name, overrides, message) => {
    const target = client(overrides);
    await expect(assertNativeWindowIdentity(target, expected, "primary")).rejects.toThrow(message);
    if (message !== "native window title") {
      expect(target.getNativeWindowTitle).not.toHaveBeenCalled();
    }
  });
});
