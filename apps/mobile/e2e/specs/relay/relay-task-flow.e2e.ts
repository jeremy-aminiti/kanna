import type { Browser } from "webdriverio";
import {
  extractTaskRowId,
  selectors,
  taskMentionedFilesRowSelector
} from "../../helpers/selectors";
import {
  ensureTaskListVisible,
  inspectTerminalWebView,
  waitForRenderedPtyTerminal,
  waitForTaskTerminalLive,
  type PtyTerminalFixture
} from "../smoke/list-detail-back.e2e";
import type { TaskActivity } from "../../../src/lib/api/types";
import { TASK_COMPOSER_MAX_HEIGHT } from "../../../src/screens/taskComposerInput";
import type {
  MobileRelayCompanionFixture,
  RelayTaskOrderingFixture
} from "../../helpers/relay-harness";

const SCREEN_TIMEOUT_MS = 30_000;
const POLL_INTERVAL_MS = 250;
const GEOMETRY_POLL_INTERVAL_MS = 1_000;
const IOS_APP_STATE_NOT_RUNNING = 1;
const TASK_COMPOSER_PLACEHOLDER = "Reply…";
// Deliberately longer than the five-line cap, so a composer that kept growing
// with its content fails the height assertion instead of quietly filling the
// screen.
const TASK_COMPOSER_FIVE_LINE_DRAFT = [
  "First relay line.",
  "Second relay line.",
  "Third relay line.",
  "Fourth relay line.",
  "Fifth relay line."
].join("\n");
const TASK_COMPOSER_MULTILINE_DRAFT = [
  "First relay line.",
  "Second relay line.",
  "Third relay line.",
  "Fourth relay line.",
  "Fifth relay line.",
  "Sixth relay line.",
  "Seventh relay line.",
  "Eighth relay line."
].join("\n");
// Five line-heights plus the input's own vertical padding, with room for iOS
// rounding and the container inset around it.
const TASK_COMPOSER_MAX_RENDERED_HEIGHT = TASK_COMPOSER_MAX_HEIGHT + 24;
const TASK_ACTION_MENU_TITLE = "Task Actions";
const TASK_ACTION_LABELS = [
  "Mentioned Files (0)",
  "View Diff",
  "Advance Stage",
  "Close Task",
  "Cancel"
] as const;

interface RelayCredentials {
  email: string;
  password: string;
}

interface RelayTaskFlowOptions {
  bundleId: string;
  companion: RelayVisualCompanionActions & {
    fixture: MobileRelayCompanionFixture;
  };
  credentials: RelayCredentials;
  emitFilePreviewLinks(): Promise<void>;
  filePreview: RelayFilePreviewFixture;
  draft: string;
  customizedReply: string;
  fixture: PtyTerminalFixture;
  observeAuthoritativeTerminalGeometry(): Promise<{ cols: number; rows: number }>;
  prepareTaskUnreadForMarkRead(): Promise<void>;
  setTaskBusyRead(): Promise<void>;
  restoreTallTerminalGeometry(): Promise<void>;
  restoreDesktopTerminalControl(): Promise<void>;
  dropRelayTunnels(whileDown: () => Promise<void>): Promise<void>;
  resyncTerminalConnection(): Promise<void>;
  setTaskBusyUnread(): Promise<void>;
  setTaskActivity(activity: TaskActivity): Promise<void>;
  taskRow: RelayTaskRowExpectation;
  taskOrdering: RelayTaskOrderingFixture;
  terminalKeys: {
    count(key: "ESC" | "ENTER"): number;
    waitForCount(key: "ESC" | "ENTER", count: number): Promise<void>;
  };
  /** The shared Expo readiness gate: dismisses the dev-client startup
   * overlays and waits out a Metro bundle fetch. A relaunch goes through
   * exactly that startup again, so it must be awaited the same way. */
  captureScreenshot(name: string): Promise<void>;
  waitForAppReady(readySelector?: string): Promise<void>;
  waitForLocalTaskActivity(activity: TaskActivity): Promise<void>;
  waitForMobileTerminalGeometry(): Promise<void>;
  waitForQuickReplyInput(): Promise<void>;
}

export interface RelayVisualCompanionActions {
  disconnect(): Promise<void>;
  expectNoEvent(choice: string): Promise<void>;
  invalidateSource(): Promise<void>;
  reconnect(): Promise<void>;
  restoreSource(): Promise<void>;
  resume(): Promise<void>;
  stop(): Promise<void>;
  waitForEvent(choice: string): Promise<unknown>;
}

export interface RelayVisualCompanionUi {
  clickChoice(choice: string): Promise<void>;
  close(): Promise<void>;
  open(): Promise<void>;
  readDocumentText(): Promise<string>;
  tryClickChoice(choice: string): Promise<boolean>;
  waitForEnded(): Promise<void>;
  waitForNoInteractiveWebView(): Promise<void>;
  waitForReconnecting(): Promise<void>;
  waitForSourceError(message: string): Promise<void>;
  waitUntil(
    condition: () => Promise<boolean>,
    options: { interval: number; timeout: number; timeoutMsg: string }
  ): Promise<unknown>;
}

interface RelayFilePreviewFixture {
  ambiguousBarePath: string;
  ambiguousCanonicalPaths: readonly string[];
  expectedHeading: string;
  expectedHighlightedToken: string;
  expectedHighlightedTokenClass: string;
  expectedRawLine: string;
  expectedRenderedText: string;
  expectedCanonicalRowOrder: readonly string[];
  line: number;
  mentionedCount: number;
  mentionedLinks: readonly string[];
  missingLink: string;
  path: string;
  rawLink: string;
  renderedLink: string;
  uniqueBarePath: string;
  uniqueCanonicalPath: string;
}

export interface RelayTaskRowExpectation {
  originalPromptSnippet: string;
  repoLabel: string;
  stage: string;
  taskId: string;
  title: string;
  waitingPromptSnippet: string;
}

interface RelayElement {
  addValue(value: string): Promise<unknown>;
  click(): Promise<unknown>;
  getAttribute(name: string): Promise<string | null>;
  getSize(): Promise<{ height: number; width: number }>;
  getText(): Promise<string>;
  isExisting(): Promise<boolean>;
  scrollIntoView(options: {
    direction: "down" | "up";
    maxScrolls: number;
  }): Promise<unknown>;
  setValue(value: string): Promise<unknown>;
  waitForDisplayed(options: { timeout: number }): Promise<unknown>;
}

interface RelayQuickReplyPersistenceJourney {
  closeEditor(): Promise<void>;
  getFirstReplyInput(): Promise<Pick<RelayElement, "getAttribute" | "setValue">>;
  openEditor(): Promise<void>;
  relaunchPreservingData(): Promise<void>;
  save(): Promise<void>;
  waitForEditorClosed(): Promise<void>;
  waitUntil(
    condition: () => Promise<boolean>,
    options: {
      interval: number;
      timeout: number;
      timeoutMsg: string;
    }
  ): Promise<unknown>;
}

interface RelayUi {
  getAccountButton(): Promise<RelayElement>;
  getAccountCloseButton(): Promise<RelayElement>;
  getAccountEmailInput(): Promise<RelayElement>;
  getAccountPasswordInput(): Promise<RelayElement>;
  getAccountSheet(): Promise<RelayElement>;
  getAccountSignInButton(): Promise<RelayElement>;
  getAccountSignOutButton(): Promise<RelayElement>;
  getAgentMessageView(): Promise<RelayElement>;
  getAgentMessageReady(): Promise<RelayElement>;
  getBackButton(): Promise<RelayElement>;
  dragFirstQuickReply(): Promise<void>;
  getTaskActionMenuTitle(): Promise<RelayElement>;
  getTaskActionOption(label: string): Promise<RelayElement>;
  getTaskInput(): Promise<RelayElement>;
  getTaskInputStatus(): Promise<RelayElement>;
  getTaskDetailScreen(): Promise<RelayElement>;
  getTaskDetailActivity(): Promise<RelayElement>;
  getTaskMoreButton(): Promise<RelayElement>;
  getRecentTab(): Promise<RelayElement>;
  getTaskRowById(taskId: string): Promise<RelayElement>;
  getTaskRows(): Promise<RelayElement[]>;
  getTasksTab(): Promise<RelayElement>;
  getTaskSendButton(): Promise<RelayElement>;
  getTerminalOverlay(): Promise<RelayElement>;
  getTerminalReconnectBadge(): Promise<RelayElement>;
  inspectTerminalWebView(): ReturnType<typeof inspectTerminalWebView>;
  readTerminalLoadingIndications(): Promise<number>;
  isKeyboardShown(): Promise<boolean>;
  pause(ms: number): Promise<unknown>;
  waitUntil(
    condition: () => Promise<boolean>,
    options: {
      interval: number;
      timeout: number;
      timeoutMsg: string;
    }
  ): Promise<unknown>;
}

interface RelayWebViewContextDriver {
  execute<T>(script: () => T): Promise<T>;
  getNativeInspection?: () => Promise<string | null>;
  getContext?: () => Promise<string>;
  getContexts?: () => Promise<unknown[]>;
  switchContext?: (context: string) => Promise<unknown>;
}

interface TaskFilePreviewInspection {
  content: string;
  initialLine: number | null;
  mode: "raw" | "rendered";
  path: string;
}

export type TaskFilePreviewWebViewInspection =
  | {
      kind: "rendered";
      path: string;
      tokenClass: string;
      tokenColor: string;
      tokenHeight: number;
      tokenText: string;
      tokenWidth: number;
      unhighlightedColor: string;
    }
  | {
      animationName: string;
      flashStarted: boolean;
      kind: "raw";
      line: number | null;
      overlayHeight: number;
      overlayTop: number;
      overlayWidth: number;
      path: string;
    }
  | {
      kind: "unavailable";
      reason: string;
    };

interface RelayPtySnapshotRevisitJourney {
  closeTask(): Promise<void>;
  openTask(): Promise<void>;
  waitForRenderedTerminal(): Promise<void>;
}

async function verifyRelayPtyStableResync(
  ui: Pick<
    RelayUi,
    | "getAgentMessageView"
    | "getTaskDetailScreen"
    | "getTerminalOverlay"
    | "inspectTerminalWebView"
    | "pause"
    | "waitUntil"
  >,
  fixture: PtyTerminalFixture,
  resync: () => Promise<void>,
): Promise<void> {
  const before = await ui.inspectTerminalWebView();
  if (before.kind !== "rendered" || !before.documentInstanceId) {
    throw new Error(`Expected a rendered terminal document before resync: ${JSON.stringify(before)}`);
  }
  await resync();
  await waitForRenderedPtyTerminal(ui, fixture);
  for (let sample = 0; sample < 10; sample += 1) {
    const overlay = await ui.getTerminalOverlay();
    const inspection = await ui.inspectTerminalWebView();
    if (
      await overlay.isExisting() ||
      inspection.kind !== "rendered" ||
      inspection.documentInstanceId !== before.documentInstanceId ||
      inspection.cols !== fixture.expectedCols
    ) {
      throw new Error(
        `Terminal attachment churned after resync at sample ${sample}: ` +
        `${JSON.stringify(inspection)}`,
      );
    }
    await ui.pause(500);
  }
}

