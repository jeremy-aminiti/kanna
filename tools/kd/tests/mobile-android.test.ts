import { describe, expect, it } from "vitest";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import {
  buildAndroidPhysicalRunPlan,
  buildAndroidPrebuildCommand,
  buildAndroidReverseCommands,
  buildAndroidRunCommand,
  cleanupOwnedAndroidReverseRoutes,
  executeAndroidPhysicalRunPlan,
  parseAdbEmulatorSerials,
  parseAdbPhysicalDevices,
  parseAdbReverseList,
  parseAndroidAvdList,
  resolveAndroidPhysicalDevice,
  setupAndroidReverseRoutes,
  selectAndroidVirtualDevice
} from "../src/runtime/mobile-android";
import type { CommandRunner } from "../src/runtime/process";
import { kdTestScratchDir } from "./test-paths";

describe("Android emulator mobile runtime", () => {
  it("parses installed AVDs and running emulator serials", () => {
    expect(parseAndroidAvdList("Medium_Phone_API_36.1\nPixel_9_API_35\n")).toEqual([
      "Medium_Phone_API_36.1",
      "Pixel_9_API_35"
    ]);
    expect(parseAdbEmulatorSerials(
      "List of devices attached\nemulator-5554 device product:sdk_gphone\nphone-1 device\nemulator-5556 offline\n"
    )).toEqual(["emulator-5554"]);
    expect(parseAdbPhysicalDevices(
      "List of devices attached\nR5CX42N3NLK device usb:5-1 product:a15xcs model:SM_A156W device:a15x\nemulator-5554 device product:sdk_gphone\nphone-2 unauthorized\n"
    )).toEqual([{ serial: "R5CX42N3NLK", model: "SM_A156W", device: "a15x" }]);
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

  it("executes physical install and launch only through the resolved exact serial", async () => {
    const tools = {
      root: "/sdk",
      adb: "/sdk/platform-tools/adb",
      emulator: "/sdk/emulator/emulator"
    };
    expect(buildAndroidReverseCommands({
      tools,
      serial: "R5CX42N3NLK",
      ports: [8082, 48122, 8082]
    })).toEqual([
      { command: tools.adb, args: ["-s", "R5CX42N3NLK", "reverse", "tcp:8082", "tcp:8082"] },
      { command: tools.adb, args: ["-s", "R5CX42N3NLK", "reverse", "tcp:48122", "tcp:48122"] }
    ]);
    const calls: string[][] = [];
    const runner: CommandRunner = {
      async run(_command, args) {
        calls.push(args);
        if (args[0] === "devices") {
          return {
            exitCode: 0,
            stdout: "List of devices attached\nUNRELATED_SAME_MODEL device model:SM_A156W device:a15x\nR5CX42N3NLK device model:SM_A156W device:a15x\n",
            stderr: ""
          };
        }
        return { exitCode: 0, stdout: "", stderr: "" };
      }
    };
    const selected = await resolveAndroidPhysicalDevice({
      runner,
      tools,
      serial: "R5CX42N3NLK"
    });
    const result = await executeAndroidPhysicalRunPlan({
      runner,
      plan: buildAndroidPhysicalRunPlan({
        repoRoot: "/repo",
        serial: selected.serial,
        packageId: "build.kanna.app.dev",
        metroPort: 8082,
        appEnv: "dev",
        tools
      })
    });
    expect(result).toMatchObject({ ok: true, step: "launch" });
    const deviceMutations = calls.filter((args) => args[0] !== "devices");
    expect(deviceMutations).toHaveLength(4);
    expect(deviceMutations.slice(1).every((args) =>
      args[0] === "-s" && args[1] === "R5CX42N3NLK"
    )).toBe(true);
    expect(deviceMutations.flat()).not.toContain("UNRELATED_SAME_MODEL");
    expect(deviceMutations[3]).toContain(
      "exp+kanna-mobile://expo-development-client/?url=http%3A%2F%2F127.0.0.1%3A8082"
    );

    const callsBeforeMissing = calls.length;
    await expect(resolveAndroidPhysicalDevice({
      runner,
      tools,
      serial: "NOT_AUTHORIZED"
    })).rejects.toThrow(/not connected and authorized/);
    expect(calls.slice(callsBeforeMissing)).toEqual([["devices", "-l"]]);

    expect(buildAndroidRunCommand({
      repoRoot: "/repo",
      deviceName: "Medium_Phone_API_36.1",
      packageId: "build.kanna.app.dev",
      metroPort: 8082,
      appEnv: "dev",
      tools,
      packagerHost: "10.0.2.2"
    })).toMatchObject({
      args: expect.arrayContaining(["--device", "Medium_Phone_API_36.1"]),
      env: {
        KANNA_APP_ENV: "dev",
        ANDROID_HOME: "/sdk",
        ANDROID_SDK_ROOT: "/sdk",
        REACT_NATIVE_PACKAGER_HOSTNAME: "10.0.2.2",
        RCT_METRO_PORT: "8082"
      }
    });
  });

  it("owns only new serial-scoped reverse routes and cleans them safely", async () => {
    const repoRoot = await kdTestScratchDir("kanna-kd-android-reverse-");
    const sdkRoot = join(repoRoot, "sdk");
    await mkdir(join(sdkRoot, "platform-tools"), { recursive: true });
    const tools = {
      root: sdkRoot,
      adb: join(sdkRoot, "platform-tools", "adb"),
      emulator: join(sdkRoot, "emulator", "emulator")
    };
    const routes = new Map([
      ["R5CX42N3NLK", new Map([["tcp:8082", "tcp:8082"]])],
      ["UNRELATED", new Map([["tcp:48122", "tcp:59999"]])]
    ]);
    let failRemote = "tcp:9082";
    const calls: string[][] = [];
    const runner: CommandRunner = {
      async run(_command, args) {
        calls.push(args);
        const serial = args[1];
        const deviceRoutes = routes.get(serial) ?? new Map<string, string>();
        routes.set(serial, deviceRoutes);
        if (args[3] === "--list") {
          return {
            exitCode: 0,
            stdout: Array.from(deviceRoutes, ([remote, local]) => `${serial} ${remote} ${local}`).join("\n"),
            stderr: ""
          };
        }
        if (args[3] === "--remove") {
          deviceRoutes.delete(args[4]);
          return { exitCode: 0, stdout: "", stderr: "" };
        }
        if (args[3] === "--no-rebind" && args[4] === failRemote) {
          return { exitCode: 1, stdout: "", stderr: "injected reverse failure" };
        }
        if (args[3] === "--no-rebind") {
          if (deviceRoutes.has(args[4])) {
            return { exitCode: 1, stdout: "", stderr: "cannot rebind existing socket" };
          }
          deviceRoutes.set(args[4], args[5]);
        }
        return { exitCode: 0, stdout: "", stderr: "" };
      }
    };

    expect(parseAdbReverseList("R5CX42N3NLK tcp:8082 tcp:8082\n"))
      .toEqual([{ remote: "tcp:8082", local: "tcp:8082" }]);
    await expect(setupAndroidReverseRoutes({
      repoRoot,
      runner,
      tools,
      serial: "R5CX42N3NLK",
      ports: [8082, 48122, 9082]
    })).rejects.toThrow("injected reverse failure");
    expect(routes.get("R5CX42N3NLK")).toEqual(new Map([["tcp:8082", "tcp:8082"]]));
    expect(routes.get("UNRELATED")).toEqual(new Map([["tcp:48122", "tcp:59999"]]));

    failRemote = "";
    const setup = await setupAndroidReverseRoutes({
      repoRoot,
      runner,
      tools,
      serial: "R5CX42N3NLK",
      ports: [8082, 48122]
    });
    expect(setup).toEqual({
      created: [{ remote: "tcp:48122", local: "tcp:48122" }],
      preexisting: [{ remote: "tcp:8082", local: "tcp:8082" }],
      owned: [{ remote: "tcp:48122", local: "tcp:48122" }]
    });
    expect(routes.get("R5CX42N3NLK")?.get("tcp:48122")).toBe("tcp:48122");
    expect(calls.some((args) => args[1] === "UNRELATED")).toBe(false);

    const cleanup = await cleanupOwnedAndroidReverseRoutes({ repoRoot, runner, tools });
    expect(cleanup).toEqual({
      cleaned: [{ remote: "tcp:48122", local: "tcp:48122" }],
      skipped: [],
      failed: []
    });
    expect(routes.get("R5CX42N3NLK")).toEqual(new Map([["tcp:8082", "tcp:8082"]]));
    expect(routes.get("UNRELATED")).toEqual(new Map([["tcp:48122", "tcp:59999"]]));
  });

  it("does not rebind a route created after setup's initial listing", async () => {
    const repoRoot = await kdTestScratchDir("kanna-kd-android-reverse-raced-create-");
    const tools = {
      root: "/sdk",
      adb: "/sdk/platform-tools/adb",
      emulator: "/sdk/emulator/emulator"
    };
    const routes = new Map<string, string>();
    const calls: string[][] = [];
    const runner: CommandRunner = {
      async run(_command, args) {
        calls.push(args);
        if (args[3] === "--list") {
          return {
            exitCode: 0,
            stdout: Array.from(routes, ([remote, local]) => `R5CX42N3NLK ${remote} ${local}`).join("\n"),
            stderr: ""
          };
        }
        if (args[3] === "--no-rebind") {
          routes.set("tcp:48122", "tcp:59999");
          return { exitCode: 1, stdout: "", stderr: "cannot rebind existing socket" };
        }
        throw new Error(`Unexpected fake-adb command: ${args.join(" ")}`);
      }
    };

    await expect(setupAndroidReverseRoutes({
      repoRoot,
      runner,
      tools,
      serial: "R5CX42N3NLK",
      ports: [48122]
    })).rejects.toThrow("cannot rebind existing socket");
    expect(routes).toEqual(new Map([["tcp:48122", "tcp:59999"]]));
    expect(calls).toContainEqual([
      "-s", "R5CX42N3NLK", "reverse", "--no-rebind", "tcp:48122", "tcp:48122"
    ]);
    expect(calls.some((args) => args[3] === "--remove")).toBe(false);
  });

  it("does not roll back a created route after another process changes it", async () => {
    const repoRoot = await kdTestScratchDir("kanna-kd-android-reverse-raced-rollback-");
    const tools = {
      root: "/sdk",
      adb: "/sdk/platform-tools/adb",
      emulator: "/sdk/emulator/emulator"
    };
    const routes = new Map<string, string>();
    const calls: string[][] = [];
    const runner: CommandRunner = {
      async run(_command, args) {
        calls.push(args);
        if (args[3] === "--list") {
          return {
            exitCode: 0,
            stdout: Array.from(routes, ([remote, local]) => `R5CX42N3NLK ${remote} ${local}`).join("\n"),
            stderr: ""
          };
        }
        if (args[3] === "--no-rebind" && args[4] === "tcp:48122") {
          routes.set(args[4], args[5]);
          return { exitCode: 0, stdout: "", stderr: "" };
        }
        if (args[3] === "--no-rebind" && args[4] === "tcp:9082") {
          routes.set("tcp:48122", "tcp:59999");
          return { exitCode: 1, stdout: "", stderr: "injected later failure" };
        }
        if (args[3] === "--remove") {
          routes.delete(args[4]);
          return { exitCode: 0, stdout: "", stderr: "" };
        }
        throw new Error(`Unexpected fake-adb command: ${args.join(" ")}`);
      }
    };

    await expect(setupAndroidReverseRoutes({
      repoRoot,
      runner,
      tools,
      serial: "R5CX42N3NLK",
      ports: [48122, 9082]
    })).rejects.toThrow("injected later failure");
    expect(routes).toEqual(new Map([["tcp:48122", "tcp:59999"]]));
    expect(calls.filter((args) => args[3] === "--list")).toHaveLength(2);
    expect(calls.some((args) => args[3] === "--remove")).toBe(false);
  });
});
