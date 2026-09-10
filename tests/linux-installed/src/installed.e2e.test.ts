import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createFixtureRepo, type FixtureRepo } from "@kanna/headless-worker/src/fixtureRepo.ts";
import {
  DEVELOPER_TOOLS,
  INSTALLED_EXECUTABLES,
  inspectInstalledTree,
  installedPaths,
} from "./installedTree.ts";
import {
  InstalledWorker,
  inspectHost,
  installPackage,
  installedPackageVersion,
  run,
  waitFor,
  type HostCapability,
} from "./installedWorker.ts";

/**
 * The installed package, on a machine, doing the job.
 *
 * Everything here is only true after `dpkg` has unpacked the artifact: the
 * build lane proves the tree it *staged* is complete, and cannot prove the
 * launcher symlink survived, that a user's `systemd --user` manager can start
 * the worker from its installed path, or that the desktop finds the built-in
 * definitions the package placed. Those are what a clean-machine install is.
 *
 * The package under test comes from `KANNA_INSTALLED_DEB`. There is no default
 * and no build fallback on purpose — a lane that built its own package would
 * be testing this checkout rather than a release artifact.
 */

const DEB = process.env.KANNA_INSTALLED_DEB;
const CHANNEL = (process.env.KANNA_INSTALLED_CHANNEL ?? "production") as "production" | "staging";

let host: HostCapability;
let repo: FixtureRepo;
let worker: InstalledWorker | null = null;
const paths = installedPaths(CHANNEL);

beforeAll(async () => {
  host = await inspectHost(DEVELOPER_TOOLS);
}, 120_000);

afterAll(async () => {
  await worker?.stop();
});

/**
 * A missing package or an unusable host must be visible as a *skip with a
 * reason*, never a pass. A release gate that cannot tell "the upgrade works"
 * from "nothing was installed" is not a gate.
 */
function requireHost(): void {
  if (!DEB) throw new Error("KANNA_INSTALLED_DEB is not set: this lane installs a built package.");
  if (!host.usable) throw new Error(`this host cannot run the installed lane: ${host.reason}`);
}

describe("installing the package", () => {
  it("records what host the evidence came from", async () => {
    requireHost();
    // Printed rather than asserted: the supported floor is a release-policy
    // decision, and a lane that silently accepted any host would let evidence
    // from the wrong distribution read as acceptance for the right one.
    // eslint-disable-next-line no-console
    console.log(
      `[installed-lane] ${host.distribution} kernel ${host.kernel} glibc ${host.glibc} ` +
        `${host.architecture}; developer tools on PATH: ${host.developerToolsOnPath.join(", ") || "none"}`
    );
    expect(host.architecture).toMatch(/^(amd64|arm64)$/);
  });

  it("installs with apt resolving its declared dependencies", async () => {
    requireHost();
    const installed = await installPackage(DEB as string);
    expect(installed.code, `${installed.stdout}\n${installed.stderr}`).toBe(0);
    expect(await installedPackageVersion(paths.packageName)).toBeTruthy();
  });

  /**
   * The whole installed layout, checked at once. A partial install diagnosed
   * one file at a time takes as many CI runs as it has missing files.
   */
  it("lays the whole tree down where the runtime looks for it", () => {
    requireHost();
    expect(inspectInstalledTree(paths)).toEqual([]);
  });

  /**
   * The claim the vendoring rule actually makes. Not "these tools are absent" —
   * an acceptance host may well have them — but that Kanna's own executables
   * start with none of them reachable, which is the state a user's machine is
   * in.
   */
  it("runs its own binaries with no developer tooling on PATH", async () => {
    requireHost();
    const bare = { ...process.env, PATH: "/usr/bin:/bin", HOME: process.env.HOME };
    for (const name of ["kanna-cli", "kanna-worker", "kanna-daemon"]) {
      // `--help`, not `--version`. All three answer it and exit 0: `kanna-cli`
      // through clap, `kanna-worker` and `kanna-daemon` through explicit arms.
      // `--version` would be wrong for the probe *and* wrong as a loose
      // assertion — `kanna-cli` declares no clap `version` attribute, so clap
      // rejects the flag as an unexpected argument and exits 2, which this
      // check would have reported as a launch failure on the first real run.
      const help = await run(paths.executable(name), ["--help"], bare);
      expect(help.code, `${name} --help: ${help.stderr || help.stdout}`).toBe(0);
      // The point of the probe: a binary that starts at all with a bare PATH
      // has its runtime dependencies satisfied by the package, not by
      // developer tooling that happens to be on this host.
      expect(`${help.stdout}${help.stderr}`).not.toMatch(
        /error while loading shared libraries|No such file or directory/
      );
    }
  });

  it("ships the built-in definitions the desktop reads at runtime", () => {
    requireHost();
    for (const section of ["agents", "workflows", "tasks"]) {
      expect(existsSync(join(paths.resources, section))).toBe(true);
    }
    // `specialty-review` is internal and must resolve by name; its presence is
    // how an installed dispatcher can give a child task its workflow at all.
    expect(existsSync(join(paths.resources, "workflows", "specialty-review.json"))).toBe(true);
  });

  it("registers a desktop entry the shell can match to the running window", () => {
    requireHost();
    const entry = readFileSync(paths.desktopEntry, "utf8");
    expect(entry).toContain(`Exec=/usr/bin/${paths.packageName}`);
    expect(entry).toContain(`StartupWMClass=${paths.desktopEntryId}`);
  });
});

