/**
 * Honouring one `kanna_open_view` command.
 *
 * The server has already resolved the target against the task's worktree and
 * is waiting for this window to say whether it is on screen, so the shape of
 * the work is fixed: select the task, open (or re-aim) its tab, wait for the
 * view to render its target, and answer. Every step that can fail answers with
 * a code rather than throwing, because a command nobody answers is reported to
 * the caller as an unavailable desktop — which would be a lie about a window
 * that was right there and simply could not find the task.
 */

import { mainTabScopeKeyForTask, type MainTabDescriptor } from "./useMainTabs";

/**
 * The native event carrying one command. Addressed to a single window rather
 * than broadcast, so it is listened for on this webview rather than globally.
 */
export const DESKTOP_VIEW_OPEN_EVENT = "desktop-view-open";

export const DESKTOP_VIEW_KINDS = [
  "agent",
  "file",
  "diff",
  "tree",
  "graph",
  "analytics",
] as const;

export type DesktopViewKind = (typeof DESKTOP_VIEW_KINDS)[number];

export interface DesktopViewOpenCommand {
  requestId: string;
  taskId: string;
  view: DesktopViewKind;
  target?: Record<string, unknown>;
}

export interface DesktopViewOpenOutcome {
  opened: boolean;
  code?: string;
  message?: string;
}

function isViewKind(value: unknown): value is DesktopViewKind {
  return DESKTOP_VIEW_KINDS.includes(value as DesktopViewKind);
}

/**
 * Read a command off the native event, refusing anything this window does not
 * understand rather than guessing at it. A window that cannot read the command
 * cannot acknowledge it either — it has no request id to answer with — so the
 * caller learns of it as an unavailable desktop.
 */
export function parseDesktopViewOpenCommand(payload: unknown): DesktopViewOpenCommand {
  const command = payload as Partial<DesktopViewOpenCommand> | null;
  if (
    !command
    || typeof command.requestId !== "string"
    || command.requestId.length === 0
    || typeof command.taskId !== "string"
    || command.taskId.length === 0
    || !isViewKind(command.view)
  ) {
    throw new Error("malformed desktop view open command");
  }
  const target = command.target;
  if (target !== undefined && (typeof target !== "object" || target === null || Array.isArray(target))) {
    throw new Error("malformed desktop view open target");
  }
  return {
    requestId: command.requestId,
    taskId: command.taskId,
    view: command.view,
    target: target as Record<string, unknown> | undefined,
  };
}

/**
 * The tab a command asks for. A `file` tab is identified by its path, so
 * re-opening the same file at another line re-aims the tab that is already
 * showing it; every other view is one per task.
 */
export function mainTabDescriptorForCommand(command: DesktopViewOpenCommand): MainTabDescriptor {
  if (command.view === "file") {
    const path = command.target?.path;
    const line = command.target?.line;
    return {
      kind: "file",
      filePath: typeof path === "string" ? path : "",
      initialLine: typeof line === "number" ? line : undefined,
    };
  }
  return { kind: command.view };
}

export interface DesktopViewOpenDeps {
  /** The selectable sidebar row for a task id, if this window has one. */
  findTaskSlotId: (taskId: string) => string | null;
  /** One reload, for a task this window has not heard about yet. */
  refreshTasks: () => Promise<void>;
  selectTask: (slotId: string) => Promise<void>;
  /** Open or re-aim the tab in the task's own scope; returns its id. */
  openTab: (scopeKey: string, descriptor: MainTabDescriptor) => string;
  /**
   * Wait for that tab's view to be showing, and for its target to be revealed
   * inside it. This is the step that makes `opened: true` a statement about a
   * screen rather than about a queue.
   */
  revealTab: (tabId: string, command: DesktopViewOpenCommand) => Promise<DesktopViewOpenOutcome>;
}

