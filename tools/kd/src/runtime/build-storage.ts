import { existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, readdirSync, rmdirSync, symlinkSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { appCacheDir } from "../context";
import { EXTERNAL_WORKSPACE_BUILD_RECORD, WORKSPACE_BUILD_DIRECTORY } from "./workspace-build";

const SETTINGS_FILE = "build-storage.local.json";

interface BuildStorageSettings {
  rustBuildRoot?: unknown;
  rustGateConcurrency?: unknown;
}

export function buildStorageSettingsPath(homeDir: string, env: NodeJS.ProcessEnv, platform = process.platform): string {
  return join(appCacheDir(homeDir, env, platform), "kanna", SETTINGS_FILE);
}

export function readExternalBuildRoot(homeDir: string, env: NodeJS.ProcessEnv, platform = process.platform): string | undefined {
  const path = buildStorageSettingsPath(homeDir, env, platform);
  if (!existsSync(path)) return undefined;
  let parsed: unknown;
  try { parsed = JSON.parse(readFileSync(path, "utf8")); } catch { throw new Error(`[kd] Invalid JSON in ${path}`); }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error(`[kd] Invalid build-storage settings in ${path}`);
  const root = (parsed as BuildStorageSettings).rustBuildRoot;
  if (typeof root !== "string" || !root.trim() || !root.startsWith("/")) {
    throw new Error(`[kd] ${path} must contain an absolute \"rustBuildRoot\" string`);
  }
  return resolve(root);
}

/** The machine-wide number of Rust gates allowed to grow private targets. */
export function readRustGateConcurrency(homeDir: string, env: NodeJS.ProcessEnv, platform = process.platform): number {
  const path = buildStorageSettingsPath(homeDir, env, platform);
  if (!existsSync(path)) return 2;
  let parsed: unknown;
  try { parsed = JSON.parse(readFileSync(path, "utf8")); } catch { throw new Error(`[kd] Invalid JSON in ${path}`); }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error(`[kd] Invalid build-storage settings in ${path}`);
  const cap = (parsed as BuildStorageSettings).rustGateConcurrency;
  if (cap === undefined) return 2;
  if (!Number.isInteger(cap) || (cap as number) < 1) {
    throw new Error(`[kd] ${path} must contain a positive integer "rustGateConcurrency"`);
  }
  return cap as number;
}

function ensureExternalBuildRecord(record: string, target: string): void {
  if (existsSync(record)) {
    if (readFileSync(record, "utf8").trim() !== target) {
      throw new Error(`[kd] Refusing to replace external .build target record ${record}`);
    }
    return;
  }
  writeFileSync(record, `${target}\n`, { mode: 0o600, flag: "wx" });
}

/** Configure one safe, identity-bound external target for this worktree. */
export function configureExternalWorkspaceBuild(repoRoot: string, root: string): { target: string; changed: boolean } {
  const workspace = resolve(repoRoot);
  const build = join(workspace, WORKSPACE_BUILD_DIRECTORY);
  const target = join(resolve(root), basename(workspace));
  if (target.startsWith(`${workspace}/`) || target === workspace) throw new Error(`[kd] External Rust build root must be outside ${workspace}`);
  const record = join(workspace, EXTERNAL_WORKSPACE_BUILD_RECORD);
  try {
    const stats = lstatSync(build);
    if (stats.isSymbolicLink()) {
      if (resolve(workspace, readlinkSync(build)) !== target) throw new Error(`[kd] .build already points to another location; leaving it unchanged`);
      // A matching link can predate the settings-backed hook.  Persist the
      // exact target before returning so close can still reclaim it after the
      // volume disappears and a local fallback replaces the dangling link.
      ensureExternalBuildRecord(record, target);
      return { target, changed: false };
    }
    if (!stats.isDirectory()) throw new Error(`[kd] .build is not a directory or symlink`);
    if (readdirSync(build).length === 0) {
      // Empty fallback left while a volume was unavailable.
      rmdirSync(build);
    } else throw new Error(`[kd] .build contains artifacts; move them with the legacy setup hook before enabling build-storage.local.json`);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }
  mkdirSync(target, { recursive: true });
  ensureExternalBuildRecord(record, target);
  symlinkSync(target, build);
  return { target, changed: true };
}
