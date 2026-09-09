import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { buildStorageSettingsPath } from "../src/runtime/build-storage";

interface GateResult { startedAt: number; finishedAt: number }

function runHolder(homeDir: string): Promise<GateResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ["--import", "tsx", "tests/fixtures/rust-gate-holder.ts", homeDir, "250"], {
      cwd: join(import.meta.dirname, ".."),
      env: { ...process.env, HOME: homeDir }
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0) reject(new Error(stderr));
      else resolve(JSON.parse(stdout) as GateResult);
    });
  });
}

describe("Rust gate machine coordination", () => {
  const fixtures: string[] = [];
  afterEach(() => fixtures.splice(0).forEach((fixture) => rmSync(fixture, { recursive: true, force: true })));

  it("bounds concurrent kd processes without sharing a Cargo target", async () => {
    const home = mkdtempSync(join(tmpdir(), "kd-rust-gate-"));
    fixtures.push(home);
    const settings = buildStorageSettingsPath(home, {}, process.platform);
    mkdirSync(join(settings, ".."), { recursive: true });
    writeFileSync(settings, JSON.stringify({ rustBuildRoot: join(home, "external"), rustGateConcurrency: 1 }));

    const [first, second] = await Promise.all([runHolder(home), runHolder(home)]);
    const earlier = first.startedAt < second.startedAt ? first : second;
    const later = earlier === first ? second : first;
    expect(later.startedAt).toBeGreaterThanOrEqual(earlier.finishedAt);
  }, 15_000);
});
