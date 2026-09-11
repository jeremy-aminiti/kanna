import { spawn } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  writeFileSync
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { CommandResult, CommandRunner } from "./process";

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

export interface AndroidPhysicalDevice {
  serial: string;
  model?: string;
  device?: string;
}

export interface AndroidCommand {
  command: string;
  args: string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  logPath?: string;
}

export interface AndroidReverseMapping {
  remote: string;
  local: string;
}

interface AndroidReverseOwnership {
  version: 1;
  devices: Array<{
    serial: string;
    mappings: AndroidReverseMapping[];
  }>;
}

export interface AndroidReverseSetupResult {
  created: AndroidReverseMapping[];
  preexisting: AndroidReverseMapping[];
  owned: AndroidReverseMapping[];
}

export interface AndroidReverseCleanupResult {
  cleaned: AndroidReverseMapping[];
  skipped: AndroidReverseMapping[];
  failed: AndroidReverseMapping[];
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

export function missingRequiredAndroidDeviceTools(tools: AndroidSdkTools): string[] {
  return [tools.adb].filter((candidate) => !existsSync(candidate));
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

export function parseAdbPhysicalDevices(stdout: string): AndroidPhysicalDevice[] {
  return stdout
    .split(/\r?\n/)
    .slice(1)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => line.split(/\s+/))
    .filter(([serial, state]) => Boolean(serial) && state === "device" && !serial.startsWith("emulator-"))
    .map(([serial, _state, ...attributes]) => {
      const values = Object.fromEntries(attributes.flatMap((attribute) => {
        const separator = attribute.indexOf(":");
        return separator > 0 ? [[attribute.slice(0, separator), attribute.slice(separator + 1)]] : [];
      }));
      return { serial, model: values.model, device: values.device };
    });
}

export async function resolveAndroidPhysicalDevice(input: {
  runner: CommandRunner;
  tools: AndroidSdkTools;
  serial: string;
}): Promise<AndroidPhysicalDevice> {
  const result = await input.runner.run(input.tools.adb, ["devices", "-l"]);
  if (result.exitCode !== 0) {
    throw new Error(result.stderr.trim() || result.stdout.trim() || "Failed to list Android devices.");
  }
  const devices = parseAdbPhysicalDevices(result.stdout);
  const device = devices.find((candidate) => candidate.serial === input.serial);
  if (device) return device;
  throw new Error(
    `Android device ${input.serial} is not connected and authorized. Connected physical devices: ${devices.map((candidate) => candidate.serial).join(", ") || "<none>"}.`
  );
}

export function buildAndroidReverseCommands(input: {
  tools: AndroidSdkTools;
  serial: string;
  ports: readonly number[];
}): AndroidCommand[] {
  return Array.from(new Set(input.ports)).map((port) => ({
    command: input.tools.adb,
    args: ["-s", input.serial, "reverse", `tcp:${port}`, `tcp:${port}`]
  }));
}

export function parseAdbReverseList(stdout: string): AndroidReverseMapping[] {
  return stdout
    .split(/\r?\n/)
    .map((line) => line.trim().split(/\s+/))
    .filter((parts) => parts.length >= 2)
    .map((parts) => ({
      remote: parts.at(-2)!,
      local: parts.at(-1)!
    }));
}

export function androidReverseOwnershipPath(repoRoot: string): string {
  return join(repoRoot, ".build", "kd", "android-reverse-routes.json");
}

export function hasAndroidReverseOwnership(repoRoot: string): boolean {
  return existsSync(androidReverseOwnershipPath(repoRoot));
}

function readAndroidReverseOwnership(repoRoot: string): AndroidReverseOwnership {
  const path = androidReverseOwnershipPath(repoRoot);
  if (!existsSync(path)) return { version: 1, devices: [] };
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as AndroidReverseOwnership;
    if (parsed.version !== 1 || !Array.isArray(parsed.devices)) {
      throw new Error("unsupported format");
    }
    return parsed;
  } catch (error) {
    throw new Error(
      `Could not read Android reverse-route ownership at ${path}: ${error instanceof Error ? error.message : String(error)}`
    );
  }
}

function writeAndroidReverseOwnership(repoRoot: string, ownership: AndroidReverseOwnership): void {
  const path = androidReverseOwnershipPath(repoRoot);
  if (ownership.devices.length === 0) {
    rmSync(path, { force: true });
    return;
  }
  mkdirSync(dirname(path), { recursive: true });
  const temporaryPath = `${path}.${process.pid}.tmp`;
  writeFileSync(temporaryPath, `${JSON.stringify(ownership, null, 2)}\n`, { mode: 0o600 });
  renameSync(temporaryPath, path);
}

