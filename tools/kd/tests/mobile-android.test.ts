import { describe, expect, it } from "vitest";
import {
  buildAndroidPrebuildCommand,
  buildAndroidRunCommand,
  parseAdbEmulatorSerials,
  parseAndroidAvdList,
  selectAndroidVirtualDevice
} from "../src/runtime/mobile-android";

describe("Android emulator mobile runtime", () => {
  it("parses installed AVDs and running emulator serials", () => {
    expect(parseAndroidAvdList("Medium_Phone_API_36.1\nPixel_9_API_35\n")).toEqual([
      "Medium_Phone_API_36.1",
      "Pixel_9_API_35"
    ]);
    expect(parseAdbEmulatorSerials(
      "List of devices attached\nemulator-5554 device product:sdk_gphone\nphone-1 device\nemulator-5556 offline\n"
    )).toEqual(["emulator-5554"]);
  });

  it("selects an exact requested AVD and otherwise prefers the one running AVD", () => {
    const devices = [
      { name: "Medium_Phone_API_36.1", running: false },
      { name: "Pixel_9_API_35", serial: "emulator-5554", running: true }
    ];
    expect(selectAndroidVirtualDevice(devices, "Medium_Phone_API_36.1").name)
      .toBe("Medium_Phone_API_36.1");
    expect(selectAndroidVirtualDevice(devices).name).toBe("Pixel_9_API_35");
  });

  it("refuses an ambiguous or missing target", () => {
    expect(() => selectAndroidVirtualDevice([
      { name: "one", running: false },
      { name: "two", running: false }
    ])).toThrow(/Pass --android-emulator/);
    expect(() => selectAndroidVirtualDevice([], "missing")).toThrow(/was not found/);
  });

  it("builds Android-only prebuild and task-scoped emulator launch commands", () => {
    expect(buildAndroidPrebuildCommand({ repoRoot: "/repo", appEnv: "dev" })).toMatchObject({
      args: ["--dir", "/repo/apps/mobile", "exec", "expo", "prebuild", "--platform", "android"],
      env: { KANNA_APP_ENV: "dev" }
    });
    expect(buildAndroidRunCommand({
      repoRoot: "/repo",
      deviceName: "Medium_Phone_API_36.1",
      packageId: "build.kanna.app.dev",
      metroPort: 8082,
      appEnv: "dev",
      tools: {
        root: "/sdk",
        adb: "/sdk/platform-tools/adb",
        emulator: "/sdk/emulator/emulator"
      }
    })).toMatchObject({
      args: [
        "--dir", "/repo/apps/mobile", "android",
        "--device", "Medium_Phone_API_36.1",
        "--app-id", "build.kanna.app.dev",
        "--port", "8082"
      ],
      env: {
        KANNA_APP_ENV: "dev",
        ANDROID_HOME: "/sdk",
        ANDROID_SDK_ROOT: "/sdk",
        REACT_NATIVE_PACKAGER_HOSTNAME: "10.0.2.2",
        RCT_METRO_PORT: "8082"
      }
    });
  });
});
