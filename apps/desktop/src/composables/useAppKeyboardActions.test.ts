import { computed, ref } from "vue";
import { describe, expect, it, vi } from "vitest";
import type { PipelineItem } from "../types/kanna";
import type { WorkspaceTask } from "../workspace/types";
import { useAppKeyboardActions } from "./useAppKeyboardActions";
import type { ShortcutContext } from "./useShortcutContext";
import { useMainTabs } from "./useMainTabs";

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("./useKeyboardShortcuts", () => ({
  useKeyboardShortcuts: vi.fn(),
}));

vi.mock("../invoke", () => ({ invoke: invokeMock }));

function item(id: string): PipelineItem {
  return {
    id,
    repo_id: "repo-1",
    issue_number: null,
    issue_title: null,
    prompt: "Durable action",
    workflow: "default",
    pipeline_def: null,
    stage: "in progress",
    pr_number: null,
    pr_url: null,
    branch: `task-${id}`,
    closed_at: null,
    agent_type: "pty",
    agent_provider: "claude",
    activity: "idle",
    activity_changed_at: null,
    unread_at: null,
    port_offset: null,
    display_name: null,
    last_output_preview: null,
    port_env: null,
    pinned: 0,
    pin_order: null,
    base_ref: null,
    agent_session_id: null,
    teardown_started_at: null,
    parent_task_id: null,
    notify_task_id: null,
    notified_at: null,
    created_at: "2026-07-11T00:00:00.000Z",
    updated_at: "2026-07-11T00:00:00.000Z",
  };
}

function remoteWorkspaceTask(presentationTaskId: string): WorkspaceTask {
  return {
    item: item(presentationTaskId),
    localTaskId: null,
  } as WorkspaceTask;
}

function createHarness(options: {
  selectedSlotId?: string | null;
  selectedTaskId?: string | null;
  currentItem?: PipelineItem | null;
  workspaceTask?: WorkspaceTask | null;
  workspaceTaskBlocked?: boolean;
  activeTabKind?: "agent" | "diff";
} = {}) {
  const openWindow = vi.fn(async () => {});
  const advanceStage = vi.fn(async () => {});
  const navigateBack = vi.fn(async () => {});
  const navigateForward = vi.fn(async () => {});
  const advanceSelectedRemoteWorkspaceTask = vi.fn(async () => {});
  const toast = { warning: vi.fn() };
  const showFilePickerModal = ref(false);
  const showFilePickerOnTop = vi.fn(() => { showFilePickerModal.value = true; });
  const store = {
    selectedRepoId: "repo-1",
    selectedItemId: options.selectedSlotId ?? "create:stable",
    selectedTaskId: options.selectedTaskId ?? null,
    currentItem: options.currentItem ?? null,
    advanceStage,
  };
  const mainTabs = useMainTabs({ scopeKey: computed(() => "item:task-durable") });
  if (options.activeTabKind === "diff") mainTabs.openTab({ kind: "diff" });
  const overlayContext = ref<ShortcutContext>("main");
  const showShortcutsModal = ref(false);
  const shortcutsContext = ref<ShortcutContext>("main");
  const shortcutsStartFull = ref(false);
  const requestCloseCurrentWindow = vi.fn(async () => {});
  const { keyboardActions } = useAppKeyboardActions({
    store,
    windowWorkspace: { openWindow },
    toast,
    t: (key: string) => key,
    selectedWorkspaceTask: computed(() => options.workspaceTask ?? null),
    selectedWorkspaceTaskBlocked: computed(() => options.workspaceTaskBlocked ?? false),
    advanceSelectedRemoteWorkspaceTask,
    mainTabs,
    mainPanelRef: ref(null),
    requestCloseCurrentWindow,
    currentShortcutContext: computed(() => showShortcutsModal.value ? "main" : overlayContext.value),
    showShortcutsModal,
    shortcutsContext,
    shortcutsStartFull,
    showCommandPalette: ref(false),
    showFilePickerModal,
    showFilePickerOnTop,
    closeFilePicker: vi.fn(),
    getCurrentPreviewRecall: () => undefined,
    openFilePreview: vi.fn(),
    navigateBack,
    navigateForward,
  } as unknown as Parameters<typeof useAppKeyboardActions>[0]);
  return {
    keyboardActions,
    overlayContext,
    showShortcutsModal,
    shortcutsContext,
    shortcutsStartFull,
    mainTabs,
    requestCloseCurrentWindow,
    openWindow,
    advanceStage,
    advanceSelectedRemoteWorkspaceTask,
    navigateBack,
    navigateForward,
    toast,
    showFilePickerOnTop,
  };
}

