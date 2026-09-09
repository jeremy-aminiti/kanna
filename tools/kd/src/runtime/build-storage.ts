import { existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, readdirSync, rmdirSync, symlinkSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { appCacheDir } from "../context";
import { EXTERNAL_WORKSPACE_BUILD_RECORD, WORKSPACE_BUILD_DIRECTORY } from "./workspace-build";

const SETTINGS_FILE = "build-storage.local.json";

export function buildStorageSettingsPath(homeDir: string, env: NodeJS.ProcessEnv, platform = process.platform): string {
  return join(appCacheDir(homeDir, env, platform), "kanna", SETTINGS_FILE);
}

export function readExternalBuildRoot(homeDir: string, env: NodeJS.ProcessEnv, platform = process.platform): string | undefined {
  const path = buildStorageSettingsPath(homeDir, env, platform);
  if (!existsSync(path)) return undefined;
  let parsed: unknown;
  try { parsed = JSON.parse(readFileSync(path, "utf8")); } catch { throw new Error(`[kd] Invalid JSON in ${path}`); }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error(`[kd] Invalid build-storage settings in ${path}`);
  const root = (parsed as { rustBuildRoot?: unknown }).rustBuildRoot;
  if (typeof root !== "string" || !root.trim() || !root.startsWith("/")) {
    throw new Error(`[kd] ${path} must contain an absolute \"rustBuildRoot\" string`);
  }
  return resolve(root);
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
  if (existsSync(record)) {
    if (readFileSync(record, "utf8").trim() !== target) throw new Error(`[kd] Refusing to replace external .build target record ${record}`);
  } else writeFileSync(record, `${target}\n`, { mode: 0o600, flag: "wx" });
  symlinkSync(target, build);
  return { target, changed: true };
}
