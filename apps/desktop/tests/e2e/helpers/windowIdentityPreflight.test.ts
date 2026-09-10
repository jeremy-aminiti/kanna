import { describe, expect, it, vi } from "vitest";

import { preflightNativeWindowIdentity } from "./windowIdentityPreflight";
import type { ExpectedNativeWindowIdentity } from "./windowIdentity";

const expected = {
  version: "0.0.68",
  branch: "task-abc-2",
  commitHash: "abc1234",
  taskId: "abc",
  worktree: "task-abc-2",
  nativeTitle: "Kanna — task abc · task-abc-2 (0.0.68 @ abc1234)",
} satisfies ExpectedNativeWindowIdentity;

function fakeClient(overrides: { taskId?: string; title?: string } = {}) {
  return {
    createSession: vi.fn(async () => "session"),
    deleteSession: vi.fn(async () => {}),
    getAppBuildInfo: vi.fn(async () => ({
      version: expected.version,
      branch: expected.branch,
      commit_hash: expected.commitHash,
      task_id: overrides.taskId ?? expected.taskId,
      worktree: expected.worktree,
    })),
    getBaseUrl: vi.fn(() => "http://127.0.0.1:4445"),
    getNativeWindowTitle: vi.fn(async () => overrides.title ?? expected.nativeTitle),
  };
}

describe("preflightNativeWindowIdentity", () => {
  it("checks primary and secondary independently without dismissing startup UI", async () => {
    const primary = fakeClient();
    const secondary = fakeClient();

    await preflightNativeWindowIdentity(primary as never, expected, "primary");
    await preflightNativeWindowIdentity(secondary as never, expected, "secondary");

    expect(primary.createSession).toHaveBeenCalledWith({
      dismissStartupShortcuts: false,
    });
    expect(secondary.createSession).toHaveBeenCalledWith({
      dismissStartupShortcuts: false,
    });
    expect(primary.getAppBuildInfo).toHaveBeenCalledOnce();
    expect(secondary.getAppBuildInfo).toHaveBeenCalledOnce();
    expect(primary.getNativeWindowTitle).toHaveBeenCalledOnce();
    expect(secondary.getNativeWindowTitle).toHaveBeenCalledOnce();
    expect(primary.deleteSession).toHaveBeenCalledOnce();
    expect(secondary.deleteSession).toHaveBeenCalledOnce();
  });

  it("cleans up the owned session path when verification fails", async () => {
    const target = fakeClient({ taskId: "wrong-task" });

    await expect(preflightNativeWindowIdentity(target as never, expected, "primary"))
      .rejects.toThrow("identity mismatch");
    expect(target.deleteSession).toHaveBeenCalledOnce();
  });
});