async function verifyRelayPtyTunnelDropIsInvisible(
  ui: Pick<
    RelayUi,
    | "getAgentMessageView"
    | "getTaskDetailScreen"
    | "getTerminalOverlay"
    | "getTerminalReconnectBadge"
    | "inspectTerminalWebView"
    | "pause"
    | "readTerminalLoadingIndications"
    | "waitUntil"
  >,
  fixture: PtyTerminalFixture,
  dropTunnels: (whileDown: () => Promise<void>) => Promise<void>,
): Promise<void> {
  const before = await ui.inspectTerminalWebView();
  if (before.kind !== "rendered" || !before.documentInstanceId) {
    throw new Error(
      `Expected a rendered terminal document before the tunnel drop: ${JSON.stringify(before)}`,
    );
  }
  const loadingIndicationsBefore = await ui.readTerminalLoadingIndications();

  await dropTunnels(async () => {
    // Sampled in the first moments of the outage, inside the client's
    // reconnect grace: a gap this short is not news, so the reader must be
    // told nothing at all about it.
    if (await (await ui.getTerminalReconnectBadge()).isExisting()) {
      throw new Error(
        "The connecting badge was shown for a sub-threshold transport gap",
      );
    }
  });

  // The grid is never taken away. Sampling right through the outage and the
  // redial is the whole point: a single blank frame here is the regression.
  for (let sample = 0; sample < 12; sample += 1) {
    const overlay = await ui.getTerminalOverlay();
    const inspection = await ui.inspectTerminalWebView();
    if (
      await overlay.isExisting() ||
      inspection.kind !== "rendered" ||
      inspection.documentInstanceId !== before.documentInstanceId ||
      inspection.cols !== fixture.expectedCols
    ) {
      throw new Error(
        `Terminal blanked or churned across the tunnel drop at sample ${sample}: ` +
        `${JSON.stringify(inspection)}`,
      );
    }
    await ui.pause(500);
  }

  await waitForRenderedPtyTerminal(ui, fixture);
  const loadingIndications = await ui.readTerminalLoadingIndications();
  if (loadingIndications > loadingIndicationsBefore + 1) {
    throw new Error(
      `One reconnect raised ${loadingIndications - loadingIndicationsBefore} ` +
      "loading indications; at most one is allowed",
    );
  }
  if (await (await ui.getTerminalReconnectBadge()).isExisting()) {
    throw new Error("The connecting badge outlived the reconnect");
  }
}

interface RelayTaskJourneys {
  verifyQuickReplyPersistence(): Promise<void>;
  verifyComposerReset(): Promise<void>;
  verifyFilePreview(): Promise<void>;
  verifyMarkedRead(): Promise<void>;
  verifyMobileTerminalControl(): Promise<void>;
  verifySendOutcomes(): Promise<void>;
  verifyPtySnapshotRevisit(): Promise<void>;
  verifyQuickReply(): Promise<void>;
  verifyTaskActionMenu(): Promise<void>;
  verifyTerminalKeys(): Promise<void>;
  verifyVisualCompanion(): Promise<void>;
}

export async function runRelayTaskJourneys(
  journeys: RelayTaskJourneys,
): Promise<void> {
  await journeys.verifyQuickReplyPersistence();
  await journeys.verifyMarkedRead();
  // Grid ownership is exercised before the revisit journey, whose stability
  // check restarts the daemon underneath the session: an observer opened
  // while that socket is gone sees no snapshot at all.
  await journeys.verifyMobileTerminalControl();
  // Both send outcomes are asserted early, while the lane is still healthy:
  // they are what this change owes, and the journeys after them are known to
  // move around between runs.
  await journeys.verifySendOutcomes();
  // The composer's five-line cap and its one-line reset after Send are native
  // layout facts no jsdom test can show, so they run here rather than last:
  // across twelve lane runs on this branch the tail was never reached, and
  // the checks silently never executed.
  await journeys.verifyComposerReset();
  await journeys.verifyPtySnapshotRevisit();
  // Exercise file discovery immediately after the terminal revisit, before
  // later menus can change the detail presentation state.
  await journeys.verifyFilePreview();
  await journeys.verifyTerminalKeys();
  await journeys.verifyQuickReply();
  await journeys.verifyTaskActionMenu();
  await journeys.verifyVisualCompanion();
}

async function verifyRelayTerminalKeys(
  driver: Browser,
  observation: RelayTaskFlowOptions["terminalKeys"]
): Promise<void> {
  const directInputToggle = await driver.$(
    selectors.taskTerminalDirectInputToggle
  );
  await directInputToggle.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await directInputToggle.click();
  const escape = await driver.$(selectors.taskTerminalKey("escape"));
  const enter = await driver.$(selectors.taskTerminalKey("enter"));
  await escape.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await enter.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  if (!(await escape.isEnabled()) || !(await enter.isEnabled())) {
    throw new Error("Expected terminal keys to enable for the authenticated relay PTY");
  }

  const escapeCount = observation.count("ESC");
  const enterCount = observation.count("ENTER");
  await escape.click();
  await observation.waitForCount("ESC", escapeCount + 1);
  await driver.pause(750);
  if (observation.count("ENTER") !== enterCount) {
    throw new Error("Esc delivered a trailing Enter to the desktop PTY");
  }

  await enter.click();
  await observation.waitForCount("ENTER", enterCount + 1);

  await directInputToggle.click();
  await (await driver.$(selectors.taskInput)).waitForDisplayed({
    timeout: SCREEN_TIMEOUT_MS,
  });
}

function createRelayQuickReplyPersistenceJourney(
  driver: Browser,
  ui: RelayUi,
  bundleId: string,
  waitForAppReady: (readySelector?: string) => Promise<void>,
): RelayQuickReplyPersistenceJourney {
  const openEditor = async () => {
    await openRelayProfileSheet(ui);
    const quickRepliesButton = await driver.$(
      selectors.accountQuickRepliesButton,
    );
    await quickRepliesButton.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    await ui.waitUntil(
      async () => await quickRepliesButton.isEnabled().catch(() => false),
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg: "Expected Quick Replies to enable after preferences hydrated",
      },
    );
    await quickRepliesButton.click();
    await (await driver.$(selectors.quickReplyEditor)).waitForDisplayed({
      timeout: SCREEN_TIMEOUT_MS,
    });
  };

  return {
    async closeEditor() {
      const cancel = await driver.$(selectors.quickReplyEditorCancel);
      await cancel.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
      await cancel.click();
    },
    async getFirstReplyInput() {
      const inputs = Array.from(
        await driver.$$(selectors.quickReplyEditorInputsXPath),
      );
      const firstInput = inputs[0];
      if (!firstInput) {
        throw new Error("Expected at least one ordered quick reply input");
      }
      await firstInput.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
      return firstInput;
    },
    openEditor,
    async relaunchPreservingData() {
      await relaunchRelayAppPreservingData(driver, bundleId);
      await dismissSavePasswordPrompt(driver);
      // A relaunched dev client refetches its bundle from Metro and can put
      // the Expo startup overlays back up. A bare 30s wait on the shell loses
      // that race on a busy machine and cannot dismiss an overlay it does not
      // know about; the readiness gate handles both.
      await waitForAppReady();
      await returnToTaskListShell(ui);
    },
    async save() {
      const done = await driver.$(selectors.quickReplyEditorDone);
      await done.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
      await done.click();
    },
    async waitForEditorClosed() {
      await ui.waitUntil(
        async () =>
          !(await (await driver.$(selectors.quickReplyEditor))
            .isExisting()
            .catch(() => false)),
        {
          interval: POLL_INTERVAL_MS,
          timeout: SCREEN_TIMEOUT_MS,
          timeoutMsg: "Expected Quick Replies to close after AsyncStorage save",
        },
      );
    },
    waitUntil: (condition, options) => ui.waitUntil(condition, options),
  };
}

export async function verifyRelayPtySnapshotRevisit(
  journey: RelayPtySnapshotRevisitJourney,
): Promise<void> {
  await journey.openTask();
  await journey.waitForRenderedTerminal();
  await journey.closeTask();
  await journey.openTask();
  await journey.waitForRenderedTerminal();
}

/**
 * Executable mobile rendered-grid coverage for the geometry change. The
 * snapshot sentinel alone proves bytes arrived, but not that the WebView
 * hydrated the source grid or retained a valid cursor after the authoritative
 * resize snapshot. The E2E inspection is emitted by the real xterm document
 * and read through Appium's WebView/native bridge.
 */
export async function verifyRelayPtyRenderedGridAndCursor(
  ui: Pick<RelayUi, "inspectTerminalWebView" | "waitUntil">,
  fixture: PtyTerminalFixture,
): Promise<void> {
  let lastInspection: Awaited<ReturnType<RelayUi["inspectTerminalWebView"]>> | null = null;
  await ui.waitUntil(
    async () => {
      lastInspection = await ui.inspectTerminalWebView();
      if (lastInspection.kind !== "rendered") return false;
      const cursorColumn = lastInspection.cursorColumn;
      const cursorRow = lastInspection.cursorRow;
      const expectedCell = fixture.expectedCell;
      const expectedCursor = fixture.expectedCursor;
      const bottomLayoutMatches = !fixture.expectBottomAnchored || (
        typeof lastInspection.gridTopGap === "number" &&
        lastInspection.gridTopGap > 0 &&
        typeof lastInspection.gridBottomGap === "number" &&
        lastInspection.gridBottomGap <= 1
      );
      const renderedCell = expectedCell && lastInspection.visibleRows
        ? lastInspection.visibleRows[expectedCell.row]?.slice(
            expectedCell.column,
            expectedCell.column + expectedCell.text.length,
          )
        : undefined;
      return (
        lastInspection.cols === fixture.expectedCols &&
        lastInspection.rows === fixture.expectedRows &&
        lastInspection.text.includes(fixture.sentinel) &&
        typeof cursorColumn === "number" &&
        Number.isInteger(cursorColumn) &&
        typeof cursorRow === "number" &&
        Number.isInteger(cursorRow) &&
        cursorColumn >= 0 &&
        cursorColumn < fixture.expectedCols &&
        cursorRow >= 0 &&
        cursorRow < fixture.expectedRows &&
        (!expectedCell || renderedCell === expectedCell.text) &&
        (!expectedCursor ||
          (cursorColumn === expectedCursor.column && cursorRow === expectedCursor.row)) &&
        bottomLayoutMatches
      );
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        `Expected the mobile WebView to render ${fixture.expectedCols}x${fixture.expectedRows} ` +
        `with cursor ${JSON.stringify(fixture.expectedCursor)} and cell ` +
        `${JSON.stringify(fixture.expectedCell)}, excess space above the grid, and ` +
        `its last row adjacent to the input chrome; last inspection ${JSON.stringify(lastInspection)}`,
    },
  );
}

