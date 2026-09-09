import { existsSync, mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { kdTestScratchDir, kdTestScratchDirSync, kdTestScratchPrefix, sweepKdTestScratchRoots } from "./test-paths";

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

describe("kd test scratch roots", () => {
  it("sweeps abandoned per-process roots but leaves a live process root alone", () => {
    const root = mkdtempSync(join(tmpdir(), "kd-test-paths-"));
    roots.push(root);
    const abandoned = join(root, "kanna-kd-tests-1001");
    const live = join(root, "kanna-kd-tests-1002");
    mkdirSync(abandoned);
    mkdirSync(live);
    writeFileSync(join(abandoned, "large-fixture"), "x");

    expect(sweepKdTestScratchRoots(root, (pid) => pid === 1002)).toEqual([abandoned]);
    expect(existsSync(abandoned)).toBe(false);
    expect(existsSync(live)).toBe(true);
  });

  it("creates every scratch directory under this process's own root", async () => {
    const processRoot = join(tmpdir(), `kanna-kd-tests-${process.pid}`);
    const asynchronous = await kdTestScratchDir("probe-");
    const synchronous = kdTestScratchDirSync("probe-");

    expect(dirname(asynchronous)).toBe(processRoot);
    expect(dirname(synchronous)).toBe(processRoot);
    expect(asynchronous).not.toBe(synchronous);
    expect(statSync(asynchronous).isDirectory()).toBe(true);
    expect(statSync(synchronous).isDirectory()).toBe(true);
    expect(kdTestScratchPrefix("probe-")).toBe(join(processRoot, "probe-"));
  });
});
