import { Buffer } from "node:buffer";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  createE2eRunIdentity,
  E2E_TMUX_NAME_MAX_BYTES,
} from "./runIdentity";

describe("createE2eRunIdentity", () => {
  it("keeps ordinary worktree and run attribution readable", () => {
    const identity = createE2eRunIdentity({
      repoRoot: "/repo/.kanna-worktrees/task-abc123",
      pid: 58082,
      now: 1_788_993_557_159,
    });

    expect(identity).toEqual({
      worktreeName: "task-abc123",
      runSuffix: "58082-1788993557159",
      primarySessionName: "kanna-e2e-task-abc123-58082-1788993557159",
      secondarySessionName: "kanna-e2e-task-abc123-58082-1788993557159-secondary",
    });
  });

  it("bounds the canonical primary and secondary names for transferred task ids", () => {
    const transferredWorktree =
      "task-dfd72f7aeacaaeb08e77e99fb5087638766895d6c539117ece361c65fe8d73aa";
    const identity = createE2eRunIdentity({
      repoRoot: `/repo/.kanna-worktrees/${transferredWorktree}`,
      pid: 58082,
      now: 1_788_993_557_159,
    });

    expect(identity.worktreeName).toBe(transferredWorktree);
    expect(identity.primarySessionName)
      .toBe("kanna-e2e-task-dfd72f7aea-03e844fe5fa3412f694e");
    expect(Buffer.byteLength(identity.primarySessionName)).toBeLessThanOrEqual(
      E2E_TMUX_NAME_MAX_BYTES - "-secondary".length,
    );
    expect(Buffer.byteLength(identity.secondarySessionName)).toBeLessThanOrEqual(
      E2E_TMUX_NAME_MAX_BYTES,
    );

    // Darwin allows 103 pathname bytes plus a NUL in sockaddr_un.sun_path.
    // Use the longest possible unsigned uid to verify margin in tmux's root.
    const longestDarwinTmuxRoot = "/private/tmp/tmux-4294967295";
    expect(Buffer.byteLength(join(longestDarwinTmuxRoot, identity.secondarySessionName)))
      .toBeLessThanOrEqual(85);
  });

  it("keeps distinct runs isolated after shortening", () => {
    const repoRoot = `/repo/.kanna-worktrees/task-${"a".repeat(64)}`;
    const first = createE2eRunIdentity({ repoRoot, pid: 12, now: 1000 });
    const second = createE2eRunIdentity({ repoRoot, pid: 12, now: 1001 });

    expect(new Set([
      first.primarySessionName,
      first.secondarySessionName,
      second.primarySessionName,
      second.secondarySessionName,
    ]).size).toBe(4);
  });

  it("hashes path-unsafe and multibyte source names instead of colliding after sanitizing", () => {
    const slashLike = createE2eRunIdentity({
      repoRoot: "/repo/.kanna-worktrees/task-a:b",
      pid: 7,
      now: 9,
    });
    const multibyte = createE2eRunIdentity({
      repoRoot: "/repo/.kanna-worktrees/task-aéb",
      pid: 7,
      now: 9,
    });

    expect(slashLike.worktreeName).toBe("task-a-b");
    expect(multibyte.worktreeName).toBe("task-a-b");
    expect(slashLike.primarySessionName).not.toBe(multibyte.primarySessionName);
    expect(slashLike.primarySessionName).toMatch(/^[a-zA-Z0-9_-]+$/);
    expect(multibyte.primarySessionName).toMatch(/^[a-zA-Z0-9_-]+$/);
  });
});