function mappingKey(mapping: AndroidReverseMapping): string {
  return `${mapping.remote}\0${mapping.local}`;
}

async function listAndroidReverseMappings(input: {
  runner: CommandRunner;
  tools: AndroidSdkTools;
  serial: string;
}): Promise<AndroidReverseMapping[]> {
  const result = await input.runner.run(input.tools.adb, [
    "-s",
    input.serial,
    "reverse",
    "--list"
  ]);
  if (result.exitCode !== 0) {
    throw new Error(
      result.stderr.trim() || result.stdout.trim() ||
      `Failed to list adb reverse routes for ${input.serial}.`
    );
  }
  return parseAdbReverseList(result.stdout);
}

export async function setupAndroidReverseRoutes(input: {
  repoRoot: string;
  runner: CommandRunner;
  tools: AndroidSdkTools;
  serial: string;
  ports: readonly number[];
}): Promise<AndroidReverseSetupResult> {
  const ownership = readAndroidReverseOwnership(input.repoRoot);
  const priorDevice = ownership.devices.find((device) => device.serial === input.serial);
  const priorOwned = new Set((priorDevice?.mappings ?? []).map(mappingKey));
  const current = await listAndroidReverseMappings(input);
  const currentByRemote = new Map(current.map((mapping) => [mapping.remote, mapping]));
  const desired = Array.from(new Set(input.ports)).map((port) => ({
    remote: `tcp:${port}`,
    local: `tcp:${port}`
  }));
  const created: AndroidReverseMapping[] = [];
  const preexisting: AndroidReverseMapping[] = [];
  const owned: AndroidReverseMapping[] = [];

  for (const mapping of desired) {
    const existing = currentByRemote.get(mapping.remote);
    if (existing && existing.local !== mapping.local) {
      throw new Error(
        `Android device ${input.serial} already has ${mapping.remote} mapped to ${existing.local}; kd will not replace an unverified route.`
      );
    }
  }

  for (const mapping of desired) {
    const existing = currentByRemote.get(mapping.remote);
    if (existing) {
      if (priorOwned.has(mappingKey(mapping))) owned.push(mapping);
      else preexisting.push(mapping);
      continue;
    }

    const result = await input.runner.run(input.tools.adb, [
      "-s",
      input.serial,
      "reverse",
      mapping.remote,
      mapping.local
    ]);
    if (result.exitCode !== 0) {
      const rollbackFailed: AndroidReverseMapping[] = [];
      for (const added of [...created].reverse()) {
        const rollback = await input.runner.run(input.tools.adb, [
          "-s",
          input.serial,
          "reverse",
          "--remove",
          added.remote
        ]);
        if (rollback.exitCode !== 0) rollbackFailed.push(added);
      }
      if (rollbackFailed.length > 0) {
        const devices = ownership.devices.filter((device) => device.serial !== input.serial);
        devices.push({
          serial: input.serial,
          mappings: Array.from(new Map([
            ...(priorDevice?.mappings ?? []),
            ...rollbackFailed
          ].map((ownedMapping) => [mappingKey(ownedMapping), ownedMapping])).values())
        });
        writeAndroidReverseOwnership(input.repoRoot, { version: 1, devices });
      }
      throw new Error(
        result.stderr.trim() || result.stdout.trim() ||
        `Failed to configure ${mapping.remote} on Android device ${input.serial}.`
      );
    }
    created.push(mapping);
    owned.push(mapping);
  }

  const mergedOwned = Array.from(new Map([
    ...(priorDevice?.mappings ?? []),
    ...owned
  ].map((mapping) => [mappingKey(mapping), mapping])).values());
  const devices = ownership.devices.filter((device) => device.serial !== input.serial);
  if (mergedOwned.length > 0) devices.push({ serial: input.serial, mappings: mergedOwned });
  writeAndroidReverseOwnership(input.repoRoot, { version: 1, devices });
  return { created, preexisting, owned: mergedOwned };
}

