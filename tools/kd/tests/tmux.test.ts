import { describe, expect, it } from "vitest";
import { tmuxWindowEnvArgs, tmuxWindowEnvKeys } from "../src/runtime/tmux";

const sessionEnv = {
  KANNA_DESKTOP_AUTO_SIGN_IN_EMAIL: "dev@example.com",
  WAYLAND_DISPLAY: "wayland-0",
  DISPLAY: ":1",
  XDG_RUNTIME_DIR: "/run/user/1000",
  DBUS_SESSION_BUS_ADDRESS: "unix:path=/run/user/1000/bus",
  XDG_SESSION_TYPE: "wayland",
  SHELL: "/bin/bash"
};

describe("tmuxWindowEnvArgs", () => {
  it("carries the Linux display session into the window", () => {
    expect(tmuxWindowEnvArgs(sessionEnv, "linux")).toEqual([
      "KANNA_DESKTOP_AUTO_SIGN_IN_EMAIL=dev@example.com",
      "WAYLAND_DISPLAY=wayland-0",
      "DISPLAY=:1",
      "XDG_RUNTIME_DIR=/run/user/1000",
      "XDG_SESSION_TYPE=wayland",
      "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus"
    ]);
  });

  it("leaves macOS windows with the environment they already had", () => {
    expect(tmuxWindowEnvArgs(sessionEnv, "darwin")).toEqual([
      "KANNA_DESKTOP_AUTO_SIGN_IN_EMAIL=dev@example.com"
    ]);
  });

  it("names no display variable that is absent, so a headless Linux run stays unchanged", () => {
    expect(tmuxWindowEnvArgs({ KANNA_DB_PATH: "/tmp/kanna.db" }, "linux")).toEqual([]);
  });

  it("forwards nothing that the window's own command already sets", () => {
    // The desktop window builds its own KANNA_E2E_* prefix; -e is only for the
    // values a window cannot reconstruct from its command line.
    expect(tmuxWindowEnvKeys("linux")).not.toContain("KANNA_E2E_AGENT_COMMAND");
  });
});
