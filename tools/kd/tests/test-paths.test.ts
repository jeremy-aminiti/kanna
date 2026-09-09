import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { sweepKdTestScratchRoots } from "./test-paths";

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
});
