/**
 * Driving an *installed* Kanna: the package's own binaries, under the user's
 * real `systemd --user` manager, upgraded the way apt upgrades it.
 *
 * The headless-worker lane already proves the worker/daemon/server contract
 * with binaries from `.build/`, spawned as a child of the test process. None of
 * what this lane exists for is reachable from there. An installed run differs
 * in exactly the ways that matter to an upgrade:
 *
 * * The supervisor's parent is the user manager's `ExecStart`, not vitest, so
 *   the daemon's launcher trust root is the installed executable at its
 *   installed path.
 * * That path's *bytes* are replaced underneath a live daemon when apt unpacks
 *   the new package, which is precisely the state Phase 1's byte-exact
 *   identity rule was written against — and the reason it deferred the
 *   installed-upgrade proof to this phase.
 * * The restart that applies the new version is `systemctl --user restart`,
 *   which stops only the supervisor (`KillMode=process`) and leaves the daemon
 *   and every live agent session running to be re-adopted.
 *
 * So the lane runs against `/usr/lib/kanna`, and its isolation comes from an
 * explicit data directory, database and ports rather than from spawning
 * something private.
 */

import { spawn } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { localProcessFetch } from "@kanna/local-process-fetch";
import { installedPaths, type InstalledChannel, type InstalledPaths } from "./installedTree.ts";

export type Run = { code: number | null; stdout: string; stderr: string };

export async function run(
  command: string,
  args: string[],
  env: NodeJS.ProcessEnv = process.env,
  cwd?: string
): Promise<Run> {
  return new Promise((resolveRun, reject) => {
    const child = spawn(command, args, { env, cwd, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += String(chunk)));
    child.stderr.on("data", (chunk) => (stderr += String(chunk)));
    child.on("error", reject);
    child.on("close", (code) => resolveRun({ code, stdout, stderr }));
  });
}

export async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (typeof address === "string" || address === null) {
        reject(new Error("could not allocate a port"));
        return;
      }
      const { port } = address;
      server.close(() => resolvePort(port));
    });
  });
}

/**
 * Whether this host can run the lane, and why not when it cannot.
 *
 * Reported rather than thrown so a caller can *skip with a reason*. A lane that
 * silently passed because it never installed anything would be worse than one
 * that did not run, which is the failure the release gate has to distinguish.
 */
export interface HostCapability {
  usable: boolean;
  reason: string;
  canBecomeRoot: boolean;
  hasUserManager: boolean;
  distribution: string;
  kernel: string;
  glibc: string;
  architecture: string;
  /** Developer tooling present on this host. Not a failure — a caveat that a
   *  pass here is not by itself a clean-machine proof. */
  developerToolsOnPath: string[];
}

export async function inspectHost(developerTools: readonly string[]): Promise<HostCapability> {
  if (process.platform !== "linux") {
    return {
      usable: false,
      reason: "not Linux",
      canBecomeRoot: false,
      hasUserManager: false,
      distribution: "",
      kernel: "",
      glibc: "",
      architecture: process.arch,
      developerToolsOnPath: [],
    };
  }

  const root = process.getuid?.() === 0 || (await run("sudo", ["-n", "true"])).code === 0;
  const userManager = (await run("systemctl", ["--user", "is-system-running"])).code !== null;
  const osRelease = await readFile("/etc/os-release", "utf8").catch(() => "");
  const distribution = /^PRETTY_NAME="?([^"\n]+)"?/m.exec(osRelease)?.[1] ?? "unknown";
  const kernel = (await run("uname", ["-r"])).stdout.trim();
  const glibc = /(\d+\.\d+)/.exec((await run("getconf", ["GNU_LIBC_VERSION"])).stdout)?.[1] ?? "unknown";
  const architecture = (await run("dpkg", ["--print-architecture"])).stdout.trim();

  const present: string[] = [];
  for (const tool of developerTools) {
    if ((await run("sh", ["-c", `command -v ${tool}`])).code === 0) present.push(tool);
  }

  const problems = [
    root ? null : "the test user cannot become root, so no package can be installed",
    userManager ? null : "there is no `systemd --user` manager to run the worker unit",
  ].filter((value): value is string => value !== null);

  return {
    usable: problems.length === 0,
    reason: problems.join("; "),
    canBecomeRoot: root,
    hasUserManager: userManager,
    distribution,
    kernel,
    glibc,
    architecture,
    developerToolsOnPath: present,
  };
}