export async function verifyRelayPtyAuthoritativeScrollback(
  driver: Browser,
  ui: Pick<RelayUi, "inspectTerminalWebView" | "waitUntil">,
  fixture: PtyTerminalFixture,
): Promise<void> {
  const scrollTop = await driver.$(selectors.terminalScrollTop);
  await scrollTop.waitForExist({ timeout: SCREEN_TIMEOUT_MS });
  const inspectionBeforeScroll = await (await driver.$(selectors.terminalInspection))
    .getAttribute("value")
    .catch(() => null);
  await scrollTop.click();

  // Activating the native scroll affordance can briefly detach the WebView
  // accessibility bridge. Wait for that exact inspection surface to return
  // before asking the context inspector for the xterm grid.
  await driver.waitUntil(
    async () => {
      const inspection = await driver.$(selectors.terminalInspection);
      const value = await inspection.getAttribute("value").catch(() => null);
      return value !== null && value !== inspectionBeforeScroll;
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: "Expected fresh terminal inspection after scroll-to-top",
    },
  );

  const expectedHistoryRow = /^MOBILE_PTY_HISTORY_\d{5}_X{100}$/;
  let lastInspection: Awaited<ReturnType<RelayUi["inspectTerminalWebView"]>> | null = null;
  await ui.waitUntil(
    async () => {
      lastInspection = await ui.inspectTerminalWebView();
      if (lastInspection.kind !== "rendered") return false;
      const visibleRows = (lastInspection.visibleRows ?? []).filter(Boolean);
      const historyRows = visibleRows.filter((row) => row.startsWith("MOBILE_PTY_HISTORY_"));
      return (
        lastInspection.cols === fixture.expectedCols &&
        historyRows.length >= 10 &&
        historyRows.every((row) =>
          row.length <= fixture.expectedCols && expectedHistoryRow.test(row)
        ) &&
        !visibleRows.some((row) => /^X+$/.test(row))
      );
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        `Expected scrollback to remain uniformly wrapped at authoritative ` +
        `${fixture.expectedCols} columns without orphan fragments; last inspection ` +
        `${JSON.stringify(lastInspection)}`,
    },
  );
}

async function dismissSavePasswordPrompt(driver: Browser): Promise<void> {
  for (const selector of [
    "~Not Now",
    '-ios predicate string:name == "Not Now" OR label == "Not Now"'
  ]) {
    const notNow = await driver.$(selector);
    const isVisible = await notNow
      .waitForDisplayed({ timeout: 2_500 })
      .then(() => true)
      .catch(() => false);
    if (isVisible) {
      await notNow.click();
      return;
    }
  }
}

async function isTaskVisible(ui: RelayUi, taskId: string): Promise<boolean> {
  const task = await ui.getTaskRowById(taskId);
  return task
    .waitForDisplayed({ timeout: 15_000 })
    .then(() => true)
    .catch(() => false);
}

export async function performFirstQuickReplyDrag(
  driver: Browser,
): Promise<void> {
  const send = await driver.$(selectors.taskSendButton);
  const [location, size] = await Promise.all([
    send.getLocation(),
    send.getSize()
  ]);
  const centerX = Math.round(location.x + size.width / 2);
  const centerY = Math.round(location.y + size.height / 2);

  await driver.execute("mobile: dragFromToForDuration", {
    duration: 0.65,
    fromX: centerX,
    fromY: centerY,
    toX: centerX,
    toY: centerY - 52,
  });
}

export async function relaunchRelayAppPreservingData(
  driver: Browser,
  bundleId: string,
): Promise<void> {
  await driver.terminateApp(undefined, bundleId);
  await driver.waitUntil(
    async () =>
      await driver.queryAppState(undefined, bundleId) ===
        IOS_APP_STATE_NOT_RUNNING,
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: `Expected ${bundleId} to terminate before relaunch`,
    },
  );
  await driver.activateApp(undefined, bundleId);
}

export async function verifyRelayQuickReplyPersistenceJourney(
  journey: RelayQuickReplyPersistenceJourney,
  customizedReply: string,
): Promise<void> {
  await journey.openEditor();
  const input = await journey.getFirstReplyInput();
  await input.setValue(customizedReply);
  await journey.save();
  await journey.waitForEditorClosed();

  await journey.relaunchPreservingData();

  await journey.openEditor();
  const reloadedInput = await journey.getFirstReplyInput();
  let lastObserved: string | null = null;
  await journey.waitUntil(
    async () => {
      lastObserved = await reloadedInput.getAttribute("value").catch(() => null);
      if (lastObserved === customizedReply) return true;
      lastObserved = await reloadedInput.getAttribute("label").catch(() => null);
      return lastObserved === customizedReply;
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        `Expected customized quick reply ${JSON.stringify(customizedReply)} ` +
        `after data-preserving relaunch; last native value was ${JSON.stringify(lastObserved)}`,
    },
  );
  await journey.closeEditor();
}

function createRelayUi(driver: Browser): RelayUi {
  return {
    async getAccountButton() {
      return driver.$(selectors.accountButton);
    },
    async getAccountCloseButton() {
      return driver.$(selectors.accountCloseButton);
    },
    async getAccountEmailInput() {
      return driver.$(selectors.accountEmailInput);
    },
    async getAccountPasswordInput() {
      return driver.$(selectors.accountPasswordInput);
    },
    async getAccountSheet() {
      return driver.$(selectors.accountSheet);
    },
    async getAccountSignInButton() {
      return driver.$(selectors.accountSignInButton);
    },
    async getAccountSignOutButton() {
      return driver.$(selectors.accountSignOutButton);
    },
    async getAgentMessageView() {
      return driver.$(selectors.agentMessageView);
    },
    async getAgentMessageReady() {
      return driver.$(selectors.agentMessageReady);
    },
    async getBackButton() {
      return driver.$(selectors.taskBackButton);
    },
    async dragFirstQuickReply() {
      await performFirstQuickReplyDrag(driver);
    },
    async getTaskActionMenuTitle() {
      return driver.$(`~${TASK_ACTION_MENU_TITLE}`);
    },
    async getTaskActionOption(label) {
      if (label.startsWith("Mentioned Files (")) {
        return driver.$(
          '-ios predicate string:name BEGINSWITH "Mentioned Files (" OR label BEGINSWITH "Mentioned Files ("'
        );
      }
      return driver.$(`~${label}`);
    },
    async getTaskInput() {
      return driver.$(selectors.taskInput);
    },
    async getTaskInputStatus() {
      return driver.$(selectors.taskInputStatus);
    },
    async getTaskDetailScreen() {
      return driver.$(selectors.taskDetailScreen);
    },
    async getTaskDetailActivity() {
      return driver.$(selectors.taskTitleButton);
    },
    async getTaskMoreButton() {
      return driver.$(selectors.taskMoreButton);
    },
    async getRecentTab() {
      return driver.$(selectors.recentTab);
    },
    async getTaskRowById(taskId) {
      return driver.$(`~mobile.task-row.${taskId}`);
    },
    async getTaskRows() {
      return Array.from(await driver.$$(selectors.taskRowsXPath));
    },
    async getTasksTab() {
      return driver.$(selectors.tasksTab);
    },
    async getTaskSendButton() {
      return driver.$(selectors.taskSendButton);
    },
    async getTerminalOverlay() {
      return driver.$(selectors.terminalOverlay);
    },
    async getTerminalReconnectBadge() {
      return driver.$(selectors.terminalReconnectBadge);
    },
    async inspectTerminalWebView() {
      return inspectTerminalWebView(createWebViewContextDriver(driver));
    },
    async readTerminalLoadingIndications() {
      const marker = await driver.$(selectors.terminalLoadingIndications);
      const label = await marker.getAttribute("label").catch(() => null);
      const parts = String(label ?? "").split(":");
      const count = Number(parts[parts.length - 1]);
      if (!Number.isInteger(count)) {
        throw new Error(
          `Terminal loading-indication counter was unreadable: ${String(label)}`,
        );
      }
      return count;
    },
    async isKeyboardShown() {
      return driver.isKeyboardShown();
    },
    async pause(ms) {
      return driver.pause(ms);
    },
    async waitUntil(condition, options) {
      return driver.waitUntil(condition, options);
    }
  };
}

export async function verifyRelayTaskActionMenuJourney(
  ui: Pick<
    RelayUi,
    | "getTaskActionMenuTitle"
    | "getTaskActionOption"
    | "getTaskMoreButton"
  >,
  options: {
    dismissWithoutCancel?: () => Promise<void>;
  } = {},
): Promise<void> {
  const taskMore = await ui.getTaskMoreButton();
  await taskMore.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await taskMore.click();

  const menuTitle = await ui.getTaskActionMenuTitle();
  await menuTitle.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const visibleLabels = options.dismissWithoutCancel
    ? TASK_ACTION_LABELS.filter((label) => label !== "Cancel")
    : TASK_ACTION_LABELS;
  for (const label of visibleLabels) {
    const option = await ui.getTaskActionOption(label);
    await option.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    if (label === "Cancel") {
      await option.click();
    }
  }
  await options.dismissWithoutCancel?.();

  await taskMore.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
}

