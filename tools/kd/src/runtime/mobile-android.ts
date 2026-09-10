import { spawn } from "node:child_process";
import { closeSync, existsSync, mkdirSync, openSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { CommandRunner } from "./process";

export interface AndroidSdkTools {
  root: string;
  adb: string;
  emulator: string;
  sdkmanager?: string;
  avdmanager?: string;
}

export interface AndroidVirtualDevice {
  name: string;
  serial?: string;
  running: boolean;
}

export interface AndroidCommand {
  command: string;
  args: string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  logPath?: string;
}

export interface AndroidEmulatorWaitOptions {
  attempts?: number;
  delayMs?: number;
  wait?: (milliseconds: number) => Promise<void>;
}

function commandLineTool(root: string, name: string): string | undefined {
  const commandLineTools = join(root, "cmdline-tools");
  if (!existsSync(commandLineTools)) return undefined;
  const versions = readdirSync(commandLineTools).sort((left, right) =>
    left === "latest" ? -1 : right === "latest" ? 1 : right.localeCompare(left)
  );
  return versions
    .map((version) => join(commandLineTools, version, "bin", name))
    .find((candidate) => existsSync(candidate));
}

export function resolveAndroidSdkTools(
  env: NodeJS.ProcessEnv,
  userHome = homedir()
): AndroidSdkTools {
  const androidHome = env.ANDROID_HOME?.trim();
  const androidSdkRoot = env.ANDROID_SDK_ROOT?.trim();
  if (
    androidHome &&
    androidSdkRoot &&
    resolve(androidHome) !== resolve(androidSdkRoot)
  ) {
    throw new Error(
      `ANDROID_HOME and ANDROID_SDK_ROOT disagree (${androidHome} vs ${androidSdkRoot}). Point both at one Android SDK.`
    );
  }

  const root = resolve(androidHome || androidSdkRoot || join(userHome, "Library", "Android", "sdk"));
  if (!existsSync(root)) {
    throw new Error(
      `Android SDK was not found at ${root}. Set ANDROID_HOME or ANDROID_SDK_ROOT; kd will not install a toolchain implicitly.`
    );
  }
  return {
    root,
    adb: join(root, "platform-tools", "adb"),
    emulator: join(root, "emulator", "emulator"),
    sdkmanager: commandLineTool(root, "sdkmanager"),
    avdmanager: commandLineTool(root, "avdmanager")
  };
}

export function missingRequiredAndroidTools(tools: AndroidSdkTools): string[] {
  return [tools.adb, tools.emulator].filter((candidate) => !existsSync(candidate));
}

export function parseAndroidAvdList(stdout: string): string[] {
  return Array.from(new Set(
    stdout.split(/\r?\n/).map((line) => line.trim()).filter(Boolean)
  ));
}

export function parseAdbEmulatorSerials(stdout: string): string[] {
  return stdout
    .split(/\r?\n/)
    .slice(1)
    .map((line) => line.trim().split(/\s+/))
    .filter(([serial, state]) => serial?.startsWith("emulator-") && state === "device")
    .map(([serial]) => serial);
}

async function runningAndroidAvds(
  runner: CommandRunner,
  tools: AndroidSdkTools
): Promise<Map<string, string>> {
  const devices = await runner.run(tools.adb, ["devices"]);
  if (devices.exitCode !== 0) return new Map();

  const entries = await Promise.all(
    parseAdbEmulatorSerials(devices.stdout).map(async (serial) => {
      const result = await runner.run(tools.adb, ["-s", serial, "emu", "avd", "name"]);
      const name = result.stdout
        .split(/\r?\n/)
        .map((line) => line.trim())
        .find((line) => line && line !== "OK");
      return result.exitCode === 0 && name ? ([name, serial] as const) : null;
    })
  );
  return new Map(entries.filter((entry): entry is readonly [string, string] => entry !== null));
}

export async function listAndroidVirtualDevices(
  runner: CommandRunner,
  tools: AndroidSdkTools
): Promise<AndroidVirtualDevice[]> {
  const available = await runner.run(tools.emulator, ["-list-avds"]);
  if (available.exitCode !== 0) {
    throw new Error(
      available.stderr.trim() || available.stdout.trim() || "Failed to list Android virtual devices."
    );
  }
  const running = await runningAndroidAvds(runner, tools);
  return parseAndroidAvdList(available.stdout).map((name) => ({
    name,
    serial: running.get(name),
    running: running.has(name)
  }));
}

export function selectAndroidVirtualDevice(
  devices: readonly AndroidVirtualDevice[],
  requested?: string
): AndroidVirtualDevice {
  if (requested) {
    const selected = devices.find((device) => device.name === requested);
    if (!selected) {
      throw new Error(
        `Android AVD ${requested} was not found. Available AVDs: ${devices.map((device) => device.name).join(", ") || "<none>"}.`
      );
    }
    return selected;
  }
  const running = devices.filter((device) => device.running);
  if (running.length === 1) return running[0];
  if (devices.length === 1) return devices[0];
  if (devices.length === 0) {
    throw new Error("No Android AVDs are installed. Create one in Android Studio first; kd will not install one implicitly.");
  }
  throw new Error(
    `Multiple Android AVDs are available (${devices.map((device) => device.name).join(", ")}). Pass --android-emulator <avd> or set KANNA_ANDROID_AVD.`
  );
}

export async function resolveAndroidVirtualDevice(input: {
  runner: CommandRunner;
  tools: AndroidSdkTools;
  requested?: string;
}): Promise<AndroidVirtualDevice> {
  return selectAndroidVirtualDevice(
    await listAndroidVirtualDevices(input.runner, input.tools),
    input.requested
  );
}

export function buildAndroidEmulatorLaunchCommand(
  tools: AndroidSdkTools,
  avd: string,
  repoRoot?: string
): AndroidCommand {
  return {
    command: tools.emulator,
    args: ["-avd", avd, "-no-snapshot-save"],
    env: {
      ...process.env,
      ANDROID_HOME: tools.root,
      ANDROID_SDK_ROOT: tools.root
    },
    ...(repoRoot
      ? {
          logPath: join(
            repoRoot,
            ".build",
            "mobile",
            `android-emulator-${avd.replace(/[^A-Za-z0-9._-]/g, "_")}.log`
          )
        }
      : {})
  };
}

export async function launchAndroidEmulator(command: AndroidCommand): Promise<void> {
  await new Promise<void>((resolveLaunch, reject) => {
    let logFd: number | undefined;
    if (command.logPath) {
      mkdirSync(dirname(command.logPath), { recursive: true });
      logFd = openSync(command.logPath, "a");
    }
    const child = spawn(command.command, command.args, {
      detached: true,
      env: command.env,
      stdio: logFd === undefined ? "ignore" : ["ignore", logFd, logFd]
    });
    if (logFd !== undefined) closeSync(logFd);
    child.once("error", reject);
    child.once("spawn", () => {
      child.unref();
      resolveLaunch();
    });
  });
}

export async function waitForAndroidVirtualDevice(input: {
  avd: string;
  runner: CommandRunner;
  tools: AndroidSdkTools;
  options?: AndroidEmulatorWaitOptions;
}): Promise<AndroidVirtualDevice> {
  const attempts = input.options?.attempts ?? 180;
  const delayMs = input.options?.delayMs ?? 1_000;
  const wait = input.options?.wait ?? ((milliseconds) =>
    new Promise<void>((resolveWait) => setTimeout(resolveWait, milliseconds))
  );
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const device = (await listAndroidVirtualDevices(input.runner, input.tools))
      .find((candidate) => candidate.name === input.avd && candidate.serial);
    if (device?.serial) {
      const boot = await input.runner.run(input.tools.adb, [
        "-s",
        device.serial,
        "shell",
        "getprop",
        "sys.boot_completed"
      ]);
      if (boot.exitCode === 0 && boot.stdout.trim() === "1") return device;
    }
    await wait(delayMs);
  }
  throw new Error(`Android AVD ${input.avd} did not finish booting.`);
}