/** `sudo` only where it is needed, and never interactively: a lane that could
 *  block on a password prompt would hang a CI job instead of failing it. */
function asRoot(command: string, args: string[]): [string, string[]] {
  return process.getuid?.() === 0 ? [command, args] : ["sudo", ["-n", command, ...args]];
}

export async function installPackage(debPath: string): Promise<Run> {
  // `apt-get install ./file.deb` rather than `dpkg -i`, because resolving the
  // declared `Depends` is part of what the package is being tested for.
  const [command, args] = asRoot("apt-get", [
    "install",
    "-y",
    "--allow-downgrades",
    "-o",
    "Dpkg::Options::=--force-confnew",
    debPath,
  ]);
  return run(command, args, { ...process.env, DEBIAN_FRONTEND: "noninteractive" });
}

export async function removePackage(packageName: string): Promise<Run> {
  const [command, args] = asRoot("apt-get", ["remove", "-y", packageName]);
  return run(command, args, { ...process.env, DEBIAN_FRONTEND: "noninteractive" });
}

export async function installedPackageVersion(packageName: string): Promise<string | null> {
  const result = await run("dpkg-query", ["-W", "-f=${Version}", packageName]);
  return result.code === 0 && result.stdout.trim().length > 0 ? result.stdout.trim() : null;
}

export interface InstalledWorkerOptions {
  channel: InstalledChannel;
  /** Provider CLIs the scripted-agent fixture installed. */
  providerBinDir: string;
  /** Unit name for this run. Distinct from the package's own so a lane can
   *  never stop the operator's real worker. */
  unitName?: string;
}

/**
 * A worker running from the installed package, under the user manager.
 */
export class InstalledWorker {
  private constructor(
    readonly paths: InstalledPaths,
    readonly unitName: string,
    readonly unitPath: string,
    readonly root: string,
    readonly dataDir: string,
    readonly dbPath: string,
    readonly lanPort: number,
    readonly baseUrl: string,
    readonly env: NodeJS.ProcessEnv
  ) {}

  static async start(options: InstalledWorkerOptions): Promise<InstalledWorker> {
    const paths = installedPaths(options.channel);
    const unitName = options.unitName ?? `kanna-installed-test-${process.pid}.service`;
    const root = await mkdtemp(join(tmpdir(), "kanna-installed-"));
    const dataDir = join(root, "worker");
    const dbPath = join(root, "kanna-installed.db");
    const lanPort = await freePort();
    const transferPort = await freePort();
    const unitPath = join(
      process.env.XDG_CONFIG_HOME ?? join(process.env.HOME ?? "/root", ".config"),
      "systemd",
      "user",
      unitName
    );
    await mkdir(join(root, "xdg-data"), { recursive: true });

    // Same rule as the headless lane: every inherited `KANNA_*` variable goes.
    // A lane run from inside a Kanna task would otherwise carry that task's own
    // identity and completion context into the instance it is testing.
    const env: NodeJS.ProcessEnv = Object.fromEntries(
      Object.entries(process.env).filter(([key]) => !key.startsWith("KANNA_"))
    );
    env.PATH = `${options.providerBinDir}:${env.PATH ?? ""}`;

    // The unit is written by the installed `kanna-worker` itself, so the lane
    // exercises the same generator an operator runs rather than a copy of it.
    const written = await run(
      paths.executable("kanna-worker"),
      [
        "install-unit",
        "--unit-path", unitPath,
        "--data-dir", dataDir,
        "--db-path", dbPath,
        "--lan-port", String(lanPort),
        "--transfer-port", String(transferPort),
      ],
      env
    );
    if (written.code !== 0) {
      throw new Error(`install-unit failed: ${written.stderr || written.stdout}`);
    }

    const worker = new InstalledWorker(
      paths, unitName, unitPath, root, dataDir, dbPath, lanPort,
      `http://127.0.0.1:${lanPort}`, env
    );
    await worker.systemctl(["daemon-reload"]);
    await worker.systemctl(["start", unitName]);
    await waitFor(
      async () => (await worker.status()) !== null,
      async () => `the installed worker never served ${worker.baseUrl}\n${await worker.journal()}`
    );
    return worker;
  }

