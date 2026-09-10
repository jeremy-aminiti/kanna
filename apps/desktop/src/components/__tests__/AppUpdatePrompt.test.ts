// @vitest-environment happy-dom

import { mount } from "@vue/test-utils";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { computed, ref } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import AppUpdatePrompt from "../AppUpdatePrompt.vue";

vi.mock("vue-i18n", () => ({
  useI18n: () => ({
    t: (key: string, params?: Record<string, string | number>) =>
      params ? `${key}:${JSON.stringify(params)}` : key,
  }),
}));

function makeController(
  overrides: {
    status?:
      | "idle"
      | "checking"
      | "available"
      | "downloading"
      | "readyToRestart"
      | "error"
      | "packageManagerUpdate"
      | "packageManagerUnknown";
    packageStatus?: {
      packageManaged: boolean;
      packageName: string;
      installedVersion: string | null;
      candidateVersion: string | null;
      updateAvailable: boolean;
      metadataUnavailable: boolean;
      detail: string | null;
    } | null;
    updateVersion?: string | null;
    releaseNotes?: string | null;
    publishedAt?: string | null;
    dismissedVersion?: string | null;
    downloadedBytes?: number;
    contentLength?: number | null;
    errorMessage?: string | null;
  } = {},
) {
  const status = ref<NonNullable<Parameters<typeof makeController>[0]>["status"]>(
    overrides.status ?? "available",
  );

  return {
    status,
    packageStatus: ref(overrides.packageStatus ?? null),
    updateVersion: ref(overrides.updateVersion ?? "0.0.39"),
    releaseNotes: ref(overrides.releaseNotes ?? "Notes for 0.0.39"),
    publishedAt: ref(overrides.publishedAt ?? "2026-04-15T00:00:00Z"),
    dismissedVersion: ref<string | null>(overrides.dismissedVersion ?? null),
    downloadedBytes: ref(overrides.downloadedBytes ?? 0),
    contentLength: ref<number | null>(overrides.contentLength ?? null),
    errorMessage: ref<string | null>(overrides.errorMessage ?? null),
    visible: computed(() => status.value !== "idle" && status.value !== "checking"),
    start: vi.fn(),
    checkNow: vi.fn(),
    dismiss: vi.fn(),
    install: vi.fn(),
    restartNow: vi.fn(),
    dispose: vi.fn(),
  };
}

