import { computed, getCurrentInstance, onBeforeUnmount, ref, shallowRef } from "vue";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type DownloadEvent } from "@tauri-apps/plugin-updater";
import { invoke } from "../invoke";
import { isTauri } from "../tauri-mock";

const STARTUP_DELAY_MS = 15_000;
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

type UpdateStatus =
  | "idle"
  | "checking"
  | "available"
  | "downloading"
  | "readyToRestart"
  | "error"
  /**
   * Linux: a newer version exists in the package manager's index. Deliberately
   * a status of its own rather than `available`, because `available` is a
   * promise the app can keep by clicking Install and this one is not — the
   * upgrade belongs to apt.
   */
  | "packageManagerUpdate"
  /** Linux: the package index cannot answer, so neither can the app. Saying
   *  "up to date" here would be a guess dressed as a fact. */
  | "packageManagerUnknown";

/** What `linux_package_status` reports. Read-only by construction: nothing in
 *  this path installs, downloads or asks for root. */
export interface LinuxPackageStatus {
  /** Does a package manager own this installation's updates? Comes from the
   *  binary, not from the webview's platform string, which describes the
   *  renderer rather than how this installation was delivered. */
  packageManaged: boolean;
  packageName: string;
  installedVersion: string | null;
  candidateVersion: string | null;
  updateAvailable: boolean;
  metadataUnavailable: boolean;
  detail: string | null;
}

interface UpdateHandle {
  currentVersion: string;
  version: string;
  date?: string;
  body?: string;
  downloadAndInstall(onEvent?: (progress: DownloadEvent) => void): Promise<void>;
  close(): Promise<void>;
}