export async function verifyRelayComposerResetJourney(
  ui: Pick<
    RelayUi,
    | "getTaskInput"
    | "getTaskInputStatus"
    | "getTaskSendButton"
    | "isKeyboardShown"
    | "waitUntil"
  >,
  actions: { captureScreenshot(name: string): Promise<void> },
): Promise<void> {
  const input = await ui.getTaskInput();
  await input.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const initialHeight = (await input.getSize()).height;

  // Exactly the cap: five lines is the tallest the composer may render.
  await input.click();
  await input.setValue(TASK_COMPOSER_FIVE_LINE_DRAFT);
  let fiveLineHeight = initialHeight;
  await ui.waitUntil(
    async () => {
      fiveLineHeight = (await input.getSize()).height;
      return fiveLineHeight > initialHeight && await ui.isKeyboardShown();
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: "Expected the composer to grow to five lines",
    },
  );
  await actions.captureScreenshot("05-composer-five-lines");

  // A sixth line and beyond scrolls inside the input rather than growing it.
  await input.setValue(TASK_COMPOSER_MULTILINE_DRAFT);

  let expandedHeight = initialHeight;
  await ui.waitUntil(
    async () => {
      expandedHeight = (await input.getSize()).height;
      return expandedHeight > initialHeight && await ui.isKeyboardShown();
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        "Expected the multiline task composer to expand with the software keyboard shown",
    },
  );

  // Five lines is the cap, and past it the input scrolls itself rather than
  // eating the screen. The draft is far longer than five lines, so a composer
  // that kept growing would fail here.
  if (expandedHeight > TASK_COMPOSER_MAX_RENDERED_HEIGHT) {
    throw new Error(
      `Expected the composer to stop growing at five lines (<= ` +
        `${TASK_COMPOSER_MAX_RENDERED_HEIGHT}pt); it rendered ${expandedHeight}pt`,
    );
  }

  if (expandedHeight > fiveLineHeight) {
    throw new Error(
      `Expected a draft past the cap to scroll inside the input, not grow it: ` +
        `five lines rendered ${fiveLineHeight}pt, eight lines ${expandedHeight}pt`,
    );
  }
  await actions.captureScreenshot("06-composer-past-cap-scrolling");

  const send = await ui.getTaskSendButton();
  await send.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await send.click();

  let lastValue: string | null = null;
  let lastLabel: string | null = null;
  let lastResetHeight = expandedHeight;
  let lastKeyboardShown = true;
  try {
    await ui.waitUntil(
      async () => {
        lastValue = await input.getAttribute("value").catch(() => null);
        lastLabel = lastValue === null
          ? await input.getAttribute("label").catch(() => null)
          : null;
        lastResetHeight = (await input.getSize()).height;
        lastKeyboardShown = await ui.isKeyboardShown();
        const cleared =
          lastValue === "" ||
          lastValue === TASK_COMPOSER_PLACEHOLDER ||
          lastLabel === TASK_COMPOSER_PLACEHOLDER;

        return (
          cleared &&
          lastResetHeight <= initialHeight &&
          lastResetHeight < expandedHeight &&
          !lastKeyboardShown
        );
      },
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg:
          "Expected Send to clear, return to one-line height, and hide the keyboard",
      },
    );
  } catch {
    throw new Error(
      "Expected Send to clear, return to one-line height, and hide the keyboard; " +
        `value=${JSON.stringify(lastValue)}, label=${JSON.stringify(lastLabel)}, ` +
        `height=${lastResetHeight} (initial=${initialHeight}, expanded=${expandedHeight}), ` +
        `keyboardShown=${lastKeyboardShown}`,
    );
  }

  // A send that landed says nothing. The notice this replaced appeared on
  // every message, and two of them stacked pushed the composer down the
  // screen. The cleared composer is the confirmation.
  const status = await ui.getTaskInputStatus();
  if (await status.isExisting()) {
    throw new Error(
      "Expected no delivery notice after a successful send; found one labelled " +
        `${JSON.stringify(await status.getAttribute("label").catch(() => null))}`,
    );
  }

  await actions.captureScreenshot("07-composer-after-send-one-line");
  process.stdout.write(
    `[mobile-e2e] composer reset passed: one line ${initialHeight}pt, five lines ` +
      `${fiveLineHeight}pt, past the cap ${expandedHeight}pt, grown to ` +
      `${expandedHeight}pt within the ${TASK_COMPOSER_MAX_RENDERED_HEIGHT}pt cap, ` +
      `back to ${lastResetHeight}pt after Send\n`,
  );
}

/**
 * What a send says. A delivered send says nothing at all — the cleared
 * composer is the confirmation, and the notice this replaced appeared on every
 * message and pushed the composer down the screen. A send that genuinely did
 * not reach the desktop still has to say so, in a sentence rather than the
 * response body it used to print at the owner, and must keep the text.
 */
export async function verifyRelaySendOutcomesJourney(
  ui: Pick<
    RelayUi,
    "getTaskInput" | "getTaskInputStatus" | "getTaskSendButton" | "waitUntil"
  >,
  actions: {
    captureScreenshot(name: string): Promise<void>;
  },
): Promise<void> {
  const input = await ui.getTaskInput();
  await input.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const send = await ui.getTaskSendButton();

  await input.click();
  await input.setValue("Delivered send, which should say nothing.");
  await actions.captureScreenshot("03-composer-before-send");
  await send.click();

  await ui.waitUntil(
    async () => {
      const value = await input.getAttribute("value").catch(() => null);
      const label = await input.getAttribute("label").catch(() => null);
      return value === "" || value === TASK_COMPOSER_PLACEHOLDER ||
        label === TASK_COMPOSER_PLACEHOLDER;
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: "Expected a delivered send to clear the composer",
    },
  );
  const quiet = await ui.getTaskInputStatus();
  if (await quiet.isExisting()) {
    throw new Error(
      "Expected a delivered send to raise no notice; found one labelled " +
        `${JSON.stringify(await quiet.getAttribute("label").catch(() => null))}`,
    );
  }
  await actions.captureScreenshot("04-after-delivered-send-no-banner");

  // The other half of this contract — a send that genuinely fails still says
  // so, in a sentence rather than a response body, keeping the text — is
  // asserted in TaskScreen.attachment.test.tsx rather than here. It is not
  // inducible from outside the app: stopping the desktop disables the
  // composer before a send can be offered, and replacing the daemon lets the
  // app recover the session before the refusal lands. Both were tried against
  // this harness. Rendering that toast needs the failure injected at the send
  // callback, which is exactly what the component test does.
  process.stdout.write("[mobile-e2e] send outcomes passed\n");
}

/**
 * Opening the rendered phone terminal makes its measured viewport active.
 */
export async function verifyRelayMobileTerminalControlJourney(
  _driver: Browser,
  ui: Pick<RelayUi, "inspectTerminalWebView" | "waitUntil">,
  fixture: PtyTerminalFixture,
  actions: {
    captureScreenshot(name: string): Promise<void>;
    observeAuthoritativeTerminalGeometry(): Promise<{ cols: number; rows: number }>;
    restoreDesktopTerminalControl(): Promise<void>;
  },
): Promise<void> {
  const taken = await actions.observeAuthoritativeTerminalGeometry();
  if (taken.cols >= fixture.expectedCols) {
    throw new Error(
      `Expected the phone's measured grid to be narrower than the desktop's ` +
        `${fixture.expectedCols} columns; opening selected ${taken.cols}x${taken.rows}`,
    );
  }
  if (taken.cols < 20 || taken.rows < 8) {
    throw new Error(
      `Expected a readable measured grid, not a still-settling layout; ` +
        `opening selected ${taken.cols}x${taken.rows}`,
    );
  }

  process.stdout.write(
    `[mobile-e2e] phone active view: daemon grid ${taken.cols}x${taken.rows} ` +
      `(measured on this device at its current zoom)\n`,
  );

  // Every renderer still shows the daemon's grid, which is now the phone's.
  let lastInspection: Awaited<ReturnType<RelayUi["inspectTerminalWebView"]>> | null = null;
  await ui.waitUntil(
    async () => {
      lastInspection = await ui.inspectTerminalWebView();
      return (
        lastInspection.kind === "rendered" &&
        lastInspection.cols === taken.cols &&
        lastInspection.rows === taken.rows
      );
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        `Expected the WebView to render the grid it now owns (${taken.cols}x${taken.rows}); ` +
        `last inspection ${JSON.stringify(lastInspection)}`,
    },
  );

  await actions.captureScreenshot("02-terminal-fitted-after-phone-open");
  await actions.restoreDesktopTerminalControl();
  await ui.waitUntil(
    async () => {
      const released = await actions.observeAuthoritativeTerminalGeometry();
      return (
        released.cols === fixture.expectedCols && released.rows === fixture.expectedRows
      );
    },
    {
      interval: GEOMETRY_POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        "Expected making the desktop terminal active to restore the desktop grid",
    },
  );
  await verifyRelayPtyRenderedGridAndCursor(ui, fixture);
  process.stdout.write(
    `[mobile-e2e] terminal active-view ownership passed at ${taken.cols}x${taken.rows}\n`,
  );
}

export async function verifyRelayQuickReplyJourney(
  ui: Pick<
    RelayUi,
    | "dragFirstQuickReply"
    | "getTaskInput"
    | "getTaskSendButton"
    | "waitUntil"
  >,
  draft: string,
): Promise<void> {
  const input = await ui.getTaskInput();
  await input.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await input.setValue(draft);

  const send = await ui.getTaskSendButton();
  await send.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await ui.dragFirstQuickReply();

  await ui.waitUntil(
    async () => {
      const value = await input.getAttribute("value").catch(() => null);
      if (value === "" || value === TASK_COMPOSER_PLACEHOLDER) {
        return true;
      }
      if (value !== null) {
        return false;
      }

      const label = await input.getAttribute("label").catch(() => null);
      return label === TASK_COMPOSER_PLACEHOLDER;
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: "Expected the task composer to clear after selecting a quick reply",
    },
  );
}

export async function verifyRelayCustomizedQuickReplyJourney(
  ui: Parameters<typeof verifyRelayQuickReplyJourney>[0],
  draft: string,
  waitForQuickReplyInput: () => Promise<void>,
): Promise<void> {
  await verifyRelayQuickReplyJourney(ui, draft);
  await waitForQuickReplyInput();
}

function createWebViewContextDriver(driver: Browser): RelayWebViewContextDriver {
  return {
    execute: async <T>(script: () => T) => {
      return await driver.execute(script) as T;
    },
    getContext: driver.getContext
      ? async () => String(await driver.getContext?.())
      : undefined,
    getContexts: driver.getContexts
      ? async () => await driver.getContexts?.() ?? []
      : undefined,
    getNativeInspection: async () => {
      const marker = await driver.$(selectors.terminalInspection);
      return marker.getAttribute("value").catch(() => null);
    },
    switchContext: driver.switchContext
      ? async (context: string) => await driver.switchContext?.(context)
      : undefined
  };
}

async function withVisualCompanionWebView<T>(
  driver: Browser,
  action: () => Promise<T>
): Promise<T> {
  if (!driver.getContexts || !driver.switchContext) {
    throw new Error("Appium did not expose WebView context switching");
  }
  const previousContext = driver.getContext
    ? String(await driver.getContext())
    : "NATIVE_APP";
  const contexts = Array.from(await driver.getContexts()).map(String);
  try {
    for (const context of contexts) {
      if (!context.includes("WEBVIEW")) continue;
      await driver.switchContext(context);
      const isCompanion = await driver.execute(() =>
        Boolean(document.querySelector("#kanna-companion-bridge"))
      );
      if (isCompanion) return await action();
    }
  } finally {
    await driver.switchContext(previousContext);
  }
  throw new Error(
    `No visual companion WebView context was available. Contexts: ${contexts.join(", ")}`
  );
}