export function buildAndroidPrebuildCommand(input: {
  repoRoot: string;
  appEnv: string;
}): AndroidCommand {
  return {
    command: "pnpm",
    args: [
      "--dir",
      join(input.repoRoot, "apps", "mobile"),
      "exec",
      "expo",
      "prebuild",
      "--platform",
      "android"
    ],
    cwd: input.repoRoot,
    env: { KANNA_APP_ENV: input.appEnv }
  };
}

export function buildAndroidRunCommand(input: {
  repoRoot: string;
  deviceName: string;
  packageId: string;
  metroPort: number;
  appEnv: string;
  tools: AndroidSdkTools;
}): AndroidCommand {
  return {
    command: "pnpm",
    args: [
      "--dir",
      join(input.repoRoot, "apps", "mobile"),
      "android",
      "--device",
      input.deviceName,
      "--app-id",
      input.packageId,
      "--port",
      String(input.metroPort)
    ],
    cwd: input.repoRoot,
    env: {
      KANNA_APP_ENV: input.appEnv,
      ANDROID_HOME: input.tools.root,
      ANDROID_SDK_ROOT: input.tools.root,
      REACT_NATIVE_PACKAGER_HOSTNAME: "10.0.2.2",
      RCT_METRO_PORT: String(input.metroPort)
    }
  };
}