interface E2eUpdateInjectionOptions {
  version: string;
  currentVersion?: string;
  date?: string;
  body?: string;
  contentLength?: number;
  chunks?: number[];
  delayMs?: number;
  failInstall?: boolean;
  failInstallAttempts?: number;
  failMessage?: string;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Read the package-manager view of this installation. Injected in tests so
 *  both platforms' behaviour is exercisable without a host. */
export type ReadPackageStatus = () => Promise<LinuxPackageStatus>;

const readPackageStatusFromHost: ReadPackageStatus = () =>
  invoke<LinuxPackageStatus>("linux_package_status");

export function useAppUpdate(readPackageStatus: ReadPackageStatus = readPackageStatusFromHost) {
  const status = ref<UpdateStatus>("idle");
  const windowFocused = ref(!isTauri);
  const updateRef = shallowRef<UpdateHandle | null>(null);
  const updateVersion = ref<string | null>(null);
  const releaseNotes = ref<string | null>(null);
  const publishedAt = ref<string | null>(null);
  const dismissedVersion = ref<string | null>(null);
  const downloadedBytes = ref(0);
  const contentLength = ref<number | null>(null);
  const errorMessage = ref<string | null>(null);
  const packageStatus = ref<LinuxPackageStatus | null>(null);
  const visible = computed(
    () =>
      windowFocused.value &&
      (status.value === "available" ||
        status.value === "downloading" ||
        status.value === "readyToRestart" ||
        status.value === "error" ||
        status.value === "packageManagerUpdate" ||
        status.value === "packageManagerUnknown"),
  );

  let started = false;
  let disposed = false;
  let checkInFlight: Promise<void> | null = null;
  let startupTimer: ReturnType<typeof setTimeout> | null = null;
  let intervalTimer: ReturnType<typeof setInterval> | null = null;
  let enabledPromise: Promise<boolean> | null = null;
  let updaterEnabled: boolean | null = null;
  let packageManagedPromise: Promise<boolean> | null = null;
  let packageManaged: boolean | null = null;
  let unlistenWindowFocus: (() => void) | null = null;

  async function startWindowFocusTracking(): Promise<void> {
    if (!isTauri) {
      windowFocused.value = true;
      return;
    }

    let listenerRegistered = false;
    try {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      const currentWindow = getCurrentWindow();
      let focusEventSeen = false;
      const unlisten = await currentWindow.onFocusChanged((event) => {
        focusEventSeen = true;
        windowFocused.value = event.payload;
      });
      listenerRegistered = true;
      if (disposed) {
        unlisten();
        return;
      }
      unlistenWindowFocus = unlisten;

      const focused = await currentWindow.isFocused();
      if (!disposed && !focusEventSeen) {
        windowFocused.value = focused;
      }
    } catch (error) {
      if (!listenerRegistered) {
        windowFocused.value = true;
      }
      console.error("[updater] failed to track current window focus", error);
    }
  }

  async function ensureEnabled(): Promise<boolean> {
    if (updaterEnabled !== null) return updaterEnabled;
    if (enabledPromise) return enabledPromise;

    enabledPromise = (async () => {
      if (!isTauri) return false;
      if (import.meta.env.MODE === "development") return false;

      const worktree = await invoke<string>("read_env_var", { name: "KANNA_WORKTREE" }).catch((error) => {
        console.debug("[app-update] KANNA_WORKTREE not set; assuming production update checks are allowed:", error);
        return "";
      });
      return worktree !== "1";
    })().then((enabled) => {
      updaterEnabled = enabled;
      return enabled;
    });

    return enabledPromise;
  }

  /**
   * Which update path this installation is on, asked once.
   *
   * A failure here resolves to the self-updater path, which is the safe
   * default: on macOS it is correct, and on Linux the updater plugin is not
   * registered at all, so the worst case is a check that finds nothing rather
   * than an install over a dpkg-managed tree.
   */
  async function ensurePackageManaged(): Promise<boolean> {
    if (packageManaged !== null) return packageManaged;
    packageManagedPromise ??= readPackageStatus()
      .then((result) => {
        packageStatus.value = result;
        return result.packageManaged;
      })
      .catch((error) => {
        console.error("[app-update] could not read package status", error);
        return false;
      })
      .then((value) => {
        packageManaged = value;
        return value;
      });
    return packageManagedPromise;
  }

  /**
   * The Linux check: ask the package manager what it already knows and report
   * it. No `apt update`, no download, no install action — the app is a reader
   * here, and the honest failure ("the index cannot answer") is a state of its
   * own rather than something rounded down to "up to date".
   */
  async function runPackageManagerCheck(): Promise<void> {
    let result: LinuxPackageStatus;
    try {
      result = await readPackageStatus();
    } catch (error) {
      console.error("[app-update] package status check failed", error);
      packageStatus.value = null;
      status.value = "idle";
      return;
    }
    applyPackageStatus(result);
  }

  /** The status-to-UI mapping, kept separate so the dev-only injector below
   *  drives the same code a real check does rather than a copy of it. */
  function applyPackageStatus(result: LinuxPackageStatus): void {
    packageStatus.value = result;
    updateVersion.value = result.candidateVersion;
    if (result.updateAvailable && dismissedVersion.value === result.candidateVersion) {
      status.value = "idle";
      return;
    }
    if (result.updateAvailable) {
      status.value = "packageManagerUpdate";
      return;
    }
    // An unreadable index is only worth interrupting a person for when it is
    // also plausible that they are behind; on an installed, current machine it
    // is noise. Reported as a state, shown only when nothing else is known.
    status.value = result.metadataUnavailable && result.installedVersion !== null
      ? "packageManagerUnknown"
      : "idle";
  }

  async function closeUpdateHandle(update: UpdateHandle | null): Promise<void> {
    if (!update) return;
    try {
      await update.close();
    } catch (error) {
      console.error("[updater] failed to close update handle", error);
    }
  }

  function resetAvailableState() {
    updateRef.value = null;
    updateVersion.value = null;
    releaseNotes.value = null;
    publishedAt.value = null;
    downloadedBytes.value = 0;
    contentLength.value = null;
    errorMessage.value = null;
  }

  async function runCheck(): Promise<void> {
    if (checkInFlight) return checkInFlight;

    checkInFlight = (async () => {
      if (!(await ensureEnabled())) return;
      if (status.value !== "downloading" && status.value !== "readyToRestart") {
        status.value = "checking";
      }

      if (await ensurePackageManaged()) {
        await runPackageManagerCheck();
        return;
      }

      let update: UpdateHandle | null;
      try {
        update = (await check()) as UpdateHandle | null;
      } catch (error) {
        if (status.value === "checking") {
          status.value = "idle";
        }
        console.error("[updater] check failed", error);
        return;
      }

      if (!update) {
        if (status.value === "checking") {
          status.value = "idle";
        }
        return;
      }

      if (dismissedVersion.value === update.version) {
        await closeUpdateHandle(update);
        if (status.value === "checking") {
          status.value = "idle";
        }
        return;
      }

      const previousUpdate = updateRef.value;
      if (previousUpdate) {
        await closeUpdateHandle(previousUpdate);
      }
      updateRef.value = update;
      updateVersion.value = update.version;
      releaseNotes.value = update.body ?? "";
      publishedAt.value = update.date ?? null;
      downloadedBytes.value = 0;
      contentLength.value = null;
      errorMessage.value = null;
      status.value = "available";
    })().finally(() => {
      checkInFlight = null;
    });

    return checkInFlight;
  }

  function start() {
    if (started) return;
    started = true;
    void startWindowFocusTracking();
    void (async () => {
      if (disposed) return;
      if (!(await ensureEnabled())) return;
      if (disposed) return;

      startupTimer = setTimeout(() => {
        void runCheck();
        intervalTimer = setInterval(() => {
          void runCheck();
        }, CHECK_INTERVAL_MS);
      }, STARTUP_DELAY_MS);
    })();
  }

  function dismiss() {
    void closeUpdateHandle(updateRef.value);
    if (updateVersion.value) {
      dismissedVersion.value = updateVersion.value;
    }
    resetAvailableState();
    status.value = "idle";
  }

  async function install() {
    // Structurally unreachable on Linux — the updater plugin is not even
    // registered there — but stated here too, because a UI change that
    // reintroduced the button must not find a working install path behind it.
    if (packageManaged === true) return;
    if (!updateRef.value) return;

    status.value = "downloading";
    downloadedBytes.value = 0;
    contentLength.value = null;
    errorMessage.value = null;

    try {
      await updateRef.value.downloadAndInstall((event: DownloadEvent) => {
        switch (event.event) {
          case "Started":
            contentLength.value = event.data.contentLength ?? null;
            break;
          case "Progress":
            downloadedBytes.value += event.data.chunkLength ?? 0;
            break;
          case "Finished":
            break;
        }
      });
      status.value = "readyToRestart";
    } catch (error) {
      errorMessage.value = error instanceof Error ? error.message : String(error);
      status.value = "error";
    }
  }

  async function restartNow() {
    if (status.value !== "readyToRestart") return;
    await relaunch();
  }

  /**
   * Render a package-manager state in the real app, for visual verification.
   *
   * The states are otherwise unreachable in a dev run: `ensureEnabled()`
   * returns false for `MODE === "development"` (and again for a worktree
   * instance) before any package check happens, so a Linux dev build shows
   * nothing no matter what dpkg and apt say. Same guard and same purpose as
   * `__e2eInjectUpdate` above, and it feeds the real `applyPackageStatus`, so
   * what gets rendered is the mapping the product uses.
   */
  function __e2eInjectPackageStatus(result: LinuxPackageStatus) {
    if (!import.meta.env.DEV || !window.__KANNA_E2E__) {
      throw new Error("E2E package-status injection is only available in dev E2E runs.");
    }
    packageManaged = true;
    windowFocused.value = true;
    applyPackageStatus(result);
  }

  function __e2eInjectUpdate(options: E2eUpdateInjectionOptions) {
    if (!import.meta.env.DEV || !window.__KANNA_E2E__) {
      throw new Error("E2E updater injection is only available in dev E2E runs.");
    }

    const chunks = options.chunks ?? (options.contentLength ? [options.contentLength] : []);
    const totalContentLength =
      options.contentLength ??
      chunks.reduce((total, chunkLength) => total + chunkLength, 0);
    const waitMs = Math.max(0, options.delayMs ?? 0);
    let remainingFailures =
      options.failInstallAttempts ??
      (options.failInstall ? Number.POSITIVE_INFINITY : 0);

    updateRef.value = {
      currentVersion: options.currentVersion ?? "0.0.0-e2e",
      version: options.version,
      date: options.date ?? "2026-04-29T00:00:00Z",
      body: options.body ?? "",
      async downloadAndInstall(onEvent?: (progress: DownloadEvent) => void) {
        if (remainingFailures > 0) {
          remainingFailures -= 1;
          throw new Error(options.failMessage ?? "E2E update install failed");
        }

        onEvent?.({ event: "Started", data: { contentLength: totalContentLength } });
        for (const chunkLength of chunks) {
          if (waitMs > 0) await delay(waitMs);
          onEvent?.({ event: "Progress", data: { chunkLength } });
        }
        onEvent?.({ event: "Finished" });
      },
      async close() {},
    };
    updateVersion.value = options.version;
    releaseNotes.value = options.body ?? "";
    publishedAt.value = options.date ?? "2026-04-29T00:00:00Z";
    downloadedBytes.value = 0;
    contentLength.value = null;
    errorMessage.value = null;
    status.value = "available";
  }

  function dispose() {
    disposed = true;
    started = false;
    void closeUpdateHandle(updateRef.value);
    if (startupTimer) clearTimeout(startupTimer);
    if (intervalTimer) clearInterval(intervalTimer);
    unlistenWindowFocus?.();
    unlistenWindowFocus = null;
    startupTimer = null;
    intervalTimer = null;
  }

  if (getCurrentInstance()) {
    onBeforeUnmount(dispose);
  }

  return {
    status,
    packageStatus,
    updateVersion,
    releaseNotes,
    publishedAt,
    dismissedVersion,
    downloadedBytes,
    contentLength,
    errorMessage,
    visible,
    start,
    checkNow: runCheck,
    dismiss,
    install,
    restartNow,
    __e2eInjectUpdate,
    __e2eInjectPackageStatus,
    dispose,
  };
}