describe("useAppKeyboardActions durable selection", () => {
  it("opens a local task window with the durable task id, not its UI slot", async () => {
    const { keyboardActions, openWindow } = createHarness({
      selectedSlotId: "create:stable",
      selectedTaskId: "task-durable",
      currentItem: item("task-durable"),
    });

    await keyboardActions.newWindow();

    expect(openWindow).toHaveBeenCalledWith({
      selectedRepoId: "repo-1",
      selectedItemId: "task-durable",
    });
  });

  it("opens a remote task window with its projected backend identity", async () => {
    const { keyboardActions, openWindow } = createHarness({
      selectedSlotId: "remote:logical-task",
      selectedTaskId: null,
      workspaceTask: remoteWorkspaceTask("cloud:repo:task-remote"),
    });

    await keyboardActions.newWindow();

    expect(openWindow).toHaveBeenCalledWith({
      selectedRepoId: "repo-1",
      selectedItemId: "cloud:repo:task-remote",
    });
  });

  it("refuses Open in IDE for a task owned by another machine without invoking a local path command", async () => {
    const workspaceTask = remoteWorkspaceTask("cloud:repo:task-remote");
    workspaceTask.capabilities = { canOpenShell: false } as WorkspaceTask["capabilities"];
    const { keyboardActions, toast } = createHarness({ workspaceTask });

    await keyboardActions.openInIDE();

    expect(toast.warning).toHaveBeenCalledWith("toasts.remoteTaskPathUnavailable");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("refuses file picker shortcuts for a task owned by another machine before a local file command", () => {
    const workspaceTask = remoteWorkspaceTask("cloud:repo:task-remote");
    workspaceTask.capabilities = { canOpenShell: false } as WorkspaceTask["capabilities"];
    const { keyboardActions, showFilePickerOnTop, toast } = createHarness({ workspaceTask });

    keyboardActions.openFile();
    keyboardActions.toggleFilePreview();

    expect(toast.warning).toHaveBeenCalledWith("toasts.remoteTaskPathUnavailable");
    expect(showFilePickerOnTop).not.toHaveBeenCalled();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("opens the local file picker shortcuts when the repository has no selected task", () => {
    const { keyboardActions, showFilePickerOnTop, toast } = createHarness();

    keyboardActions.openFile();
    keyboardActions.toggleFilePreview();

    expect(showFilePickerOnTop).toHaveBeenCalledTimes(2);
    expect(toast.warning).not.toHaveBeenCalled();
  });

  it("refuses the repo-root shell shortcut for a task owned by another machine", () => {
    const workspaceTask = remoteWorkspaceTask("cloud:repo:task-remote");
    workspaceTask.capabilities = { canOpenShell: false } as WorkspaceTask["capabilities"];
    const { keyboardActions, mainTabs, toast } = createHarness({ workspaceTask });

    keyboardActions.openShellRepoRoot();

    expect(toast.warning).toHaveBeenCalledWith("toasts.remoteShellUnavailable");
    expect(mainTabs.tabs.value.some((tab) => tab.kind === "shell")).toBe(false);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("advances a selected durable task behind a noncanonical UI slot", () => {
    const durableItem = item("task-durable");
    const { keyboardActions, advanceStage } = createHarness({
      selectedSlotId: "create:stable",
      selectedTaskId: durableItem.id,
      currentItem: durableItem,
    });

    keyboardActions.advanceStage();

    expect(advanceStage).toHaveBeenCalledWith("task-durable");
  });

  it("closes the tab in front with the close-tab shortcut", async () => {
    const { keyboardActions, mainTabs, requestCloseCurrentWindow } = createHarness({
      currentItem: item("task-durable"),
      activeTabKind: "diff",
    });

    await keyboardActions.closeTabOrWindow();

    expect(mainTabs.isOpen("diff")).toBe(false);
    expect(requestCloseCurrentWindow).not.toHaveBeenCalled();
  });

  it("refuses to close the window while views are still open behind the agent tab", async () => {
    const { keyboardActions, mainTabs, requestCloseCurrentWindow } = createHarness({
      currentItem: item("task-durable"),
      activeTabKind: "diff",
    });
    // The agent session comes forward while the diff stays open behind it.
    mainTabs.activateTab("agent");

    await keyboardActions.closeTabOrWindow();

    expect(mainTabs.isOpen("diff")).toBe(true);
    expect(requestCloseCurrentWindow).not.toHaveBeenCalled();
  });

  it("closes the window once the agent tab is all that is left", async () => {
    const { keyboardActions, requestCloseCurrentWindow } = createHarness({
      currentItem: item("task-durable"),
    });

    await keyboardActions.closeTabOrWindow();

    expect(requestCloseCurrentWindow).toHaveBeenCalledOnce();
  });

  it("advances a selected task while its diff tab is in front", () => {
    const { keyboardActions, advanceStage } = createHarness({
      currentItem: item("task-durable"),
      activeTabKind: "diff",
    });

    keyboardActions.advanceStage();

    expect(advanceStage).toHaveBeenCalledWith("task-durable");
  });

  it("does not advance a selected remote task while its blocker is unresolved", () => {
    const workspaceTask = remoteWorkspaceTask("cloud:repo:task-remote");
    workspaceTask.capabilities = {
      canAdvanceStage: true,
    } as WorkspaceTask["capabilities"];
    const {
      keyboardActions,
      advanceSelectedRemoteWorkspaceTask,
      toast,
    } = createHarness({
      workspaceTask,
      workspaceTaskBlocked: true,
    });

    keyboardActions.advanceStage();

    expect(advanceSelectedRemoteWorkspaceTask).not.toHaveBeenCalled();
    expect(toast.warning).toHaveBeenCalledWith("mainPanel.taskBlocked");
  });

  it("does not advance a selected remote task while its owner is running a post", () => {
    const workspaceTask = remoteWorkspaceTask("cloud:repo:task-remote");
    workspaceTask.item.has_running_post = 1;
    workspaceTask.capabilities = {
      canAdvanceStage: true,
    } as WorkspaceTask["capabilities"];
    const { keyboardActions, advanceSelectedRemoteWorkspaceTask } = createHarness({
      workspaceTask,
    });

    keyboardActions.advanceStage();

    expect(advanceSelectedRemoteWorkspaceTask).not.toHaveBeenCalled();
  });

  it("routes history shortcuts through workspace-aware navigation", async () => {
    const { keyboardActions, navigateBack, navigateForward } = createHarness();

    await keyboardActions.goBack();
    await keyboardActions.goForward();

    expect(navigateBack).toHaveBeenCalledOnce();
    expect(navigateForward).toHaveBeenCalledOnce();
  });
});

describe("app shortcut menu context", () => {
  it("captures the active tool on each opening, including existing tabs and close fallback", () => {
    const h = createHarness();
    h.mainTabs.openTab({ kind: "tree" });
    h.mainTabs.openTab({ kind: "file", filePath: "README.md" });
    h.mainTabs.openTab({ kind: "diff" });
    const open = (context: ShortcutContext) => {
      h.keyboardActions.showShortcuts();
      expect(h.showShortcutsModal.value).toBe(true);
      expect(h.shortcutsContext.value).toBe(context);
      expect(h.shortcutsStartFull.value).toBe(context === "main");
      h.showShortcutsModal.value = false;
    };
    open("diff");
    h.mainTabs.activateTab("tree");
    open("tree");
    h.mainTabs.activateTab("file:README.md");
    open("file");
    h.mainTabs.closeActiveTab();
    open("diff");
    h.mainTabs.closeActiveTab();
    open("tree");
    h.mainTabs.closeActiveTab();
    open("main");
  });

  it.each<ShortcutContext>(["file", "newTask", "transfer"])(
    "preserves %s overlay precedence and its captured context through full-mode toggles",
    (context) => {
      const h = createHarness();
      h.mainTabs.openTab({ kind: "tree" });
      h.overlayContext.value = context;
      h.keyboardActions.showAllShortcuts();
      expect(h.shortcutsContext.value).toBe(context);
      expect(h.shortcutsStartFull.value).toBe(true);
      h.keyboardActions.showShortcuts();
      expect(h.showShortcutsModal.value).toBe(true);
      expect(h.shortcutsStartFull.value).toBe(false);
      expect(h.shortcutsContext.value).toBe(context);
      h.keyboardActions.showAllShortcuts();
      expect(h.shortcutsStartFull.value).toBe(true);
      h.keyboardActions.showAllShortcuts();
      expect(h.showShortcutsModal.value).toBe(false);
      h.overlayContext.value = "main";
      h.keyboardActions.showShortcuts();
      expect(h.shortcutsContext.value).toBe("tree");
    },
  );
});