export async function performDesktopViewOpen(
  command: DesktopViewOpenCommand,
  deps: DesktopViewOpenDeps,
): Promise<DesktopViewOpenOutcome> {
  let slotId = deps.findTaskSlotId(command.taskId);
  if (slotId === null) {
    // The server resolved this task a moment ago, so a window that has not
    // heard of it is behind rather than wrong. One reload, then take the
    // answer: retrying past that would keep the caller waiting for a task this
    // window is never going to show.
    try {
      await deps.refreshTasks();
    } catch (error: unknown) {
      console.error("[desktop-view-open] refreshing tasks failed:", error);
    }
    slotId = deps.findTaskSlotId(command.taskId);
  }
  if (slotId === null) {
    return {
      opened: false,
      code: "task_not_found",
      message: `this window has no task ${command.taskId}`,
    };
  }

  try {
    await deps.selectTask(slotId);
  } catch (error: unknown) {
    return {
      opened: false,
      code: "renderer_failed",
      message: `selecting the task failed: ${error instanceof Error ? error.message : String(error)}`,
    };
  }

  let tabId: string;
  try {
    tabId = deps.openTab(mainTabScopeKeyForTask(command.taskId), mainTabDescriptorForCommand(command));
  } catch (error: unknown) {
    return {
      opened: false,
      code: "renderer_failed",
      message: `opening the view failed: ${error instanceof Error ? error.message : String(error)}`,
    };
  }

  try {
    return await deps.revealTab(tabId, command);
  } catch (error: unknown) {
    return {
      opened: false,
      code: "renderer_failed",
      message: `showing the view failed: ${error instanceof Error ? error.message : String(error)}`,
    };
  }
}

/**
 * Wait for a view to be ready, with a ceiling.
 *
 * The caller is holding an HTTP request open on the answer, so a view that
 * never finishes loading has to become a "no" rather than an indefinite wait.
 * The ceiling is below the route's own timeout, so a slow view is reported as
 * a slow view instead of arriving after the caller has already given up.
 */
export async function waitForViewReady(
  isReady: () => boolean,
  { timeoutMs = 8_000, stepMs = 50 }: { timeoutMs?: number; stepMs?: number } = {},
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (!isReady()) {
    if (Date.now() >= deadline) return false;
    await new Promise((resolve) => setTimeout(resolve, stepMs));
  }
  return true;
}

/** The target of a `file` view command, as the viewer needs it. */
export interface FileViewTarget {
  path: string;
  line?: number;
  column?: number;
  endLine?: number;
  endColumn?: number;
}

export function fileViewTarget(command: DesktopViewOpenCommand): FileViewTarget | null {
  const target = command.target;
  if (!target || typeof target.path !== "string") return null;
  const number = (value: unknown): number | undefined =>
    typeof value === "number" && Number.isFinite(value) && value > 0 ? value : undefined;
  return {
    path: target.path,
    line: number(target.line),
    column: number(target.column),
    endLine: number(target.endLine),
    endColumn: number(target.endColumn),
  };
}

/** The target of a `diff` view command: one line of one side of one file. */
export interface DiffViewTarget {
  scope: "branch" | "working";
  path?: string;
  side?: "old" | "new";
  /** Which side(s) number the anchored line, resolved by the server. */
  anchorKind?: "context" | "addition" | "deletion";
  oldLine?: number;
  newLine?: number;
}

export function diffViewTarget(command: DesktopViewOpenCommand): DiffViewTarget {
  const target = command.target ?? {};
  const number = (value: unknown): number | undefined =>
    typeof value === "number" && Number.isFinite(value) && value > 0 ? value : undefined;
  const side = target.side === "old" || target.side === "new" ? target.side : undefined;
  const anchorKind = target.anchorKind === "context"
    || target.anchorKind === "addition"
    || target.anchorKind === "deletion"
    ? target.anchorKind
    : undefined;
  return {
    scope: target.scope === "working" ? "working" : "branch",
    path: typeof target.path === "string" ? target.path : undefined,
    side,
    anchorKind,
    oldLine: number(target.oldLine),
    newLine: number(target.newLine),
  };
}
