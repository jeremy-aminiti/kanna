import { chmodSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { INSTALLED_EXECUTABLES, inspectInstalledTree, installedPaths } from "./installedTree.ts";

/**
 * The tree inspector's own rules, on synthetic trees. Runs anywhere, so a
 * change to what "correctly installed" means fails on a developer's machine
 * rather than only inside a CI job that needs a package.
 */

const temporaries: string[] = [];
afterEach(() => {
  while (temporaries.length > 0) rmSync(temporaries.pop() as string, { recursive: true, force: true });
});

function completeTree(): string {
  const prefix = mkdtempSync(join(tmpdir(), "kanna-installed-tree-"));
  temporaries.push(prefix);
  const paths = installedPaths("production", prefix);
  mkdirSync(paths.libDir, { recursive: true });
  for (const name of INSTALLED_EXECUTABLES) writeFileSync(paths.executable(name), "x", { mode: 0o755 });
  mkdirSync(join(prefix, "bin"), { recursive: true });
  symlinkSync("../lib/kanna/kanna-desktop", paths.launcher);
  for (const section of ["agents", "workflows", "tasks"]) {
    mkdirSync(join(paths.resources, section), { recursive: true });
  }
  mkdirSync(join(prefix, "share", "applications"), { recursive: true });
  writeFileSync(paths.desktopEntry, "[Desktop Entry]");
  for (const icon of paths.icons) {
    mkdirSync(join(icon, ".."), { recursive: true });
    writeFileSync(icon, "png");
  }
  return prefix;
}

describe("inspectInstalledTree", () => {
  it("accepts a complete installation", () => {
    expect(inspectInstalledTree(installedPaths("production", completeTree()))).toEqual([]);
  });

  it("reports a missing executable", () => {
    const prefix = completeTree();
    const paths = installedPaths("production", prefix);
    rmSync(paths.executable("kanna-worker"));
    expect(inspectInstalledTree(paths)).toEqual([
      { path: paths.executable("kanna-worker"), problem: "missing" },
    ]);
  });

  it("reports an executable installed without its executable bit", () => {
    const prefix = completeTree();
    const paths = installedPaths("production", prefix);
    chmodSync(paths.executable("kanna-daemon"), 0o644);
    expect(inspectInstalledTree(paths)[0]?.problem).toMatch(/executable bit/);
  });

  /**
   * The failure this check exists for: a copy resolves `current_exe()` into
   * `/usr/bin`, where no sidecar lives, so every task spawn fails on a user's
   * machine and passes on every build machine.
   */
  it("reports a launcher installed as a copy instead of a symlink", () => {
    const prefix = completeTree();
    const paths = installedPaths("production", prefix);
    rmSync(paths.launcher);
    writeFileSync(paths.launcher, "x", { mode: 0o755 });
    expect(inspectInstalledTree(paths)[0]?.problem).toMatch(/copy, not a symlink/);
  });

  it("reports missing built-in definitions and icons together", () => {
    const prefix = completeTree();
    const paths = installedPaths("production", prefix);
    rmSync(join(paths.resources, "workflows"), { recursive: true });
    rmSync(paths.icons[0] as string);
    const problems = inspectInstalledTree(paths);
    expect(problems.map((problem) => problem.problem)).toEqual(["built-in resources missing", "missing"]);
  });

  it("keeps the channels' trees separate", () => {
    const production = installedPaths("production", "/usr");
    const staging = installedPaths("staging", "/usr");
    expect(production.libDir).not.toBe(staging.libDir);
    expect(production.workerUnitName).not.toBe(staging.workerUnitName);
  });
});