export async function cleanupOwnedAndroidReverseRoutes(input: {
  repoRoot: string;
  runner: CommandRunner;
  tools: AndroidSdkTools;
}): Promise<AndroidReverseCleanupResult> {
  const ownership = readAndroidReverseOwnership(input.repoRoot);
  const result: AndroidReverseCleanupResult = { cleaned: [], skipped: [], failed: [] };
  const remaining: AndroidReverseOwnership["devices"] = [];

  for (const device of ownership.devices) {
    let current: AndroidReverseMapping[];
    try {
      current = await listAndroidReverseMappings({ ...input, serial: device.serial });
    } catch {
      result.failed.push(...device.mappings);
      remaining.push(device);
      continue;
    }
    const currentByRemote = new Map(current.map((mapping) => [mapping.remote, mapping]));
    const deviceRemaining: AndroidReverseMapping[] = [];
    for (const mapping of device.mappings) {
      const existing = currentByRemote.get(mapping.remote);
      if (!existing || existing.local !== mapping.local) {
        result.skipped.push(mapping);
        continue;
      }
      const remove = await input.runner.run(input.tools.adb, [
        "-s",
        device.serial,
        "reverse",
        "--remove",
        mapping.remote
      ]);
      if (remove.exitCode === 0) result.cleaned.push(mapping);
      else {
        result.failed.push(mapping);
        deviceRemaining.push(mapping);
      }
    }
    if (deviceRemaining.length > 0) {
      remaining.push({ serial: device.serial, mappings: deviceRemaining });
    }
  }
  writeAndroidReverseOwnership(input.repoRoot, { version: 1, devices: remaining });
  return result;
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

export interface AndroidPhysicalRunPlan {
  build: AndroidCommand;
  install: AndroidCommand;
  stop: AndroidCommand;
  launch: AndroidCommand;
  apkPath: string;
  payloadUrl: string;
}

export interface AndroidPhysicalRunResult {
  ok: boolean;
  step: "build" | "install" | "stop" | "launch";
  result: CommandResult;
}

export function buildAndroidPhysicalRunPlan(input: {
  repoRoot: string;
  serial: string;
  packageId: string;
  metroPort: number;
  appEnv: string;
  tools: AndroidSdkTools;
  packagerHost?: string;
}): AndroidPhysicalRunPlan {
  const androidRoot = join(input.repoRoot, "apps", "mobile", "android");
  const apkPath = join(androidRoot, "app", "build", "outputs", "apk", "debug", "app-debug.apk");
  const packagerHost = input.packagerHost ?? "127.0.0.1";
  const payloadUrl = `exp+kanna-mobile://expo-development-client/?url=${encodeURIComponent(`http://${packagerHost}:${input.metroPort}`)}`;
  const env = {
    KANNA_APP_ENV: input.appEnv,
    ANDROID_HOME: input.tools.root,
    ANDROID_SDK_ROOT: input.tools.root,
    REACT_NATIVE_PACKAGER_HOSTNAME: packagerHost,
    RCT_METRO_PORT: String(input.metroPort)
  };
  return {
    build: {
      command: join(androidRoot, "gradlew"),
      args: [
        "app:assembleDebug",
        "-x",
        "lint",
        "-x",
        "test",
        "--configure-on-demand",
        "--build-cache",
        `-PreactNativeDevServerPort=${input.metroPort}`
      ],
      cwd: androidRoot,
      env
    },
    install: {
      command: input.tools.adb,
      args: ["-s", input.serial, "install", "-r", apkPath]
    },
    stop: {
      command: input.tools.adb,
      args: ["-s", input.serial, "shell", "am", "force-stop", input.packageId]
    },
    launch: {
      command: input.tools.adb,
      args: [
        "-s",
        input.serial,
        "shell",
        "am",
        "start",
        "-W",
        "-a",
        "android.intent.action.VIEW",
        "-d",
        payloadUrl,
        "-p",
        input.packageId
      ]
    },
    apkPath,
    payloadUrl
  };
}

export async function executeAndroidPhysicalRunPlan(input: {
  runner: CommandRunner;
  plan: AndroidPhysicalRunPlan;
  env?: NodeJS.ProcessEnv;
}): Promise<AndroidPhysicalRunResult> {
  let last: AndroidPhysicalRunResult | undefined;
  for (const [step, command] of [
    ["build", input.plan.build],
    ["install", input.plan.install],
    ["stop", input.plan.stop],
    ["launch", input.plan.launch]
  ] as const) {
    const result = await input.runner.run(command.command, command.args, {
      cwd: command.cwd,
      env: { ...input.env, ...command.env },
      streamOutput: step === "build"
    });
    last = { ok: result.exitCode === 0, step, result };
    if (!last.ok) return last;
  }
  return last!;
}

export function buildAndroidRunCommand(input: {
  repoRoot: string;
  deviceName: string;
  packageId: string;
  metroPort: number;
  appEnv: string;
  tools: AndroidSdkTools;
  packagerHost?: string;
  deviceSerial?: string;
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
      ...(input.deviceSerial ? { ANDROID_SERIAL: input.deviceSerial } : {}),
      REACT_NATIVE_PACKAGER_HOSTNAME: input.packagerHost ?? "10.0.2.2",
      RCT_METRO_PORT: String(input.metroPort)
    }
  };
}