export function createVisualCompanionUi(driver: Browser): RelayVisualCompanionUi {
  return {
    async open() {
      const button = await driver.$(selectors.visualCompanionButton);
      await button.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
      await button.click();
      const modal = await driver.$(selectors.visualCompanionModal);
      await modal.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    },
    async close() {
      const close = await driver.$(selectors.visualCompanionClose);
      await close.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
      await close.click();
      await driver.waitUntil(
        async () =>
          !(await driver
            .$(selectors.visualCompanionModal)
            .isExisting()
            .catch(() => false)),
        {
          interval: POLL_INTERVAL_MS,
          timeout: SCREEN_TIMEOUT_MS,
          timeoutMsg: "Expected the visual companion modal to close"
        }
      );
    },
    async readDocumentText() {
      return withVisualCompanionWebView(driver, async () =>
        String(await driver.execute(() => document.body.innerText))
      );
    },
    async clickChoice(choice) {
      if (!/^[a-zA-Z0-9_-]+$/.test(choice)) {
        throw new Error(`Unsafe companion fixture choice ${JSON.stringify(choice)}`);
      }
      await withVisualCompanionWebView(driver, async () => {
        const element = await driver.$(`[data-choice="${choice}"]`);
        await element.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
        await element.click();
      });
    },
    async tryClickChoice(choice) {
      try {
        await this.clickChoice(choice);
        return true;
      } catch {
        return false;
      }
    },
    waitForEnded: () =>
      expectNativeText(
        driver,
        selectors.visualCompanionStatus,
        "This visual companion has ended."
      ),
    async waitForNoInteractiveWebView() {
      await driver.waitUntil(
        async () =>
          !(await driver
            .$(selectors.visualCompanionWebView)
            .isExisting()
            .catch(() => false)),
        {
          interval: POLL_INTERVAL_MS,
          timeout: SCREEN_TIMEOUT_MS,
          timeoutMsg:
            "Expected the failed visual companion WebView to be removed"
        }
      );
    },
    waitForReconnecting: () =>
      expectNativeText(
        driver,
        selectors.visualCompanionStatus,
        "Reconnecting to visual companion…"
      ),
    waitForSourceError: (message) =>
      expectNativeText(driver, selectors.visualCompanionStatus, message),
    waitUntil: (condition, options) => driver.waitUntil(condition, options)
  };
}

async function waitForCompanionMarker(
  ui: Pick<RelayVisualCompanionUi, "readDocumentText" | "waitUntil">,
  marker: string
): Promise<void> {
  let lastText = "";
  await ui.waitUntil(
    async () => {
      lastText = await ui.readDocumentText().catch(() => "");
      return lastText.includes(marker);
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        `Expected visual companion marker ${JSON.stringify(marker)}; ` +
        `last document text ${JSON.stringify(lastText)}`
    }
  );
}

export async function verifyRelayVisualCompanionJourney(
  ui: RelayVisualCompanionUi,
  fixture: MobileRelayCompanionFixture,
  actions: RelayVisualCompanionActions
): Promise<void> {
  await ui.open();
  await waitForCompanionMarker(ui, fixture.initialMarker);
  await actions.invalidateSource();
  await ui.waitForSourceError(fixture.sourceErrorMessage);
  await ui.waitForNoInteractiveWebView();
  if (await ui.tryClickChoice(fixture.choice)) {
    throw new Error("A stale visual companion choice remained interactive after a source error");
  }
  await actions.expectNoEvent(fixture.choice);

  await actions.restoreSource();
  await waitForCompanionMarker(ui, fixture.updatedMarker);

  await actions.disconnect();
  await ui.waitForReconnecting();
  if (await ui.tryClickChoice(fixture.choice)) {
    throw new Error("A stale visual companion choice remained interactive offline");
  }
  await actions.expectNoEvent(fixture.choice);

  await actions.reconnect();
  await waitForCompanionMarker(ui, fixture.updatedMarker);
  await actions.expectNoEvent(fixture.choice);
  await ui.clickChoice(fixture.choice);
  await actions.waitForEvent(fixture.choice);

  await actions.stop();
  await ui.waitForEnded();

  await actions.resume();
  await waitForCompanionMarker(ui, fixture.updatedMarker);
  await ui.close();
}

function webViewContextName(context: unknown): string | null {
  if (typeof context === "string") return context;
  if (!context || typeof context !== "object") return null;

  const record = context as Record<string, unknown>;
  for (const key of ["id", "name"]) {
    if (typeof record[key] === "string") return record[key];
  }
  return null;
}

export async function inspectTaskFilePreviewWebView(
  driver: RelayWebViewContextDriver
): Promise<TaskFilePreviewWebViewInspection> {
  if (!driver.getContexts || !driver.switchContext) {
    return {
      kind: "unavailable",
      reason: "Appium driver does not expose WebView context APIs"
    };
  }

  const contexts = await driver.getContexts();
  const webViewContexts = contexts
    .map(webViewContextName)
    .filter(
      (context): context is string => Boolean(context?.includes("WEBVIEW"))
    );
  if (webViewContexts.length === 0) {
    return {
      kind: "unavailable",
      reason: `No WEBVIEW context was available. Contexts: ${contexts
        .map(webViewContextName)
        .filter(Boolean)
        .join(", ") || "<none>"}`
    };
  }

  const previousContext = driver.getContext ? await driver.getContext() : null;
  let inspection: Exclude<
    TaskFilePreviewWebViewInspection,
    { kind: "unavailable" }
  > | null = null;
  const failures: string[] = [];

  try {
    for (const context of webViewContexts) {
      try {
        await driver.switchContext(context);
        inspection = await driver.execute(() => {
          const path = document
            .querySelector<HTMLElement>(".document-path")
            ?.textContent?.trim();
          if (!path) return null;

          const raw = document.querySelector<HTMLElement>(".raw");
          if (raw) {
            const overlay = raw.querySelector<HTMLElement>(".raw-line");
            const bounds = overlay?.getBoundingClientRect();
            return {
              animationName: overlay ? getComputedStyle(overlay).animationName : "",
              flashStarted: overlay?.dataset.flashStarted === "true",
              kind: "raw" as const,
              line: overlay
                ? Number.parseInt(overlay.dataset.line ?? "", 10) || null
                : null,
              overlayHeight: bounds?.height ?? 0,
              overlayTop: bounds?.top ?? 0,
              overlayWidth: bounds?.width ?? 0,
              path
            };
          }

          const token = document.querySelector<HTMLElement>(
            '.markdown [class^="hljs-"], .markdown [class*=" hljs-"]'
          );
          if (!token) return null;
          const tokenBounds = token.getBoundingClientRect();
          const tokenClass = Array.from(token.classList).find((className) =>
            className.startsWith("hljs-")
          ) ?? "";
          const unhighlightedElement = token.closest("code") ?? token.parentElement;
          return {
            kind: "rendered" as const,
            path,
            tokenClass,
            tokenColor: getComputedStyle(token).color,
            tokenHeight: tokenBounds.height,
            tokenText: token.textContent ?? "",
            tokenWidth: tokenBounds.width,
            unhighlightedColor: unhighlightedElement
              ? getComputedStyle(unhighlightedElement).color
              : getComputedStyle(document.body).color
          };
        });
        if (inspection) break;
      } catch (error) {
        failures.push(
          `${context}: ${error instanceof Error ? error.message : String(error)}`
        );
      }
    }
  } finally {
    if (previousContext) await driver.switchContext(previousContext);
  }

  return inspection ?? {
    kind: "unavailable",
    reason:
      "No WebView document contained a rendered task file preview" +
      (failures.length > 0 ? ` (${failures.join("; ")})` : "")
  };
}

async function inspectTaskFilePreview(
  driver: Browser
): Promise<TaskFilePreviewInspection> {
  const marker = await driver.$(selectors.taskFilePreviewInspection);
  await marker.waitForExist({ timeout: SCREEN_TIMEOUT_MS });
  const value = await marker.getAttribute("value");
  if (!value) throw new Error("Task file preview inspection had no value");
  return JSON.parse(value) as TaskFilePreviewInspection;
}

async function expectNativeText(
  driver: Browser,
  selector: string,
  expected: string | RegExp
): Promise<void> {
  let lastText = "";
  await driver.waitUntil(
    async () => {
      const element = await driver.$(selector);
      if (!(await element.isExisting().catch(() => false))) return false;
      lastText = await element.getText().catch(() => "");
      return typeof expected === "string"
        ? lastText === expected
        : expected.test(lastText);
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: `Expected ${selector} to contain ${String(expected)}; last text ${JSON.stringify(lastText)}`
    }
  );
}

async function closeTaskFilePreview(driver: Browser): Promise<void> {
  const close = await driver.$(selectors.taskFilePreviewClose);
  await close.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await close.click();
  await driver.waitUntil(
    async () => {
      const path = await driver.$(selectors.taskFilePreviewPath);
      return !(await path.isExisting().catch(() => false));
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: "Expected task file preview to close"
    }
  );
}

