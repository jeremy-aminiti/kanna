/**
 * What an installed Kanna must look like on disk, checked against a real
 * installation rather than against the build that produced it.
 *
 * This is the assertion that no build-machine test can make. `kd build
 * linux-package` proves the *tree it staged* is complete; it cannot prove that
 * `dpkg` unpacked it, that the launcher symlink survived, that the file modes
 * came through, or that the desktop finds its resources from where the package
 * actually put them. Those only become facts after an install.
 */

import { existsSync, readlinkSync, statSync } from "node:fs";
import { join } from "node:path";

export type InstalledChannel = "production" | "staging";

export const INSTALLED_EXECUTABLES = [
  "kanna-desktop",
  "kanna-worker",
  "kanna-daemon",
  "kanna-cli",
  "kanna-mcp",
  "kanna-server",
  "kanna-task-transfer",
  "kanna-terminal-recovery",
] as const;

export interface InstalledPaths {
  packageName: string;
  desktopEntryId: string;
  workerUnitName: string;
  libDir: string;
  launcher: string;
  desktopEntry: string;
  icons: string[];
  resources: string;
  executable: (name: string) => string;
}

export function installedPaths(channel: InstalledChannel, prefix = "/usr"): InstalledPaths {
  const packageName = channel === "staging" ? "kanna-staging" : "kanna";
  const desktopEntryId = channel === "staging" ? "build.kanna.staging" : "build.kanna";
  const libDir = join(prefix, "lib", packageName);
  return {
    packageName,
    desktopEntryId,
    workerUnitName: channel === "staging" ? "kanna-staging-worker.service" : "kanna-worker.service",
    libDir,
    launcher: join(prefix, "bin", packageName),
    desktopEntry: join(prefix, "share", "applications", `${desktopEntryId}.desktop`),
    icons: [32, 64, 128].map((size) =>
      join(prefix, "share", "icons", "hicolor", `${size}x${size}`, "apps", `${desktopEntryId}.png`)
    ),
    resources: join(libDir, ".kanna"),
    executable: (name: string) => join(libDir, name),
  };
}

export interface TreeProblem {
  path: string;
  problem: string;
}

/** Every way an installed tree can be wrong, reported together rather than
 *  one failure at a time — a partial install is more useful diagnosed whole. */
export function inspectInstalledTree(paths: InstalledPaths): TreeProblem[] {
  const problems: TreeProblem[] = [];

  for (const name of INSTALLED_EXECUTABLES) {
    const path = paths.executable(name);
    if (!existsSync(path)) {
      problems.push({ path, problem: "missing" });
      continue;
    }
    if ((statSync(path).mode & 0o111) === 0) {
      problems.push({ path, problem: "installed without an executable bit" });
    }
  }

  // A copy here instead of a symlink would put `current_exe()` in `/usr/bin`,
  // where no sidecar lives, and every task spawn would fail on a user's
  // machine while passing on every builder.
  if (!existsSync(paths.launcher)) {
    problems.push({ path: paths.launcher, problem: "missing" });
  } else {
    let target: string | null = null;
    try {
      target = readlinkSync(paths.launcher);
    } catch {
      target = null;
    }
    if (target === null) {
      problems.push({ path: paths.launcher, problem: "is a copy, not a symlink into the library directory" });
    } else if (!target.endsWith(`/${paths.packageName}/kanna-desktop`)) {
      problems.push({ path: paths.launcher, problem: `points at ${target}` });
    }
  }

  for (const section of ["agents", "workflows", "tasks"]) {
    const path = join(paths.resources, section);
    if (!existsSync(path)) problems.push({ path, problem: "built-in resources missing" });
  }

  if (!existsSync(paths.desktopEntry)) problems.push({ path: paths.desktopEntry, problem: "missing" });
  for (const icon of paths.icons) {
    if (!existsSync(icon)) problems.push({ path: icon, problem: "missing" });
  }

  return problems;
}

/**
 * The clean-machine claim, stated as a check rather than a promise.
 *
 * Kanna's own launch must need none of these. It is deliberately not a check
 * that they are *absent* — the acceptance host may well have them — but a
 * record of which were on PATH when the run happened, so a passing result on a
 * developer-tooled host is not mistaken for a clean-machine proof.
 */
export const DEVELOPER_TOOLS = ["cargo", "rustc", "node", "pnpm", "npm", "cc", "gcc", "zig", "bazel"] as const;