describe("AppUpdatePrompt", () => {
  afterEach(() => {
    document.body.innerHTML = "";
  });

  it("shows the available update details and actions", () => {
    const wrapper = mount(AppUpdatePrompt, {
      props: {
        controller: makeController(),
      },
      global: {
        mocks: {
          $t: (key: string) => key,
        },
      },
    });

    expect(wrapper.text()).toContain("app.update.available");
    expect(wrapper.text()).toContain("0.0.39");
    expect(wrapper.text()).toContain("Notes for 0.0.39");
    expect(wrapper.find(".update-prompt").attributes("role")).toBeUndefined();
    expect(wrapper.get(".update-prompt__status").attributes("role")).toBe("status");
    expect(wrapper.get(".update-prompt__status").attributes("aria-live")).toBe("polite");
    expect(wrapper.get('[data-testid="update-install"]').text()).toBe("app.update.install");
    expect(wrapper.get('[data-testid="update-dismiss"]').attributes("aria-label")).toBe("actions.dismiss");
  });

  it("renders download progress while installing", async () => {
    const controller = makeController({
      status: "downloading",
      downloadedBytes: 12,
      contentLength: 42,
    });

    const wrapper = mount(AppUpdatePrompt, {
      props: { controller },
      global: {
        mocks: {
          $t: (key: string) => key,
        },
      },
    });

    expect(wrapper.text()).toContain("app.update.downloading");
    expect(wrapper.text()).toContain("12");
    expect(wrapper.text()).toContain("42");
  });

  it("uses an indeterminate progress bar when content length is zero", async () => {
    const controller = makeController({
      status: "downloading",
      downloadedBytes: 12,
      contentLength: 0,
    });

    const wrapper = mount(AppUpdatePrompt, {
      props: { controller },
      global: {
        mocks: {
          $t: (key: string) => key,
        },
      },
    });

    const progress = wrapper.get(".update-prompt__progress");
    expect(progress.element.tagName).toBe("PROGRESS");
    expect(progress.attributes("value")).toBeUndefined();
    expect(progress.attributes("max")).toBeUndefined();
    expect(wrapper.findAll(".update-prompt__progress")).toHaveLength(1);
  });

  it("shows the restart action after a successful install", () => {
    const controller = makeController({ status: "readyToRestart" });

    const wrapper = mount(AppUpdatePrompt, {
      props: { controller },
      global: {
        mocks: {
          $t: (key: string) => key,
        },
      },
    });

    expect(wrapper.text()).toContain("app.update.readyToRestart");
    expect(wrapper.get('[data-testid="update-restart"]').text()).toBe("app.update.restartNow");
    expect(wrapper.get('[data-testid="update-later"]').text()).toBe("app.update.later");
  });

  it("shows the install error and retry action", () => {
    const controller = makeController({
      status: "error",
      errorMessage: "download failed",
    });

    const wrapper = mount(AppUpdatePrompt, {
      props: { controller },
      global: {
        mocks: {
          $t: (key: string) => key,
        },
      },
    });

    expect(wrapper.text()).toContain("app.update.error");
    expect(wrapper.text()).toContain("download failed");
    expect(wrapper.get('[data-testid="update-retry"]').text()).toBe("app.update.retry");
  });

  it("keeps long release notes inside a bounded prompt", () => {
    const source = readFileSync(resolve(__dirname, "../AppUpdatePrompt.vue"), "utf8");

    expect(source).toMatch(/\.update-prompt\s*{[^}]*max-height:\s*min\(640px,\s*calc\(100vh - 32px\)\)/s);
    expect(source).toMatch(/\.update-prompt\s*{[^}]*display:\s*grid/s);
    expect(source).toMatch(/\.update-prompt__body\s*{[^}]*overflow-y:\s*auto/s);
  });

  /**
   * The Linux states. The distinguishing property is negative: whatever else
   * this panel shows, it must never offer an action the app cannot perform.
   * apt owns the upgrade, and a button that looked like Install would leave a
   * person believing they had started one.
   */
  describe("on a package-managed installation", () => {
    const packageStatus = {
      packageManaged: true,
      packageName: "kanna",
      installedVersion: "1.2.3-1",
      candidateVersion: "1.2.4-1",
      updateAvailable: true,
      metadataUnavailable: false,
      detail: null,
    };

    function mountWith(overrides: Parameters<typeof makeController>[0]) {
      return mount(AppUpdatePrompt, {
        props: { controller: makeController(overrides) },
        global: { mocks: { $t: (key: string) => key } },
      });
    }

    it("says the package manager owns the update, and shows both versions", () => {
      const wrapper = mountWith({ status: "packageManagerUpdate", packageStatus });
      expect(wrapper.text()).toContain("app.update.packageManagerUpdate");
      expect(wrapper.text()).toContain("1.2.3-1");
      expect(wrapper.text()).toContain("1.2.4-1");
      expect(wrapper.get('[data-testid="update-package-command"]').text()).toContain("kanna");
    });

    it("offers no install or restart action", () => {
      const wrapper = mountWith({ status: "packageManagerUpdate", packageStatus });
      expect(wrapper.find('[data-testid="update-install"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="update-restart"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="update-retry"]').exists()).toBe(false);
      expect(wrapper.get('[data-testid="update-dismiss-button"]').exists()).toBe(true);
    });

    /** "We cannot tell" is shown as itself. Rounding it down to silence would
     *  hide a real update behind a reassuring nothing. */
    it("says plainly when the package index cannot answer", () => {
      const wrapper = mountWith({
        status: "packageManagerUnknown",
        packageStatus: {
          ...packageStatus,
          candidateVersion: null,
          updateAvailable: false,
          metadataUnavailable: true,
          detail: "Run `sudo apt update` to refresh it.",
        },
      });
      expect(wrapper.text()).toContain("app.update.packageManagerUnknown");
      expect(wrapper.text()).toContain("sudo apt update");
      expect(wrapper.find('[data-testid="update-install"]').exists()).toBe(false);
    });
  });
});