describe("the installed worker's lifecycle", () => {
  it("starts under the user manager from its installed path", async () => {
    requireHost();
    repo = await createFixtureRepo();
    worker = await InstalledWorker.start({ channel: CHANNEL, providerBinDir: repo.providerBinDir });
    expect((await worker.status())?.state).toBe("running");

    // The supervisor's executable is the installed one. That path is the
    // daemon's launcher trust root, so an installed run proving anything about
    // handoff depends on it being this file and not a build output.
    const supervisor = await worker.supervisorPid();
    expect(supervisor).toBeGreaterThan(0);
    const exe = await run("readlink", ["-f", `/proc/${supervisor}/exe`]);
    expect(exe.stdout.trim()).toBe(paths.executable("kanna-worker"));
  });

  it("spawns the daemon and the server as its own children", async () => {
    requireHost();
    const supervisor = await worker!.supervisorPid();
    const daemon = await worker!.daemonPid();
    const parent = await run("ps", ["-o", "ppid=", "-p", String(daemon)]);
    expect(Number.parseInt(parent.stdout.trim(), 10)).toBe(supervisor);
  });

  it("runs a real agent task through the installed binaries", async () => {
    requireHost();
    const added = await worker!.cli(["repo", "add", "--path", repo.path]);
    expect(added.code, `${added.stdout}${added.stderr}`).toBe(0);
    const addedRepo = JSON.parse(added.stdout);
    expect(addedRepo.path).toBe(repo.path);
    expect(addedRepo.id).toEqual(expect.any(String));
    expect(addedRepo.id.length).toBeGreaterThan(0);
    const repoId = addedRepo.id;

    const created = await worker!.cli([
      "task", "create", "--repo-id", repoId, "--prompt", "installed lane task", "--workflow-name", "gate",
    ]);
    expect(created.code, created.stderr).toBe(0);
    const createdTask = JSON.parse(created.stdout);
    expect(createdTask.repoId).toBe(repoId);
    expect(createdTask.taskId).toMatch(/^[a-f0-9]{8,64}$/);
    const taskId = createdTask.taskId;

    await waitFor(
      async () => (await worker!.cli(["task", "logs", "--task-id", taskId])).stdout.includes("SCRIPT_READY"),
      "the scripted agent never announced itself under the installed worker"
    );
  });

  it("installs no unit and enables no linger of its own accord", async () => {
    requireHost();
    // The package must never put a competing canonical server on the machine.
    // The lane's own unit is the one it wrote itself, under a test name.
    const packageUnit = await run("systemctl", ["--user", "cat", paths.workerUnitName]);
    expect(packageUnit.code).not.toBe(0);
  });
});

describe("removal", () => {
  it("removes the package's own files and nothing else", async () => {
    requireHost();

    /**
     * A file under the running instance's data directory, written *before* the
     * removal. This is the assertion that matters: `dpkg` must take away
     * exactly the paths the package owns, and a maintainer script that reached
     * into user state would destroy tasks nobody asked about.
     *
     * It is written before `worker.stop()` because stop tears the instance's
     * directory down, so the marker has to outlive only the removal.
     */
    const dataDir = worker?.dataDir as string;
    const marker = join(dataDir, "user-data-marker");
    mkdirSync(dataDir, { recursive: true });
    writeFileSync(marker, "a task the operator cares about");

    await worker?.systemctl(["stop", worker.unitName]).catch(() => undefined);
    const removed = await run(
      "sh",
      ["-c", `sudo -n apt-get remove -y ${paths.packageName} || apt-get remove -y ${paths.packageName}`],
      { ...process.env, DEBIAN_FRONTEND: "noninteractive" }
    );
    expect(removed.code, removed.stderr).toBe(0);

    // Every package-owned executable is gone...
    for (const name of INSTALLED_EXECUTABLES) {
      expect(existsSync(paths.executable(name)), `${name} survived removal`).toBe(false);
    }
    // ...and the instance's own state is untouched.
    expect(existsSync(marker), "removing the package deleted user data").toBe(true);
    expect(readFileSync(marker, "utf8")).toBe("a task the operator cares about");

    await worker?.stop();
    worker = null;
  });
});
