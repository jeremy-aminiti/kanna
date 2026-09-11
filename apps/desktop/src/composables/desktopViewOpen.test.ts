import { describe, expect, it, vi } from "vitest";

import {
  mainTabDescriptorForCommand,
  parseDesktopViewOpenCommand,
  performDesktopViewOpen,
  diffViewTarget,
  fileViewTarget,
  type DesktopViewOpenCommand,
  type DesktopViewOpenDeps,
} from "./desktopViewOpen";

function command(overrides: Partial<DesktopViewOpenCommand> = {}): DesktopViewOpenCommand {
  return { requestId: "view-1", taskId: "task-a", view: "file", ...overrides };
}

function deps(overrides: Partial<DesktopViewOpenDeps> = {}): DesktopViewOpenDeps {
  return {
    findTaskSlotId: (taskId) => (taskId === "task-a" ? "slot-a" : null),
    refreshTasks: async () => {},
    selectTask: async () => {},
    openTab: () => "file:src/main.rs",
    revealTab: async () => ({ opened: true }),
    ...overrides,
  };
}

describe("parseDesktopViewOpenCommand", () => {
  it("refuses a command it cannot act on rather than guessing", () => {
    expect(() => parseDesktopViewOpenCommand(null)).toThrow();
    expect(() => parseDesktopViewOpenCommand({ taskId: "task-a", view: "file" })).toThrow();
    expect(() => parseDesktopViewOpenCommand({ requestId: "r", taskId: "task-a", view: "shell" }))
      .toThrow();
    expect(() => parseDesktopViewOpenCommand({
      requestId: "r",
      taskId: "task-a",
      view: "file",
      target: "src/main.rs",
    })).toThrow();
  });

  it("reads the whitelisted views and their target object", () => {
    expect(parseDesktopViewOpenCommand({
      requestId: "view-7",
      taskId: "task-a",
      view: "diff",
      target: { scope: "working", path: "src/main.rs", side: "new", line: 4 },
    })).toEqual({
      requestId: "view-7",
      taskId: "task-a",
      view: "diff",
      target: { scope: "working", path: "src/main.rs", side: "new", line: 4 },
    });
  });
});

describe("mainTabDescriptorForCommand", () => {
  it("identifies a file tab by its path, so a second open re-aims it", () => {
    expect(mainTabDescriptorForCommand(command({
      target: { path: "src/main.rs", line: 12 },
    }))).toEqual({
      kind: "file",
      filePath: "src/main.rs",
      initialLine: 12,
      // The view reads back through the server's contained resolution, so the
      // task whose worktree bounds it travels with the tab.
      containedTaskId: "task-a",
    });
  });

  it("gives every other view one tab per task", () => {
    expect(mainTabDescriptorForCommand(command({ view: "diff", target: { scope: "branch" } })))
      .toEqual({ kind: "diff" });
    expect(mainTabDescriptorForCommand(command({ view: "tree", target: { path: "src" } })))
      .toEqual({ kind: "tree", containedTaskId: "task-a" });
    expect(mainTabDescriptorForCommand(command({ view: "agent", target: undefined })))
      .toEqual({ kind: "agent" });
  });
});

describe("target readers", () => {
  it("keeps only positions that are positions", () => {
    expect(fileViewTarget(command({ target: { path: "a.txt", line: 0, column: -3 } })))
      .toEqual({ path: "a.txt", line: undefined, column: undefined, endLine: undefined, endColumn: undefined });
    expect(fileViewTarget(command({ target: {} }))).toBeNull();
  });

  it("defaults a diff target to the branch scope", () => {
    expect(diffViewTarget(command({ view: "diff", target: undefined })).scope).toBe("branch");
    expect(diffViewTarget(command({ view: "diff", target: { scope: "working" } })).scope)
      .toBe("working");
  });
});

describe("performDesktopViewOpen", () => {
  it("selects the task, opens its tab, and reports what the view says", async () => {
    const selected: string[] = [];
    const opened: Array<[string, unknown]> = [];
    const outcome = await performDesktopViewOpen(
      command({ target: { path: "src/main.rs", line: 3 } }),
      deps({
        selectTask: async (slotId) => {
          selected.push(slotId);
        },
        openTab: (scopeKey, descriptor) => {
          opened.push([scopeKey, descriptor]);
          return "file:src/main.rs";
        },
      }),
    );
    expect(outcome).toEqual({ opened: true });
    expect(selected).toEqual(["slot-a"]);
    expect(opened).toEqual([[
      "item:task-a",
      {
        kind: "file",
        filePath: "src/main.rs",
        initialLine: 3,
        containedTaskId: "task-a",
      },
    ]]);
  });

  it("reloads once for a task this window has not heard of, then gives up honestly", async () => {
    const refreshTasks = vi.fn(async () => {});
    const outcome = await performDesktopViewOpen(
      command({ taskId: "task-unknown" }),
      deps({ refreshTasks }),
    );
    expect(refreshTasks).toHaveBeenCalledTimes(1);
    expect(outcome.opened).toBe(false);
    expect(outcome.code).toBe("task_not_found");
  });

  it("takes the task a reload turned up", async () => {
    let known = false;
    const outcome = await performDesktopViewOpen(
      command({ taskId: "task-late", target: { path: "a.txt" } }),
      deps({
        findTaskSlotId: (taskId) => (known && taskId === "task-late" ? "slot-late" : null),
        refreshTasks: async () => {
          known = true;
        },
      }),
    );
    expect(outcome).toEqual({ opened: true });
  });

  it("turns a throwing step into a failure the caller can read", async () => {
    const outcome = await performDesktopViewOpen(
      command({ target: { path: "a.txt" } }),
      deps({
        revealTab: async () => {
          throw new Error("the view exploded");
        },
      }),
    );
    expect(outcome.opened).toBe(false);
    expect(outcome.code).toBe("renderer_failed");
    expect(outcome.message).toContain("the view exploded");
  });

  it("passes a view's own refusal through unchanged", async () => {
    const outcome = await performDesktopViewOpen(
      command({ view: "graph", target: { commit: "a".repeat(40) } }),
      deps({
        openTab: () => "graph",
        revealTab: async () => ({ opened: false, code: "commit_not_found", message: "gone" }),
      }),
    );
    expect(outcome).toEqual({ opened: false, code: "commit_not_found", message: "gone" });
  });
});
