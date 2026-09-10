// @vitest-environment happy-dom

import { mount } from "@vue/test-utils";
import { nextTick } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import CommitGraphView from "../CommitGraphView.vue";
import {
  clearContextShortcuts,
  getContextShortcuts,
  resetContext,
} from "../../composables/useShortcutContext";

const invokeMock = vi.fn<
  (command: string, args?: Record<string, unknown>) => Promise<unknown>
>();

vi.mock("../../invoke", () => ({
  invoke: (...args: [string, Record<string, unknown> | undefined]) => invokeMock(...args),
}));

vi.mock("vue-i18n", () => ({
  useI18n: () => ({
    t: (key: string) => key,
  }),
}));

async function flushPromises() {
  await Promise.resolve();
  await nextTick();
}

function graphResult() {
  return {
    head_commit: "aaa1111111111111111111111111111111111111",
    commits: [
      {
        hash: "aaa1111111111111111111111111111111111111",
        short_hash: "aaa1111",
        message: "feat: add search bar",
        author: "Jeremy Hale",
        timestamp: 1710000000,
        parents: ["bbb2222222222222222222222222222222222222"],
        refs: ["main", "origin/main"],
      },
      {
        hash: "bbb2222222222222222222222222222222222222",
        short_hash: "bbb2222",
        message: "fix: stabilize graph layout",
        author: "Graph Bot",
        timestamp: 1709990000,
        parents: [],
        refs: ["v0.3.2"],
      },
    ],
  };
}