// Xterm link hitboxes remain inside the WebView. Appium drives the native
// mentioned-files list, whose data comes from the same incremental xterm scan.
// Direct hitbox activation remains covered by the terminal document bridge
// tests without adding E2E-only controls to production UI.
export async function openMentionedFileMenuSelection(
  driver: Browser,
  ui: Pick<RelayUi, "inspectTerminalWebView" | "waitUntil">,
  fixture: RelayFilePreviewFixture
): Promise<void> {
  let lastInspection: Awaited<ReturnType<RelayUi["inspectTerminalWebView"]>> | null = null;

  await ui.waitUntil(
    async () => {
      lastInspection = await ui.inspectTerminalWebView();
      return lastInspection.kind === "rendered" && fixture.mentionedLinks.every(
        (path) => lastInspection?.kind === "rendered" && lastInspection.text.includes(path)
      );
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: `Expected mentioned file paths to remain visible inside xterm; last inspection ${JSON.stringify(lastInspection)}`
    }
  );

  const obsoleteStrip = await driver.$("~Files mentioned in terminal");
  if (await obsoleteStrip.isExisting().catch(() => false)) {
    throw new Error("The removed horizontal mentioned-file strip is still visible");
  }

  // The xterm detector batches bridge updates for 200 ms. Terminal text can
  // become inspectable just before the native history receives that batch.
  await driver.pause(350);
  lastInspection = await ui.inspectTerminalWebView();
  const taskMore = await driver.$(selectors.taskMoreButton);
  await taskMore.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await taskMore.click();
  const mentionedFilesAction = await driver.$(
    '-ios predicate string:label BEGINSWITH "Mentioned Files ("'
  );
  await mentionedFilesAction.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const actualMentionedFilesLabel = await mentionedFilesAction.getAttribute(
    "label"
  );
  const expectedMentionedFilesLabel =
    `Mentioned Files (${fixture.mentionedCount})`;
  if (actualMentionedFilesLabel !== expectedMentionedFilesLabel) {
    throw new Error(
      `Expected ${expectedMentionedFilesLabel}, got ${JSON.stringify(actualMentionedFilesLabel)}; ` +
      `terminal inspection ${JSON.stringify(lastInspection)}`
    );
  }
  await mentionedFilesAction.click();

  const modal = await driver.$(selectors.taskMentionedFilesModal);
  await modal.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const canonicalRowLocations: Array<{ path: string; y: number }> = [];
  for (const path of fixture.expectedCanonicalRowOrder) {
    const row = await driver.$(taskMentionedFilesRowSelector(path));
    await row.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    const y = await row.getLocation("y");
    canonicalRowLocations.push({ path, y });
  }
  for (let index = 1; index < canonicalRowLocations.length; index += 1) {
    const previous = canonicalRowLocations[index - 1]!;
    const current = canonicalRowLocations[index]!;
    if (current.y <= previous.y) {
      throw new Error(
        `Expected canonical mentioned-file row order ${JSON.stringify(fixture.expectedCanonicalRowOrder)}; ` +
        `measured ${JSON.stringify(canonicalRowLocations)}`
      );
    }
  }

  const uniqueRow = await driver.$(
    taskMentionedFilesRowSelector(fixture.uniqueCanonicalPath)
  );
  await uniqueRow.click();
}

async function verifyMentionedFileMenuFlow(
  driver: Browser,
  ui: Pick<RelayUi, "inspectTerminalWebView" | "waitUntil">,
  fixture: RelayFilePreviewFixture
): Promise<void> {
  await openMentionedFileMenuSelection(driver, ui, fixture);
  await expectNativeText(
    driver,
    selectors.taskFilePreviewPath,
    fixture.uniqueCanonicalPath
  );
  let inspection = await inspectTaskFilePreview(driver);
  if (
    inspection.path !== fixture.uniqueCanonicalPath ||
    inspection.mode !== "raw" ||
    inspection.initialLine !== fixture.line ||
    !inspection.content
      .split(/\r\n|\r|\n/)
      [fixture.line - 1]?.includes(fixture.expectedRawLine)
  ) {
    throw new Error(
      `Expected bare filename to resolve to raw line ${fixture.line}; got ${JSON.stringify(inspection)}`
    );
  }
  let rawWebView: TaskFilePreviewWebViewInspection = {
    kind: "unavailable",
    reason: "WebView inspection has not started"
  };
  rawWebView = await inspectTaskFilePreviewWebView(
    createWebViewContextDriver(driver)
  );
  if (rawWebView.kind !== "unavailable") {
    try {
      await driver.waitUntil(
        async () => {
          rawWebView = await inspectTaskFilePreviewWebView(
            createWebViewContextDriver(driver)
          );
          return (
            rawWebView.kind === "raw" &&
            rawWebView.path === fixture.uniqueCanonicalPath &&
            rawWebView.line === fixture.line &&
            rawWebView.flashStarted &&
            rawWebView.overlayWidth > 0 &&
            rawWebView.overlayHeight > 0 &&
            Number.isFinite(rawWebView.overlayTop)
          );
        },
        {
          interval: POLL_INTERVAL_MS,
          timeout: SCREEN_TIMEOUT_MS,
          timeoutMsg: `Expected raw preview WebView line ${fixture.line}`
        }
      );
    } catch {
      throw new Error(
        `Expected resolved raw preview line ${fixture.line} to be laid out and flashed; got ${JSON.stringify(rawWebView)}`
      );
    }
  }
  await closeTaskFilePreview(driver);
  // The native preview closes before React has completed the modal unmount.
  // Let that transition settle before opening another action sheet; otherwise
  // its stale close callback can clear the second mentioned-files request.
  await driver.pause(350);

  const taskMore = await driver.$(selectors.taskMoreButton);
  await taskMore.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await taskMore.click();
  const taskActionMenu = await driver.$(`~${TASK_ACTION_MENU_TITLE}`);
  await taskActionMenu.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const mentionedFilesAction = await driver.$(
    '-ios predicate string:label BEGINSWITH "Mentioned Files ("'
  );
  await mentionedFilesAction.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const mentionedFilesLabel = await mentionedFilesAction.getAttribute("label");
  const expectedMentionedFilesLabel = `Mentioned Files (${fixture.mentionedCount})`;
  if (mentionedFilesLabel !== expectedMentionedFilesLabel) {
    throw new Error(
      `Expected ${expectedMentionedFilesLabel} before reopening mentioned files, got ${JSON.stringify(mentionedFilesLabel)}`
    );
  }
  await mentionedFilesAction.click();
  const markdownRow = await driver.$(`~Open file ${fixture.path}`);
  await markdownRow.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  const markdownRowEnabled = await markdownRow.getAttribute("enabled");
  const markdownRowLabel = await markdownRow.getAttribute("label");
  if (markdownRowEnabled !== "true") {
    throw new Error(
      `Expected resolved mentioned file ${fixture.path} to be selectable; ` +
        `native enabled=${JSON.stringify(markdownRowEnabled)}, label=${JSON.stringify(markdownRowLabel)}`
    );
  }
  await markdownRow.click();
  await expectNativeText(driver, selectors.taskFilePreviewPath, fixture.path);
  await expectNativeText(driver, selectors.taskFilePreviewMode, "Rendered Markdown");
  inspection = await inspectTaskFilePreview(driver);
  if (
    inspection.path !== fixture.path ||
    inspection.mode !== "rendered" ||
    !inspection.content.includes(`# ${fixture.expectedHeading}`) ||
    !inspection.content.includes(fixture.expectedRenderedText)
  ) {
    throw new Error(
      `Expected authenticated relay Markdown content in rendered preview; got ${JSON.stringify(inspection)}`
    );
  }
  let renderedWebView: TaskFilePreviewWebViewInspection = {
    kind: "unavailable",
    reason: "WebView inspection has not started"
  };
  renderedWebView = await inspectTaskFilePreviewWebView(
    createWebViewContextDriver(driver)
  );
  if (renderedWebView.kind !== "unavailable") {
    try {
      await driver.waitUntil(
        async () => {
          renderedWebView = await inspectTaskFilePreviewWebView(
            createWebViewContextDriver(driver)
          );
          return (
            renderedWebView.kind === "rendered" &&
            renderedWebView.path === fixture.path &&
            renderedWebView.tokenClass === fixture.expectedHighlightedTokenClass &&
            renderedWebView.tokenText === fixture.expectedHighlightedToken &&
            Boolean(renderedWebView.tokenColor) &&
            renderedWebView.tokenColor !== renderedWebView.unhighlightedColor &&
            renderedWebView.tokenWidth > 0 &&
            renderedWebView.tokenHeight > 0
          );
        },
        {
          interval: POLL_INTERVAL_MS,
          timeout: SCREEN_TIMEOUT_MS,
          timeoutMsg: "Expected rendered preview WebView syntax highlighting"
        }
      );
    } catch {
      throw new Error(
        `Expected rendered preview WebView syntax highlighting with a non-default computed color; got ${JSON.stringify(renderedWebView)}`
      );
    }
  }
  await closeTaskFilePreview(driver);
}

async function isRelaySignedIn(ui: RelayUi): Promise<boolean> {
  return (await ui.getAccountSignOutButton()).isExisting().catch(() => false);
}

async function closeAccountSheet(driver: Browser, ui: RelayUi): Promise<void> {
  const closeButton = await ui.getAccountCloseButton();
  if (await closeButton.isExisting()) {
    await closeButton.click();
    await ui.pause(500);
    return;
  }
  const nativeCloseButton = await driver.$("~Close account");
  if (await nativeCloseButton.isExisting().catch(() => false)) {
    await nativeCloseButton.click();
    await ui.pause(500);
  }
}

export async function assertRelayTaskRowPresentation(
  row: Pick<RelayElement, "getAttribute" | "getText">,
  expected: RelayTaskRowExpectation,
): Promise<void> {
  const nativeLabel = await row.getAttribute("label").catch(() => null);
  const label = nativeLabel?.trim() || (await row.getText()).trim();
  const expectedLabel = [
    expected.title,
    `Task ID ${expected.taskId}`,
    expected.stage,
    expected.waitingPromptSnippet === expected.title
      ? null
      : expected.waitingPromptSnippet,
  ].filter(Boolean).join(". ");
  const forbidden = [
    expected.originalPromptSnippet,
    expected.repoLabel,
    "TASK",
    "RECENT",
  ];
  // iOS may append a visual truncation ellipsis to the accessibility label.
  // The semantic row fields still have to match exactly.
  const expectedLabels = [expectedLabel, `${expectedLabel}. …`];
  if (!expectedLabels.includes(label) || forbidden.some((value) => label.includes(value))) {
    throw new Error(
      `Relay task row rendered unexpected content: ${JSON.stringify(label)}; ` +
        `expected ${JSON.stringify(expectedLabel)}`,
    );
  }
}

export async function assertRecentTaskRowShowsRepoLabel(
  row: Pick<RelayElement, "getAttribute" | "getText">,
  expected: RelayTaskRowExpectation,
): Promise<void> {
  const nativeLabel = await row.getAttribute("label").catch(() => null);
  const label = nativeLabel?.trim() || (await row.getText()).trim();
  const expectedLabel = [
    expected.title,
    `Task ID ${expected.taskId}`,
    expected.repoLabel,
    expected.stage,
    expected.waitingPromptSnippet === expected.title
      ? null
      : expected.waitingPromptSnippet,
  ].filter(Boolean).join(". ");
  if (label !== expectedLabel) {
    throw new Error(
      `Recent task row rendered unexpected content: ${JSON.stringify(label)}; ` +
        `expected ${JSON.stringify(expectedLabel)}`,
    );
  }
}

export async function verifyRecentTabShowsRepoLabel(
  ui: Pick<RelayUi, "getRecentTab" | "getTasksTab" | "getTaskRowById">,
  taskId: string,
  expected: RelayTaskRowExpectation,
): Promise<void> {
  const recentTab = await ui.getRecentTab();
  await recentTab.click();
  const row = await ui.getTaskRowById(taskId);
  await row.scrollIntoView({ direction: "down", maxScrolls: 5 });
  await row.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await assertRecentTaskRowShowsRepoLabel(row, expected);
  const tasksTab = await ui.getTasksTab();
  await tasksTab.click();
}

