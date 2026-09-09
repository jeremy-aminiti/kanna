import { nextTick, ref } from "vue";
import { describe, expect, it, vi } from "vitest";

import type { TransferAlert } from "../stores/state";
import type { PipelineItem } from "../types/kanna";
import { useTransferFailureToasts } from "./useTransferFailureToasts";

function item(overrides: Partial<PipelineItem>): PipelineItem {
  return {
    id: "task-1",
    repo_id: "repo-1",
    stage: "in progress",
    ...overrides,
  } as PipelineItem;
}

describe("useTransferFailureToasts", () => {
  it("announces a failed transfer once, with the reason the engine recorded", async () => {
    const items = ref<PipelineItem[]>([]);
    const toastError = vi.fn();
    useTransferFailureToasts(items, toastError, () => "Task transfer failed");

    items.value = [
      item({
        transfer_status: "failed",
        transfer_error: "task task-1 resumes claude session s-1 but no transcript exists",
      }),
    ];
    await nextTick();

    expect(toastError).toHaveBeenCalledTimes(1);
    expect(toastError.mock.calls[0][0]).toContain("no transcript exists");
    expect(toastError.mock.calls[0][0]).toContain("Task transfer failed");

    // The snapshot reloads constantly; the same failure is not news twice.
    items.value = [...items.value];
    await nextTick();
    expect(toastError).toHaveBeenCalledTimes(1);
  });

  /**
   * A retry that fails for a new reason is new information, and the same task
   * failing the same way again after a successful retry is too — otherwise the
   * second outage is silent.
   */
  it("announces a new reason, and the same reason again after it clears", async () => {
    const items = ref<PipelineItem[]>([
      item({ transfer_status: "failed", transfer_error: "peer unreachable" }),
    ]);
    const toastError = vi.fn();
    useTransferFailureToasts(items, toastError, () => "Task transfer failed");
    expect(toastError).toHaveBeenCalledTimes(1);

    items.value = [item({ transfer_status: "failed", transfer_error: "no transcript exists" })];
    await nextTick();
    expect(toastError).toHaveBeenCalledTimes(2);

    // The retry succeeds — the row leaves `failed` — and then fails again the
    // same way. That is a second outage, not a repeat of the first.
    items.value = [item({ transfer_status: "pending", transfer_error: null })];
    await nextTick();
    items.value = [item({ transfer_status: "failed", transfer_error: "no transcript exists" })];
    await nextTick();
    expect(toastError).toHaveBeenCalledTimes(3);
  });

  it("says nothing about transfers that are in flight or have no reason", async () => {
    const items = ref<PipelineItem[]>([]);
    const toastError = vi.fn();
    useTransferFailureToasts(items, toastError, () => "Task transfer failed");

    items.value = [
      item({ id: "task-1", transfer_status: "pending", transfer_error: null }),
      item({ id: "task-2", transfer_status: "importing", transfer_error: null }),
      // `failed` with no reason has nothing to tell the operator that the
      // sidebar's own transfer state does not already show.
      item({ id: "task-3", transfer_status: "failed", transfer_error: "   " }),
      item({ id: "task-4" }),
    ];
    await nextTick();

    expect(toastError).not.toHaveBeenCalled();
  });

  /**
   * The 2026-09-08 report: a pull refused by the source told the machine that
   * asked for it nothing at all. There is no task here to carry the failure —
   * nothing arrived and nothing will — so it arrives as a transfer alert, and
   * it has to reach the operator who started the move.
   */
  it("announces a pull the other machine refused, naming the task asked for", async () => {
    const items = ref<PipelineItem[]>([]);
    const alerts = ref<TransferAlert[]>([]);
    const toastError = vi.fn();
    useTransferFailureToasts(
      items,
      toastError,
      () => "Task transfer failed",
      alerts,
      (taskId) => `The other machine will not send task ${taskId}`,
    );

    alerts.value = [
      {
        transferId: "refused-pull-peer-a-pull-1",
        direction: "incoming",
        sourceTaskId: "afed27d1",
        sourcePeerId: "peer-a",
        error: "task afed27d1 resumes codex session 5a2eb492 but its rollout could not be found",
      },
    ];
    await nextTick();

    expect(toastError).toHaveBeenCalledTimes(1);
    expect(toastError.mock.calls[0][0]).toContain("afed27d1");
    expect(toastError.mock.calls[0][0]).toContain("rollout could not be found");

    // Every snapshot carries the alert until it is dismissed; it is news once.
    alerts.value = [...alerts.value];
    await nextTick();
    expect(toastError).toHaveBeenCalledTimes(1);
  });

  /**
   * The defect the reviewer caught: an alert has no marker to click and no
   * task to sit on, so nothing ever set `dismissed_at` for it and the same
   * refusal toasted at every window mount for the life of the database.
   */
  it("retires an alert on the server once it has been announced", async () => {
    const alerts = ref<TransferAlert[]>([]);
    const toastError = vi.fn();
    const dismissAlert = vi.fn();
    useTransferFailureToasts(
      ref<PipelineItem[]>([]),
      toastError,
      () => "Task transfer failed",
      alerts,
      (taskId) => `The other machine will not send task ${taskId}`,
      dismissAlert,
    );

    alerts.value = [
      {
        transferId: "refused-pull-1",
        direction: "incoming",
        sourceTaskId: "afed27d1",
        error: "its rollout could not be found",
      },
    ];
    await nextTick();
    expect(toastError).toHaveBeenCalledTimes(1);
    expect(dismissAlert).toHaveBeenCalledWith("refused-pull-1");
    expect(dismissAlert).toHaveBeenCalledTimes(1);

    // The row stays in this window's snapshot until the next refresh; it is
    // dismissed once, not on every tick of the watcher.
    alerts.value = [...alerts.value];
    await nextTick();
    expect(dismissAlert).toHaveBeenCalledTimes(1);
    expect(toastError).toHaveBeenCalledTimes(1);
  });

  /**
   * The retirement is server-side, so the next window's snapshot no longer
   * carries the alert — which is what stops it announcing at every launch.
   */
  it("says nothing in a fresh window once the server has retired the alert", async () => {
    const announced: TransferAlert[] = [
      {
        transferId: "refused-pull-1",
        direction: "incoming",
        sourceTaskId: "afed27d1",
        error: "its rollout could not be found",
      },
    ];
    const retired = new Set<string>();
    const mount = () => {
      const toastError = vi.fn();
      useTransferFailureToasts(
        ref<PipelineItem[]>([]),
        toastError,
        () => "Task transfer failed",
        ref(announced.filter((alert) => !retired.has(alert.transferId))),
        (taskId) => `The other machine will not send task ${taskId}`,
        (transferId) => retired.add(transferId),
      );
      return toastError;
    };

    const first = mount();
    await nextTick();
    expect(first).toHaveBeenCalledTimes(1);

    const second = mount();
    await nextTick();
    expect(second).not.toHaveBeenCalled();
  });

  /**
   * A failure that rides a task keeps the sidebar's `⇄✗` marker as its
   * standing surface, so announcing it must not retire it behind the
   * operator's back.
   */
  it("never retires a failure that has a task of its own", async () => {
    const dismissAlert = vi.fn();
    useTransferFailureToasts(
      ref<PipelineItem[]>([
        item({ transfer_status: "failed", transfer_error: "peer unreachable" }),
      ]),
      vi.fn(),
      () => "Task transfer failed",
      ref<TransferAlert[]>([]),
      (taskId) => `will not send ${taskId}`,
      dismissAlert,
    );
    await nextTick();
    expect(dismissAlert).not.toHaveBeenCalled();
  });

  it("says nothing about an alert the source gave no reason for", async () => {
    const alerts = ref<TransferAlert[]>([
      { transferId: "refused-pull-1", direction: "incoming", sourceTaskId: "t-1", error: "  " },
    ]);
    const toastError = vi.fn();
    useTransferFailureToasts(
      ref<PipelineItem[]>([]),
      toastError,
      () => "Task transfer failed",
      alerts,
      (taskId) => `The other machine will not send task ${taskId}`,
    );
    await nextTick();
    expect(toastError).not.toHaveBeenCalled();
  });
});