describe("CommitGraphView", () => {
  afterEach(() => {
    invokeMock.mockReset();
    clearContextShortcuts("graph");
    resetContext();
    document.body.innerHTML = "";
  });

  it("loads a remote task graph without invoking local git against its worktree", async () => {
    const remoteGraphLoader = vi.fn(async () => ({
      taskId: "owner-task",
      headCommit: "aaa1111111111111111111111111111111111111",
      commits: [{
        hash: "aaa1111111111111111111111111111111111111",
        shortHash: "aaa1111",
        message: "from the owning machine",
        author: "Owner",
        timestamp: 1710000000,
        parents: [],
        refs: ["main"],
      }],
    }));
    invokeMock.mockRejectedValue(new Error("local git must not run"));

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/remote/repo", worktreePath: "/remote/worktree", remoteGraphLoader },
      attachTo: document.body,
    });
    await flushPromises();
    await flushPromises();

    expect(remoteGraphLoader).toHaveBeenCalledOnce();
    expect(remoteGraphLoader).toHaveBeenCalledWith({ fromRef: "HEAD" });
    expect(invokeMock).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("from the owning machine");
  });

  it("uses HEAD ancestry in auto mode and all refs after Space for a remote graph", async () => {
    const remoteGraphLoader = vi.fn(async (request: { fromRef?: "HEAD" }) => ({
      taskId: "owner-task",
      headCommit: "aaa1111111111111111111111111111111111111",
      commits: [{
        hash: request.fromRef ? "aaa1111111111111111111111111111111111" : "ccc3333333333333333333333333333333333333",
        shortHash: request.fromRef ? "aaa1111" : "ccc3333",
        message: request.fromRef ? "HEAD only" : "all owner refs",
        author: "Owner", timestamp: 1710000000, parents: [], refs: ["main"],
      }],
    }));
    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/remote/repo", remoteGraphLoader }, attachTo: document.body,
    });
    await flushPromises();
    await flushPromises();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: " ", bubbles: true }));
    await flushPromises();
    await flushPromises();

    expect(remoteGraphLoader).toHaveBeenNthCalledWith(1, { fromRef: "HEAD" });
    expect(remoteGraphLoader).toHaveBeenNthCalledWith(2, { fromRef: undefined });
    expect(wrapper.text()).toContain("all owner refs");
  });

  it("opens search with slash and focuses the input", async () => {
    invokeMock.mockResolvedValue(graphResult());

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    window.dispatchEvent(new KeyboardEvent("keydown", {
      key: "/",
      bubbles: true,
    }));
    await flushPromises();

    const input = wrapper.get(".search-input");
    expect(document.activeElement).toBe(input.element);
  });

  it("matches message, author, hash, and refs", async () => {
    invokeMock.mockResolvedValue(graphResult());

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    window.dispatchEvent(new KeyboardEvent("keydown", {
      key: "/",
      bubbles: true,
    }));
    await flushPromises();

    const input = wrapper.get(".search-input");

    await input.setValue("Jeremy");
    expect(wrapper.get(".search-count").text()).toBe("1/1");

    await input.setValue("aaa1111");
    expect(wrapper.get(".search-count").text()).toBe("1/1");

    await input.setValue("origin/main");
    expect(wrapper.get(".search-count").text()).toBe("1/1");
  });

  it("returns focus to the graph after confirming search with Enter", async () => {
    invokeMock.mockResolvedValue(graphResult());

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    window.dispatchEvent(new KeyboardEvent("keydown", {
      key: "/",
      bubbles: true,
    }));
    await flushPromises();

    const input = wrapper.get(".search-input");
    await input.setValue("graph");
    await input.trigger("keydown", { key: "Enter" });
    await flushPromises();

    expect(document.activeElement).toBe(wrapper.get(".graph-scroll").element);
  });

  it("marks the active and inactive matching rows", async () => {
    invokeMock.mockResolvedValue({
      head_commit: "aaa1111111111111111111111111111111111111",
      commits: [
        {
          hash: "aaa1111111111111111111111111111111111111",
          short_hash: "aaa1111",
          message: "fix graph search",
          author: "Jeremy Hale",
          timestamp: 1710000000,
          parents: ["bbb2222222222222222222222222222222222222"],
          refs: ["main"],
        },
        {
          hash: "bbb2222222222222222222222222222222222222",
          short_hash: "bbb2222",
          message: "search follow-up",
          author: "Jeremy Hale",
          timestamp: 1709990000,
          parents: [],
          refs: [],
        },
      ],
    });

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    window.dispatchEvent(new KeyboardEvent("keydown", {
      key: "/",
      bubbles: true,
    }));
    await flushPromises();

    await wrapper.get(".search-input").setValue("search");
    await flushPromises();

    expect(wrapper.findAll(".commit-row.is-search-match")).toHaveLength(2);
    expect(wrapper.findAll(".commit-row.is-search-active")).toHaveLength(1);
  });

  it("marks the current HEAD commit in the graph and row text", async () => {
    invokeMock.mockResolvedValue(graphResult());

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    expect(wrapper.findAll(".head-node-marker")).toHaveLength(1);
    const headRow = wrapper.get(".commit-row.is-head");
    expect(headRow.get(".head-pill").text()).toBe("HEAD");
    expect(headRow.text()).toContain("aaa1111");
  });

  it("dismiss closes search before allowing the modal to close", async () => {
    invokeMock.mockResolvedValue(graphResult());

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    window.dispatchEvent(new KeyboardEvent("keydown", {
      key: "/",
      bubbles: true,
    }));
    await flushPromises();

    expect(wrapper.find(".search-input").exists()).toBe(true);

    const firstDismissResult = (wrapper.vm as { dismiss: () => boolean }).dismiss();
    await flushPromises();

    expect(firstDismissResult).toBe(false);
    expect(wrapper.find(".search-input").exists()).toBe(false);

    const secondDismissResult = (wrapper.vm as { dismiss: () => boolean }).dismiss();
    expect(secondDismissResult).toBe(true);
  });

  it("shows the no-matches label when the query finds nothing", async () => {
    invokeMock.mockResolvedValue(graphResult());

    const wrapper = mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    window.dispatchEvent(new KeyboardEvent("keydown", {
      key: "/",
      bubbles: true,
    }));
    await flushPromises();

    await wrapper.get(".search-input").setValue("does-not-exist");
    await flushPromises();

    expect(wrapper.get(".search-count").text()).toBe("commitGraph.searchNoMatches");
  });

  it("registers search shortcuts in the graph context", async () => {
    invokeMock.mockResolvedValue(graphResult());

    mount(CommitGraphView, {
      props: { repoPath: "/repo" },
      attachTo: document.body,
    });

    await flushPromises();
    await flushPromises();

    expect(getContextShortcuts("graph")).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ action: "commitGraph.shortcutSearch", keys: "/" }),
        expect.objectContaining({ action: "commitGraph.shortcutSearchAlt", keys: "⌘F" }),
        expect.objectContaining({ action: "commitGraph.shortcutNextPrevMatch", keys: "n / N" }),
      ])
    );
  });
});
