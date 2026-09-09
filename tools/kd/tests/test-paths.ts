import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll } from "vitest";

const ROOT_PREFIX = "kanna-kd-tests-";

function processIsAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

/** Remove abandoned roots from test processes that were interrupted before finally blocks ran. */
export function sweepKdTestScratchRoots(
  temporaryDirectory = tmpdir(),
  isAlive: (pid: number) => boolean = processIsAlive
): string[] {
  if (!existsSync(temporaryDirectory)) return [];
  const removed: string[] = [];
  for (const entry of readdirSync(temporaryDirectory)) {
    const match = new RegExp(`^${ROOT_PREFIX}(\\d+)$`).exec(entry);
    if (!match) continue;
    const path = join(temporaryDirectory, entry);
    // Another worker's `afterAll` may remove its root between the listing
    // and this stat; a root that is already gone needs nothing from us.
    if (!lstatSync(path, { throwIfNoEntry: false })?.isDirectory()) continue;
    const pid = Number(match[1]);
    if (Number.isSafeInteger(pid) && isAlive(pid)) continue;
    rmSync(path, { recursive: true, force: true });
    removed.push(path);
  }
  return removed;
}

const processRoot = join(tmpdir(), `${ROOT_PREFIX}${process.pid}`);
sweepKdTestScratchRoots();
// Normal completion leaves no root behind. Vitest tears its fork workers down
// without firing `exit`, so the file's `afterAll` is what removes the root in
// a suite; the exit handler covers a helper run outside one. SIGKILL and
// machine crashes run neither, and the startup sweep above covers those.
afterAll(() => {
  rmSync(processRoot, { recursive: true, force: true });
});
process.once("exit", () => {
  rmSync(processRoot, { recursive: true, force: true });
});

/**
 * Prefix for test scratch directories. Keeping every expensive fixture below
 * one PID-owned root means a later test run can reclaim it after an interrupted
 * run, instead of stranding large trees directly in /private/tmp.
 */
export function kdTestScratchPrefix(name: string): string {
  // Created on demand: an earlier file's `afterAll` may have removed it.
  mkdirSync(processRoot, { recursive: true });
  return join(processRoot, name);
}

/**
 * `mkdtemp` under this process's root: a fresh directory whose lifetime is the
 * root's, so a test that never removes it still leaves nothing behind. Use it
 * in place of `mkdtemp(join(tmpdir(), name))`, which strands the directory in
 * the shared temp root — a leak the Mac Studio measured at 73k directories.
 */
export function kdTestScratchDir(name: string): Promise<string> {
  return mkdtemp(kdTestScratchPrefix(name));
}

/** [`kdTestScratchDir`] for a synchronous fixture. */
export function kdTestScratchDirSync(name: string): string {
  return mkdtempSync(kdTestScratchPrefix(name));
}
