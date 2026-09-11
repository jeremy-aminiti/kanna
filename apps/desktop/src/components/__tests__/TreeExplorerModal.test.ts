// @vitest-environment happy-dom

import { mount } from "@vue/test-utils";
import { nextTick } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import TreeExplorerModal from "../TreeExplorerModal.vue";

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("../../invoke", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

async function settle() {
  await Promise.resolve();
  await nextTick();
  await Promise.resolve();
  await nextTick();
}

describe("TreeExplorerModal task roots", () => {
  afterEach(() => {
    invokeMock.mockReset();
  });

  it("lists a remote owner task without reading the local filesystem", async () => {
    const remoteDirectoryLoader = vi.fn(async () => ({
      entries: [
        { name: "src", path: "src", isDir: true },
        { name: "README.md", path: "README.md", isDir: false },
      ],
    }));
    const wrapper = mount(TreeExplorerModal, {
      props: {
        worktreePath: "task-owner-branch",
        repoRoot: "task-owner-branch",
        remoteDirectoryLoader,
      },
    });

    await settle();

    expect(remoteDirectoryLoader).toHaveBeenCalledWith("", false);
    expect(invokeMock).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("README.md");
    expect(wrapper.text()).not.toContain("(empty)");
    wrapper.unmount();
  });

  it("shows a missing local worktree as unavailable without falling back to the repo", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    invokeMock.mockRejectedValue(new Error("not a directory"));
    const wrapper = mount(TreeExplorerModal, {
      props: {
        worktreePath: "/repo/.kanna-worktrees/task-removed",
        repoRoot: "/repo",
      },
    });

    await settle();

    expect(wrapper.get('[data-testid="tree-explorer-unavailable"]').text()).toContain(
      "Task files unavailable: not a directory",
    );
    expect(invokeMock).toHaveBeenCalledWith("read_dir_entries", {
      path: "/repo/.kanna-worktrees/task-removed",
      repoRoot: "/repo",
      showAllFiles: false,
    });
    expect(invokeMock).not.toHaveBeenCalledWith(
      "read_dir_entries",
      expect.objectContaining({ path: "/repo" }),
    );
    expect(wrapper.text()).not.toContain("(empty)");
    consoleError.mockRestore();
    wrapper.unmount();
  });
});

/**
 * The explorer's half of `kanna_open_view` answers a waiting caller, so
 * "opened" has to mean the requested directory was read — not that the
 * explorer is mounted over an empty column.
 */
describe("TreeExplorerModal reveal for kanna_open_view", () => {
  afterEach(() => {
    invokeMock.mockReset();
  });

  function command(target?: Record<string, unknown>) {
    return {
      requestId: "view-1",
      taskId: "task-a",
      view: "tree" as const,
      ...(target ? { target } : {}),
    };
  }

  it("refuses a directory that vanished between validation and the read", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const wrapper = mount(TreeExplorerModal, {
      props: { worktreePath: "/repo/.kanna-worktrees/task-a", repoRoot: "/repo" },
    });
    await settle();

    // The server validated `build/`, and it is gone by the time the explorer
    // asks for it.
    invokeMock.mockRejectedValue(new Error("directory deleted after dispatch"));
    const outcome = await wrapper.vm.revealDesktopViewTarget(
      command({ path: "build", kind: "directory" }),
    );

    expect(outcome.opened).toBe(false);
    expect(outcome.message).toContain("directory deleted after dispatch");
    consoleError.mockRestore();
    wrapper.unmount();
  });

  it("refuses an untargeted root it could not read", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    invokeMock.mockRejectedValue(new Error("not a directory"));
    const wrapper = mount(TreeExplorerModal, {
      props: { worktreePath: "/repo/.kanna-worktrees/task-removed", repoRoot: "/repo" },
    });
    await settle();

    // No target at all: being mounted is not being on screen.
    const outcome = await wrapper.vm.revealDesktopViewTarget(command());

    expect(outcome.opened).toBe(false);
    expect(outcome.message).toContain("not a directory");
    consoleError.mockRestore();
    wrapper.unmount();
  });

  it("opens a root it could read", async () => {
    invokeMock.mockResolvedValue([
      { name: "src", is_dir: true },
      { name: "README.md", is_dir: false },
    ]);
    const wrapper = mount(TreeExplorerModal, {
      props: { worktreePath: "/repo/.kanna-worktrees/task-a", repoRoot: "/repo" },
    });
    await settle();

    expect(await wrapper.vm.revealDesktopViewTarget(command())).toEqual({ opened: true });
    wrapper.unmount();
  });

  it("reveals a file through the reading it can actually do", async () => {
    invokeMock.mockResolvedValue([
      { name: "notes.txt", is_dir: false },
    ]);
    const wrapper = mount(TreeExplorerModal, {
      props: { worktreePath: "/repo/.kanna-worktrees/task-a", repoRoot: "/repo" },
    });
    await settle();

    expect(await wrapper.vm.revealDesktopViewTarget(command({ path: "notes.txt", kind: "file" })))
      .toEqual({ opened: true });
    expect(await wrapper.vm.revealDesktopViewTarget(command({ path: "absent.txt", kind: "file" })))
      .toMatchObject({ opened: false, code: "file_not_found" });
    wrapper.unmount();
  });
});
