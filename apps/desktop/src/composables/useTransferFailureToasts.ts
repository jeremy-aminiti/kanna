import { watch, type Ref } from "vue";
import type { TransferAlert } from "../stores/state";
import type { PipelineItem } from "../types/kanna";

/**
 * Announces a transfer that failed, once.
 *
 * A transfer is server work now, so a refusal has no caller left to throw at:
 * the source that cannot ship a conversation, and the import that gave up,
 * both end as a `failed` row carrying their reason. That reason reaches the
 * frontend on the ordinary snapshot — no bespoke event protocol between the
 * engine and a window — and this turns it into the one thing a snapshot cannot
 * express on its own: a notification the operator sees without looking.
 *
 * Two sources, because a failure does not always have a task here to ride on,
 * and they retire differently for exactly that reason:
 *
 * - A failure that rides a **task** keeps the sidebar's `⇄✗` marker as its
 *   standing surface, so it is retired when the operator clicks that marker.
 *   Announcing it here is a nudge, not the whole telling.
 * - A **task-less** failure — a pull the source refused, an import that died
 *   before it created anything — has no surface at all: nothing arrived and
 *   nothing will. The toast *is* the telling, so it is dismissed as soon as it
 *   has been told. Without that, `dismissed_at` had no caller reachable for
 *   these rows and the same alert toasted at every window mount for the life
 *   of the database, which is the defect this composable exists to remove.
 *
 * The durable record is untouched either way — `kanna_task_transfers` still
 * answers "where has this been?" with it. Only the news is retired.
 *
 * Reactive rather than imperative, so it works the same whether the failure
 * arrives while the window is open, or is already there when it mounts.
 */
export function useTransferFailureToasts(
  items: Ref<PipelineItem[]>,
  toastError: (message: string) => void,
  transferFailedLabel: () => string,
  alerts?: Ref<TransferAlert[]>,
  refusedPullLabel?: (sourceTaskId: string) => string,
  dismissAlert?: (transferId: string) => void,
) {
  // Keyed by the failure's identity *and* its reason: a retry that fails again
  // for a new reason is new information, and the same reason twice is not.
  //
  // JSON-encoded rather than joined on a separator. The key is only ever
  // compared, and a reason is free text from another machine, so no separator
  // is provably absent from it. (This used to join on a literal NUL, which
  // made the whole file read as binary to `grep` and to `git diff` — the same
  // trap `session_plan_identity` documents on the Rust side.)
  const announced = new Set<string>();
  const key = (id: string, reason: string) => JSON.stringify([id, reason]);

  /** True when this call is what put the message on screen. */
  function announce(live: Set<string>, announcementKey: string, message: string): boolean {
    live.add(announcementKey);
    if (announced.has(announcementKey)) return false;
    announced.add(announcementKey);
    toastError(message);
    return true;
  }

  const stop = watch(
    [items, () => alerts?.value ?? []] as const,
    ([currentItems, currentAlerts]) => {
      const live = new Set<string>();
      for (const item of currentItems) {
        if (item.transfer_status !== "failed") continue;
        const reason = item.transfer_error?.trim();
        if (!reason) continue;
        announce(live, key(item.id, reason), `${transferFailedLabel()}: ${reason}`);
      }
      for (const alert of currentAlerts) {
        const reason = alert.error?.trim();
        if (!reason) continue;
        // The source's own id for the task is what the operator typed to pull
        // it, so it is what identifies the failure back to them.
        const label = refusedPullLabel && alert.sourceTaskId
          ? refusedPullLabel(alert.sourceTaskId)
          : transferFailedLabel();
        const announcedNow = announce(live, key(alert.transferId, reason), `${label}: ${reason}`);
        // Retired on the server, not just in this window: `announced` above
        // dies with the window, and the next one would say it all again.
        if (announcedNow) dismissAlert?.(alert.transferId);
      }
      // Forget failures that are no longer reported, so a task whose retry
      // fails the same way later is announced again rather than silently.
      for (const key of announced) {
        if (!live.has(key)) announced.delete(key);
      }
    },
    { deep: true, immediate: true },
  );

  return { stop };
}
