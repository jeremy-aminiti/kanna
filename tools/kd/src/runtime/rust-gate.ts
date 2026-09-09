import { existsSync, mkdirSync, readFileSync, rmSync, rmdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { appCacheDir } from "../context";
import { readRustGateConcurrency } from "./build-storage";

const HELD = "KANNA_RUST_GATE_HELD";
const POLL_MS = 100;

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function ownerIsAlive(lock: string): boolean {
  try {
    const owner = JSON.parse(readFileSync(join(lock, "owner.json"), "utf8")) as { pid?: unknown };
    if (!Number.isInteger(owner.pid) || (owner.pid as number) < 1) return true;
    try {
      process.kill(owner.pid as number, 0);
      return true;
    } catch (error) {
      return (error as NodeJS.ErrnoException).code === "EPERM";
    }
  } catch {
    // Do not steal a slot during the small mkdir/write ownership window.
    return true;
  }
}

function releaseAbandonedLock(lock: string): void {
  if (!existsSync(lock) || ownerIsAlive(lock)) return;
  rmSync(lock, { recursive: true, force: true });
}

/**
 * Bounds Rust gates per machine while preserving every worktree's private
 * target. Nested kd commands inherit HELD so `test rust` can dispatch its
 * sidecar build without consuming a second slot and deadlocking itself.
 */
export async function withRustGate<T>(input: {
  homeDir: string;
  env: NodeJS.ProcessEnv;
  run: (env: NodeJS.ProcessEnv) => Promise<T>;
  platform?: NodeJS.Platform;
}): Promise<T> {
  if (input.env[HELD] === "1") return input.run(input.env);

  const platform = input.platform ?? process.platform;
  const cap = readRustGateConcurrency(input.homeDir, input.env, platform);
  const root = join(appCacheDir(input.homeDir, input.env, platform), "kanna", "rust-gates");
  mkdirSync(root, { recursive: true });
  let slot: string | undefined;
  while (!slot) {
    for (let index = 0; index < cap; index += 1) {
      const candidate = join(root, `slot-${index}`);
      try {
        mkdirSync(candidate);
        writeFileSync(join(candidate, "owner.json"), JSON.stringify({ pid: process.pid }), { mode: 0o600, flag: "wx" });
        slot = candidate;
        break;
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
        releaseAbandonedLock(candidate);
      }
    }
    if (!slot) await sleep(POLL_MS);
  }

  try {
    return await input.run({ ...input.env, [HELD]: "1" });
  } finally {
    rmSync(join(slot, "owner.json"), { force: true });
    rmdirSync(slot);
  }
}
