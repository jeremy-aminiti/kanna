// @vitest-environment happy-dom

import { ref } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createWorkflowApi } from "./workflow";
import type { StoreContext } from "./state";
import { postDesktopTaskAction } from "../services/desktopTaskActions";

const post = vi.fn(async () => new Response(
  JSON.stringify({ taskId: "task-merge", created: false, ownerDesktopId: "desktop-2" }),
  { status: 200 },
));

vi.mock("../services/desktopTaskActions", () => ({
  postDesktopTaskAction: (...args: unknown[]) => post(...(args as [])),
}));

vi.mock("../services/desktopServerClient", () => ({
  fetchDesktopRepoWorkflowDefinition: vi.fn(),
  fetchDesktopRepoAgentDefinition: vi.fn(),
}));

function testContext(): StoreContext {
  return {
    state: {
      items: ref([]),
      selectedItemId: ref(null),
      selectedRepoId: ref("repo-1"),
      lastSelectedItemByRepo: ref({}),
      workflowCache: new Map(),
      agentCache: new Map(),
    },
    services: {
      selectedTaskId: ref(null),
      sortedItemsForCurrentRepo: ref([]),
      reloadSnapshot: vi.fn(),
      isItemHidden: () => false,
      selectItem: vi.fn(),
      persistSelection: vi.fn(),
    },
    toast: { warning: vi.fn(), error: vi.fn() },
    tt: (key: string) => key,
  } as unknown as StoreContext;
}

describe("queueReviewedPrForMerge", () => {
  afterEach(() => vi.clearAllMocks());

  /**
   * The request carries only what the operator was looking at. It deliberately
   * sends no branch, target, or PR URL: the server derives those from the
   * task's stored review context, so this control cannot confirm one pull
   * request and queue another.
   */
  it("sends only the operator's decision and reports the merge task", async () => {
    const result = await createWorkflowApi(testContext()).queueReviewedPrForMerge("task-review", {
      reviewContextVersion: 3,
      headSha: "a".repeat(40),
      actionText: "I reviewed it and authorize the merge.",
      summary: "Human-reviewed https://github.com/acme/repo/pull/12",
    });

    expect(post).toHaveBeenCalledWith("task-review", "signal-merge-handoff", {
      summary: "Human-reviewed https://github.com/acme/repo/pull/12",
      humanReviewDecision: {
        reviewContextVersion: 3,
        headSha: "a".repeat(40),
        actionText: "I reviewed it and authorize the merge.",
      },
    });
    expect(result).toEqual({
      status: "delivered",
      mergeTaskId: "task-merge",
      ownerDesktopId: "desktop-2",
    });
  });

  /**
   * A refusal is the server saying the decision no longer matches what would
   * be merged — a head that moved, a context that was refreshed. The operator
   * has to see that reason, not a generic failure, because the fix is a fresh
   * read of the pull request.
   */
  it("surfaces the server's refusal verbatim", async () => {
    post.mockResolvedValueOnce(
      new Response("the reviewed head moved: you decided on abc, the recorded head is def", {
        status: 409,
      }),
    );

    const result = await createWorkflowApi(testContext()).queueReviewedPrForMerge("task-review", {
      reviewContextVersion: 1,
      headSha: "b".repeat(40),
      actionText: "I reviewed it and authorize the merge.",
      summary: "Human-reviewed pull request",
    });

    expect(result.status).toBe("failed");
    expect(result).toMatchObject({
      message: expect.stringContaining("the reviewed head moved"),
    });
  });
});
