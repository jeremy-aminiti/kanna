import { existsSync, lstatSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

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
    if (!lstatSync(path).isDirectory()) continue;
    const pid = Number(match[1]);
    if (Number.isSafeInteger(pid) && isAlive(pid)) continue;
    rmSync(path, { recursive: true, force: true });
    removed.push(path);
  }
  return removed;
}

const processRoot = join(tmpdir(), `${ROOT_PREFIX}${process.pid}`);
sweepKdTestScratchRoots();
mkdirSync(processRoot, { recursive: true });
// Normal test completion leaves no root behind. SIGKILL and machine crashes
// cannot run this handler; the startup sweep above covers those cases.
process.once("exit", () => {
  rmSync(processRoot, { recursive: true, force: true });
});

/**
 * Prefix for test scratch directories. Keeping every expensive fixture below
 * one PID-owned root means a later test run can reclaim it after an interrupted
 * run, instead of stranding large trees directly in /private/tmp.
 */
export function kdTestScratchPrefix(name: string): string {
  return join(processRoot, name);
}