export async function openRelayFixtureTask(
  ui: Pick<RelayUi, "getTaskRowById">,
  taskId: string,
  expected?: RelayTaskRowExpectation,
): Promise<void> {
  if (expected) {
    const task = await ui.getTaskRowById(taskId);
    await task.scrollIntoView({ direction: "down", maxScrolls: 5 });
    await task.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    await assertRelayTaskRowPresentation(task, expected);
    await task.click();
    return;
  }
  const task = await ui.getTaskRowById(taskId);
  await task.scrollIntoView({ direction: "down", maxScrolls: 5 });
  await task.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await task.click();
}

async function renderedTaskRowIds(
  ui: Pick<RelayUi, "getTaskRows">,
): Promise<string[]> {
  const taskIds: string[] = [];
  for (const row of await ui.getTaskRows()) {
    const taskId = extractTaskRowId(
      await row.getAttribute("name").catch(() => null),
    );
    if (taskId) taskIds.push(taskId);
  }
  return taskIds;
}

export async function verifyTasksTabNewestFirst(
  ui: Pick<RelayUi, "getTasksTab" | "getTaskRows" | "waitUntil">,
  fixture: RelayTaskOrderingFixture,
  selectTasksTab = true,
): Promise<void> {
  if (selectTasksTab) {
    const tasksTab = await ui.getTasksTab();
    await tasksTab.click();
  }

  const fixtureIds = new Set(fixture.expectedVisualOrderTaskIds);
  let lastRenderedTaskIds: string[] = [];
  await ui.waitUntil(
    async () => {
      lastRenderedTaskIds = await renderedTaskRowIds(ui);
      return fixture.expectedVisualOrderTaskIds.every((taskId) =>
        lastRenderedTaskIds.includes(taskId)
      );
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg:
        "Expected both deterministic creation-order tasks on the Tasks tab",
    },
  );

  const actualVisualOrder = lastRenderedTaskIds.filter((taskId) =>
    fixtureIds.has(taskId)
  );
  if (
    actualVisualOrder.length !== fixture.expectedVisualOrderTaskIds.length ||
    actualVisualOrder.some(
      (taskId, index) => taskId !== fixture.expectedVisualOrderTaskIds[index],
    )
  ) {
    throw new Error(
      `Expected Tasks-tab creation order ${JSON.stringify(fixture.expectedVisualOrderTaskIds)}; ` +
        `source order was ${JSON.stringify(fixture.sourceOrderTaskIds)}; ` +
        `native visual order was ${JSON.stringify(actualVisualOrder)}`,
    );
  }
}

async function verifyTabletWorkspaceSelection(
  driver: Browser,
  ui: Pick<RelayUi, "getTaskRowById" | "waitUntil">,
  fixtureTaskId: string,
  ordering: RelayTaskOrderingFixture,
): Promise<boolean> {
  const shell = await driver.$(selectors.tabletWorkspaceShell);
  if (!(await shell.isExisting().catch(() => false))) return false;

  // React Native exposes this flex-only container in the native tree without
  // assigning it an independently displayed accessibility frame on iPad.
  // Its substantive children carry frames, so assert those instead.
  await (await driver.$(selectors.tabletWorkspaceSidebar)).waitForDisplayed({
    timeout: SCREEN_TIMEOUT_MS,
  });
  await (await driver.$(selectors.tabletWorkspaceEmpty)).waitForDisplayed({
    timeout: SCREEN_TIMEOUT_MS,
  });

  const alternateTaskId = ordering.expectedVisualOrderTaskIds.find(
    (taskId) => taskId !== fixtureTaskId,
  );
  if (!alternateTaskId) {
    throw new Error("Expected a second relay task for tablet workspace switching");
  }

  for (const taskId of [alternateTaskId, fixtureTaskId]) {
    const row = await ui.getTaskRowById(taskId);
    await row.scrollIntoView({ direction: "down", maxScrolls: 5 });
    await row.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    await row.click();
    const detailTaskId = await driver.$(selectors.taskDetailTaskId);
    await detailTaskId.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
    await ui.waitUntil(
      async () => (await detailTaskId.getText()).includes(
        relayTaskDetailDisplayId(taskId),
      ),
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg: `Expected tablet workspace to select task ${taskId}`,
      },
    );
  }

  return true;
}

export function relayTaskDetailDisplayId(taskId: string): string {
  if (!taskId.startsWith("cloud:")) return taskId;
  const ownerLocalTaskSeparator = taskId.lastIndexOf(":");
  return ownerLocalTaskSeparator >= 0
    ? taskId.slice(ownerLocalTaskSeparator + 1)
    : taskId;
}

async function returnToTaskListShell(ui: RelayUi): Promise<void> {
  const backButton = await ui.getBackButton();
  if (await backButton.isExisting().catch(() => false)) {
    await backButton.click();
    await ui.pause(500);
  }
}

async function waitForTaskActivity(
  ui: Pick<RelayUi, "getTaskRowById" | "getTaskRows" | "waitUntil">,
  taskId: string,
  expectedActivity: TaskActivity,
): Promise<void> {
  let lastObserved: string | null = null;
  try {
    await ui.waitUntil(
      async () => {
        const task = await ui.getTaskRowById(taskId);
        lastObserved = await task.getAttribute("value").catch(() => null);
        return lastObserved === expectedActivity;
      },
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg: `Expected relay task ${taskId} activity ${expectedActivity}`,
      },
    );
  } catch {
    const renderedTaskIds: string[] = [];
    for (const row of await ui.getTaskRows().catch(() => [])) {
      const name = await row.getAttribute("name").catch(() => null) ??
        await row.getAttribute("label").catch(() => null);
      const renderedTaskId = extractTaskRowId(name);
      if (renderedTaskId) renderedTaskIds.push(renderedTaskId);
    }
    throw new Error(
      `Expected relay task ${taskId} activity ${expectedActivity}; ` +
        `last native accessibility value was ${String(lastObserved)}; ` +
        `rendered task row ids were ${JSON.stringify(renderedTaskIds.sort())}`,
    );
  }
}

async function waitForTaskRowValue(
  ui: Pick<RelayUi, "getTaskRowById" | "waitUntil">,
  taskId: string,
  expectedValues: readonly string[],
): Promise<void> {
  let lastObserved: string | null = null;
  await ui.waitUntil(async () => {
    const task = await ui.getTaskRowById(taskId);
    lastObserved = await task.getAttribute("value").catch(() => null);
    return lastObserved !== null && expectedValues.includes(lastObserved);
  }, {
    interval: POLL_INTERVAL_MS,
    timeout: SCREEN_TIMEOUT_MS,
    timeoutMsg:
      `Expected relay task ${taskId} rendered value ${expectedValues.join(" or ")}; ` +
      `last value was ${String(lastObserved)}`,
  });
}

async function waitForBusyUnreadTaskRow(
  ui: Pick<RelayUi, "getTaskRowById" | "waitUntil">,
  taskId: string,
): Promise<void> {
  let lastObserved: string | null = null;
  await ui.waitUntil(async () => {
    const task = await ui.getTaskRowById(taskId);
    lastObserved = await task.getAttribute("value").catch(() => null);
    return lastObserved === "working, unread";
  }, {
    interval: POLL_INTERVAL_MS,
    timeout: SCREEN_TIMEOUT_MS,
    timeoutMsg:
      `Expected relay task ${taskId} to render running and unread on the list ` +
      `without visiting detail; last value was ${String(lastObserved)}`
  });
}

async function waitForBusyReadTaskRow(
  ui: Pick<RelayUi, "getTaskRowById" | "waitUntil">,
  taskId: string,
): Promise<void> {
  let lastObserved: string | null = null;
  await ui.waitUntil(async () => {
    const task = await ui.getTaskRowById(taskId);
    lastObserved = await task.getAttribute("value").catch(() => null);
    return lastObserved === "working";
  }, {
    interval: POLL_INTERVAL_MS,
    timeout: SCREEN_TIMEOUT_MS,
    timeoutMsg:
      `Expected relay task ${taskId} to render running and read on the list ` +
      `without visiting detail; last value was ${String(lastObserved)}`
  });
}

async function waitForSelectedTaskDetailActivity(
  ui: Pick<RelayUi, "getTaskDetailActivity" | "waitUntil">,
  expectedActivity: TaskActivity,
): Promise<void> {
  let lastObserved: string | null = null;
  try {
    await ui.waitUntil(
      async () => {
        const activity = await ui.getTaskDetailActivity();
        lastObserved = await activity.getAttribute("value").catch(() => null);
        return lastObserved === expectedActivity;
      },
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg: `Expected selected relay task activity ${expectedActivity}`,
      },
    );
  } catch {
    throw new Error(
      `Expected selected relay task activity ${expectedActivity}; ` +
        `last native accessibility value was ${String(lastObserved)}`,
    );
  }
}

export async function verifyRelayTaskActivityTransitions(
  ui: Pick<RelayUi, "getTaskRowById" | "getTaskRows" | "waitUntil">,
  taskId: string,
  setTaskActivity: (activity: "unread" | "idle") => Promise<void>,
): Promise<void> {
  await waitForTaskActivity(ui, taskId, "working");
  await setTaskActivity("unread");
  // The activity remains unread even if the independent runtime state returns
  // to busy before the list samples it.
  await waitForTaskRowValue(ui, taskId, ["unread", "working, unread"]);
  await setTaskActivity("idle");
  await waitForTaskActivity(ui, taskId, "idle");
}

export async function verifyRelayTaskMarkedRead(
  ui: Pick<RelayUi, "getTaskRowById" | "getTaskRows" | "waitUntil">,
  taskId: string,
  actions: {
    closeTask(): Promise<void>;
    openTask(): Promise<void>;
    prepareUnread(): Promise<void>;
    settleAfterClose?(): Promise<void>;
    waitForOwnerIdle(): Promise<void>;
    waitForSelectedDetailIdle(): Promise<void>;
  },
): Promise<void> {
  await actions.prepareUnread();
  await waitForTaskRowValue(ui, taskId, ["unread", "working, unread"]);
  await actions.openTask();
  await actions.waitForOwnerIdle();
  await actions.waitForSelectedDetailIdle();
  await actions.closeTask();
  await actions.settleAfterClose?.();
  await waitForTaskActivity(ui, taskId, "idle");
}

