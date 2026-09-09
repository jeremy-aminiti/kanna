import type { PipelineItem } from "../types/kanna";
import type { DesktopCloudTerminalRef } from "../services/desktopCloudTaskIndex";

/**
 * Ids the cloud index mints for tasks owned by another machine
 * (`cloud:<cloud task id>`, `cloud:lan:<peer>:<repo>:<task>`). They are a
 * presentation namespace: no `kanna-server` anywhere owns one, so asking any
 * server about one is always a 404. The local server answers that 404 by
 * forwarding the id to every reachable peer, which turns one mistaken lookup
 * into a relay round-trip per machine — 82,047 of them in one session, and a
 * matching 404 stream in the *other* machine's log.
 */
export function isRemotePresentationTaskId(taskId: string): boolean {
  return taskId.startsWith("cloud:") || taskId.startsWith("lan:");
}

export function remoteTaskClosureKey(ref: DesktopCloudTerminalRef | undefined | null): string | null {
  if (!ref?.ownerDesktopId || !ref.ownerLocalTaskId) return null;
  return `owner:${ref.ownerDesktopId}:${ref.ownerLocalTaskId}`;
}

export function remoteTaskClosureAliases(
  item: Pick<PipelineItem, "id">,
  ref?: DesktopCloudTerminalRef | null,
): string[] {
  const aliases = new Set<string>([item.id]);
  const key = remoteTaskClosureKey(ref);
  if (key) aliases.add(key);
  return [...aliases];
}

export function remoteTaskIsLocallyClosed(
  item: Pick<PipelineItem, "id">,
  ref: DesktopCloudTerminalRef | undefined,
  closedAliases: ReadonlySet<string>,
): boolean {
  return remoteTaskClosureAliases(item, ref).some((alias) => closedAliases.has(alias));
}
