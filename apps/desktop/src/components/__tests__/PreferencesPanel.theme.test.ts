// @vitest-environment happy-dom

import { mount } from "@vue/test-utils";
import { AGENT_PROVIDERS } from "@kanna/agent-protocol";
import { describe, expect, it, vi } from "vitest";
import PreferencesPanel from "../PreferencesPanel.vue";
import en from "../../i18n/locales/en.json";

vi.mock("../../services/desktopAuthSdk", () => ({
  getConfiguredDesktopAuthSession: vi.fn(async () => ({
    initialize: vi.fn(async () => {}),
    subscribe: vi.fn((next) => {
      next({ status: "signedOut" });
      return () => undefined;
    }),
  })),
}));

vi.mock("../../invoke", () => ({
  invoke: vi.fn(async () => ({ state: "stopped" })),
}));

vi.mock("vue-i18n", () => ({
  useI18n: () => ({ t: (key: string) => key }),
}));

function mountPreferences(defaultAgentType: "pty" | "agent" = "pty") {
  return mount(PreferencesPanel, {
    props: {
      preferences: {
        suspendAfterMinutes: 5,
        killAfterMinutes: 30,
        ideCommand: "code",
        locale: "en",
        devLingerTerminals: false,
        defaultAgentProvider: "claude",
        defaultAgentType,
        appTheme: "dark",
        codeTheme: "match",
        agentMessageAppearance: "chat",
      },
    },
    global: {
      mocks: {
        $t: (key: string) => key,
      },
    },
  });
}

describe("PreferencesPanel theme controls", () => {
  it("renders app, code, and agent message appearance selectors", () => {
    const wrapper = mountPreferences();

    const appTheme = wrapper.get('[data-testid="app-theme-select"]');
    const codeTheme = wrapper.get('[data-testid="code-theme-select"]');
    const appearance = wrapper.get('[data-testid="agent-message-appearance-select"]');

    expect(appTheme.element).toHaveProperty("value", "dark");
    expect(codeTheme.element).toHaveProperty("value", "match");
    expect(appearance.element).toHaveProperty("value", "chat");
    expect(wrapper.text()).toContain("preferences.theme");
    expect(wrapper.text()).toContain("preferences.codeTheme");
    expect(wrapper.text()).toContain("preferences.agentMessageAppearance");
  });

  it("emits theme and appearance preference updates", async () => {
    const wrapper = mountPreferences();

    await wrapper.get('[data-testid="app-theme-select"]').setValue("light");
    await wrapper.get('[data-testid="code-theme-select"]').setValue("dark");
    await wrapper.get('[data-testid="agent-message-appearance-select"]').setValue("terminal");

    expect(wrapper.emitted("update")).toContainEqual(["appTheme", "light"]);
    expect(wrapper.emitted("update")).toContainEqual(["codeTheme", "dark"]);
    expect(wrapper.emitted("update")).toContainEqual(["agentMessageAppearance", "terminal"]);
  });

  it("offers only provider choices for terminal defaults", () => {
    const wrapper = mountPreferences();
    const defaultAgentSelect = wrapper.get('[data-testid="default-agent-select"]');

    expect(en.preferences.defaultAgent).toBe("Default agent");
    expect(defaultAgentSelect.findAll("option").map((option) => option.text())).toEqual(AGENT_PROVIDERS);
  });

  it("resolves a persisted legacy agent preference to its provider choice", () => {
    const wrapper = mountPreferences("agent");
    const defaultAgentSelect = wrapper.get('[data-testid="default-agent-select"]');

    expect(defaultAgentSelect.element).toHaveProperty("value", "claude");
    expect((defaultAgentSelect.element as HTMLSelectElement).selectedIndex).not.toBe(-1);
  });

  it("stores pty execution when changing the default provider", async () => {
    const wrapper = mountPreferences("agent");
    const defaultAgentSelect = wrapper.get('[data-testid="default-agent-select"]');

    await defaultAgentSelect.setValue("opencode");

    expect(wrapper.emitted("update")).toContainEqual(["defaultAgentProvider", "opencode"]);
    expect(wrapper.emitted("update")).toContainEqual(["defaultAgentType", "pty"]);
    expect(wrapper.emitted("update")).not.toContainEqual(["defaultAgentType", "agent"]);
  });

  it("ignores an invalid provider selection", async () => {
    const wrapper = mountPreferences();
    const select = wrapper.get('[data-testid="default-agent-select"]');
    (select.element as HTMLSelectElement).value = "future-agent";

    await select.trigger("change");

    expect(wrapper.emitted("update")).toBeUndefined();
  });
});