async function signInToRelay(
  driver: Browser,
  ui: RelayUi,
  credentials: RelayCredentials
): Promise<void> {
  await openRelayProfileSheet(ui);

  const signOutButton = await ui.getAccountSignOutButton();
  if (await signOutButton.isExisting().catch(() => false)) {
    await signOutButton.click();
    // Auth persistence updates asynchronously. Do not let the old Sign Out
    // control satisfy the subsequent sign-in readiness check.
    await ui.waitUntil(
      async () =>
        !(await ui.getAccountSignOutButton())
          .isExisting()
          .catch(() => false),
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg: "Expected the prior relay account to finish signing out"
      }
    );
  }

  const emailInput = await ui.getAccountEmailInput();
  await emailInput.setValue(credentials.email);
  const passwordInput = await ui.getAccountPasswordInput();
  await passwordInput.setValue(credentials.password);
  const signInButton = await ui.getAccountSignInButton();
  await signInButton.click();
  await dismissSavePasswordPrompt(driver);

  await ui.waitUntil(
    async () => {
      await dismissSavePasswordPrompt(driver);
      return await isRelaySignedIn(ui);
    },
    {
      interval: POLL_INTERVAL_MS,
      timeout: SCREEN_TIMEOUT_MS,
      timeoutMsg: "Expected mobile app to connect through the relay-backed cloud path"
    }
  );
  await closeAccountSheet(driver, ui);
}

async function openRelayProfileSheet(
  ui: Pick<RelayUi, "getAccountButton" | "getAccountCloseButton">,
): Promise<void> {
  const accountButton = await ui.getAccountButton();
  await accountButton.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await accountButton.click();
  const closeButton = await ui.getAccountCloseButton();
  await closeButton.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
}

export async function runRelayTaskFlow(
  driver: Browser,
  options: RelayTaskFlowOptions
): Promise<void> {
  const ui = createRelayUi(driver);
  await dismissSavePasswordPrompt(driver);
  const appShell = await driver.$(selectors.appShell);
  await appShell.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  await returnToTaskListShell(ui);

  if (!(await isTaskVisible(ui, options.fixture.taskId))) {
    await signInToRelay(driver, ui, options.credentials);
  }
  await ensureTaskListVisible(ui);
  const isTabletWorkspace = await verifyTabletWorkspaceSelection(
    driver,
    ui,
    options.fixture.taskId,
    options.taskOrdering,
  );
  const closeTaskForJourney = async () => {
    if (!isTabletWorkspace) {
      await returnToTaskListShell(ui);
      return;
    }

    const alternateTaskId = options.taskOrdering.expectedVisualOrderTaskIds.find(
      (taskId) => taskId !== options.fixture.taskId,
    );
    if (!alternateTaskId) {
      throw new Error("Expected a second relay task for tablet task switching");
    }
    await openRelayFixtureTask(ui, alternateTaskId);
    const detailTaskId = await driver.$(selectors.taskDetailTaskId);
    await ui.waitUntil(
      async () => (await detailTaskId.getText()).includes(
        relayTaskDetailDisplayId(alternateTaskId),
      ),
      {
        interval: POLL_INTERVAL_MS,
        timeout: SCREEN_TIMEOUT_MS,
        timeoutMsg: `Expected tablet workspace to switch away to ${alternateTaskId}`,
      },
    );
  };
  let renderedTerminalVisits = 0;
  await verifyTasksTabNewestFirst(ui, options.taskOrdering, !isTabletWorkspace);
  const exactTaskRow = await ui.getTaskRowById(options.fixture.taskId);
  await exactTaskRow.scrollIntoView({ direction: "down", maxScrolls: 5 });
  try {
    await exactTaskRow.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
  } catch {
    const renderedTaskIds = await renderedTaskRowIds(ui);
    throw new Error(
      `Expected relay task row ${options.fixture.taskId}; rendered task row ids were ` +
        `${JSON.stringify(renderedTaskIds)}`,
    );
  }
  await assertRelayTaskRowPresentation(exactTaskRow, options.taskRow);
  await options.setTaskBusyRead();
  await waitForBusyReadTaskRow(ui, options.fixture.taskId);
  const busyReadScreenshotPath =
    process.env.KANNA_E2E_TASK_BUSY_READ_SCREENSHOT_PATH?.trim();
  if (busyReadScreenshotPath) {
    await driver.saveScreenshot(busyReadScreenshotPath);
  }
  await options.setTaskBusyUnread();
  await waitForBusyUnreadTaskRow(ui, options.fixture.taskId);
  const busyUnreadScreenshotPath =
    process.env.KANNA_E2E_TASK_BUSY_UNREAD_SCREENSHOT_PATH?.trim();
  if (busyUnreadScreenshotPath) {
    await driver.saveScreenshot(busyUnreadScreenshotPath);
  }
  await options.setTaskActivity("unread");
  await waitForTaskActivity(ui, options.fixture.taskId, "unread");
  if (!isTabletWorkspace) {
    await verifyRecentTabShowsRepoLabel(ui, options.fixture.taskId, options.taskRow);
  }
  await options.setTaskActivity("working");
  await waitForTaskActivity(ui, options.fixture.taskId, "working");
  await verifyRelayTaskActivityTransitions(
    ui,
    options.fixture.taskId,
    options.setTaskActivity,
  );
  await runRelayTaskJourneys({
    verifyQuickReplyPersistence: () =>
      verifyRelayQuickReplyPersistenceJourney(
        createRelayQuickReplyPersistenceJourney(
          driver,
          ui,
          options.bundleId,
          options.waitForAppReady,
        ),
        options.customizedReply,
      ),
    verifyMarkedRead: () => verifyRelayTaskMarkedRead(ui, options.fixture.taskId, {
      prepareUnread: options.prepareTaskUnreadForMarkRead,
      async openTask() {
        await openRelayFixtureTask(ui, options.fixture.taskId);
        const detailTaskId = await driver.$(selectors.taskDetailTaskId);
        await detailTaskId.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
        if (!isTabletWorkspace) {
          const backButton = await ui.getBackButton();
          await backButton.waitForDisplayed({ timeout: SCREEN_TIMEOUT_MS });
        }
      },
      async waitForOwnerIdle() {
        // Opening the synthetic PTY snapshot is itself output activity. Settle
        // the fixture after that attach so this read-state journey does not
        // confuse snapshot hydration with an agent turn.
        await options.setTaskActivity("idle");
        await options.waitForLocalTaskActivity("idle");
      },
      async waitForSelectedDetailIdle() {
        // Geometry is asserted by the dedicated rendering journey below.
      },
      closeTask: closeTaskForJourney,
      settleAfterClose: () => options.setTaskActivity("idle"),
    }),
    verifyPtySnapshotRevisit: () => verifyRelayPtySnapshotRevisit({
      openTask: () => openRelayFixtureTask(ui, options.fixture.taskId),
      async waitForRenderedTerminal() {
        await waitForTaskTerminalLive(ui);
        await waitForRenderedPtyTerminal(ui, options.fixture);
        await verifyRelayPtyRenderedGridAndCursor(ui, options.fixture);
        const terminalScreenshotPath =
          process.env.KANNA_E2E_TERMINAL_SCREENSHOT_PATH?.trim();
        if (terminalScreenshotPath && renderedTerminalVisits === 0) {
          await driver.saveScreenshot(terminalScreenshotPath);
        }
        if (renderedTerminalVisits === 0) {
          await options.restoreTallTerminalGeometry();
        } else {
          await verifyRelayPtyAuthoritativeScrollback(driver, ui, options.fixture);
        }
        process.stdout.write(
          `[mobile-e2e] authoritative terminal render visit ${renderedTerminalVisits + 1} passed\n`,
        );
        renderedTerminalVisits += 1;
        if (renderedTerminalVisits === 2) {
          await verifyRelayPtyStableResync(
            ui,
            options.fixture,
            options.resyncTerminalConnection,
          );
          process.stdout.write("[mobile-e2e] terminal resync stability passed\n");
          await verifyRelayPtyTunnelDropIsInvisible(
            ui,
            options.fixture,
            options.dropRelayTunnels,
          );
          process.stdout.write(
            "[mobile-e2e] terminal tunnel-drop invisibility passed\n",
          );
        }
      },
      closeTask: closeTaskForJourney,
    }),
    verifySendOutcomes: async () => {
      await openRelayFixtureTask(ui, options.fixture.taskId);
      await waitForTaskTerminalLive(ui);
      await verifyRelaySendOutcomesJourney(ui, {
        captureScreenshot: options.captureScreenshot,
      });
      await closeTaskForJourney();
    },
    verifyTerminalKeys: () =>
      verifyRelayTerminalKeys(driver, options.terminalKeys),
    // Opens the task itself and waits for a rendered authoritative terminal,
    // so the daemon is known live before any geometry is observed, then
    // returns to the list the way the other detail journeys do.
    verifyMobileTerminalControl: async () => {
      await openRelayFixtureTask(ui, options.fixture.taskId);
      await waitForTaskTerminalLive(ui);
      await waitForRenderedPtyTerminal(ui, options.fixture);
      await verifyRelayMobileTerminalControlJourney(driver, ui, options.fixture, {
        captureScreenshot: options.captureScreenshot,
        observeAuthoritativeTerminalGeometry:
          options.observeAuthoritativeTerminalGeometry,
        restoreDesktopTerminalControl: options.restoreDesktopTerminalControl,
      });
      await closeTaskForJourney();
    },
    verifyTaskActionMenu: () => verifyRelayTaskActionMenuJourney(
      ui,
      isTabletWorkspace
        ? {
            async dismissWithoutCancel() {
              // iPad presents this as a popover with no Cancel row. Appium's
              // dismissAlert chooses the destructive action in that shape;
              // tapping outside the popover is the native cancellation path.
              const sidebar = await driver.$(selectors.tabletWorkspaceSidebar);
              await sidebar.click();
            },
          }
        : {},
    ),
    verifyVisualCompanion: () =>
      verifyRelayVisualCompanionJourney(
        createVisualCompanionUi(driver),
        options.companion.fixture,
        options.companion
      ),
    async verifyFilePreview() {
      await options.emitFilePreviewLinks();
      await verifyMentionedFileMenuFlow(driver, ui, options.filePreview);
    },
    // Owns its open/close like the other detail journeys: it now runs after
    // verifySendOutcomes, which returns to the list.
    verifyComposerReset: async () => {
      await openRelayFixtureTask(ui, options.fixture.taskId);
      await waitForTaskTerminalLive(ui);
      await verifyRelayComposerResetJourney(ui, {
        captureScreenshot: options.captureScreenshot,
      });
      await closeTaskForJourney();
    },
    // On iPad this input follows the sidebar's alternate-task -> fixture-task
    // switch above, proving the existing single selected-task subscription is
    // the one that receives the composer message.
    verifyQuickReply: () =>
      verifyRelayCustomizedQuickReplyJourney(
        ui,
        options.draft,
        options.waitForQuickReplyInput,
      ),
  });
}