  async systemctl(args: string[]): Promise<Run> {
    return run("systemctl", ["--user", ...args], this.env);
  }

  /** The restart an operator performs after an upgrade. It stops only the
   *  supervisor; the daemon and its sessions survive to be re-adopted. */
  async restartUnit(): Promise<void> {
    const restarted = await this.systemctl(["restart", this.unitName]);
    if (restarted.code !== 0) {
      throw new Error(`restart failed: ${restarted.stderr}\n${await this.journal()}`);
    }
    await waitFor(
      async () => (await this.status()) !== null,
      async () => `the restarted worker never served ${this.baseUrl}\n${await this.journal()}`
    );
  }

  async journal(): Promise<string> {
    return (await run("journalctl", ["--user", "-u", this.unitName, "-n", "200", "--no-pager"], this.env)).stdout;
  }

  async status(): Promise<Record<string, unknown> | null> {
    try {
      const response = await localProcessFetch(`${this.baseUrl}/v1/status`);
      if (!response.ok) return null;
      return (await response.json()) as Record<string, unknown>;
    } catch {
      return null;
    }
  }

  /** The supervisor's pid, as the user manager reports it — not something the
   *  lane guessed from a process listing. */
  async supervisorPid(): Promise<number> {
    const shown = await this.systemctl(["show", this.unitName, "-p", "MainPID", "--value"]);
    return Number.parseInt(shown.stdout.trim(), 10);
  }

  async daemonPid(): Promise<number> {
    return Number.parseInt((await readFile(join(this.dataDir, "daemon.pid"), "utf8")).trim(), 10);
  }

  async api(path: string, init?: RequestInit): Promise<Response> {
    return localProcessFetch(`${this.baseUrl}${path}`, init);
  }

  async json<T>(path: string, init?: RequestInit): Promise<T> {
    const response = await this.api(path, init);
    if (!response.ok) {
      throw new Error(`${path} failed (${response.status}): ${await response.text()}`);
    }
    return (await response.json()) as T;
  }

  async cli(args: string[]): Promise<Run> {
    return run(this.paths.executable("kanna-cli"), args, {
      ...this.env,
      KANNA_SERVER_BASE_URL: this.baseUrl,
    });
  }

  /** Stop the instance and take the unit away again, so a lane leaves the
   *  machine as it found it even when it failed part-way. */
  async stop(): Promise<void> {
    await this.systemctl(["stop", this.unitName]).catch(() => undefined);
    await run(this.paths.executable("kanna-worker"), ["stop-daemon", "--data-dir", this.dataDir], this.env).catch(
      () => undefined
    );
    await rm(this.unitPath, { force: true }).catch(() => undefined);
    await this.systemctl(["daemon-reload"]).catch(() => undefined);
    await rm(this.root, { recursive: true, force: true }).catch(() => undefined);
  }
}

/**
 * A process's start time from `/proc`, in clock ticks since boot.
 *
 * A pid alone cannot answer "is this the same process": pids are reused, and
 * an upgrade proof asserting only that a number is unchanged would pass
 * against a *different* agent that happened to inherit it. The pair is the
 * identity, which is the same pairing the daemon's own authorizer uses.
 */
export async function processStartTime(pid: number): Promise<string | null> {
  const stat = await readFile(`/proc/${pid}/stat`, "utf8").catch(() => null);
  if (stat === null) return null;
  // Field 22, counting from 1, after the comm field — which may itself contain
  // spaces and parentheses, so the split starts after the last `)`.
  const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
  return fields[19] ?? null;
}

export function processIsAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export async function waitFor(
  condition: () => Promise<boolean>,
  message: string | (() => string | Promise<string>),
  timeoutMs = 120_000
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await condition()) return;
    await sleep(250);
  }
  throw new Error(`timed out: ${typeof message === "string" ? message : await message()}`);
}
