import { execFileSync, spawnSync } from "node:child_process";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { buildStorageSettingsPath } from "../src/runtime/build-storage";
import { syncMachineLocalConfig, writeCargoConfig } from "../src/runtime/env-sync";

describe("env sync", () => {
  it("writes repo-local Cargo target and build metadata config", () => {
    const root = mkdtempSync(join(tmpdir(), "kd-env-"));
    const path = writeCargoConfig(root);

    expect(path).toBe(join(root, ".cargo/config.toml"));
    expect(readFileSync(path, "utf8")).toBe(
      '[build]\ntarget-dir = ".build"\nbuild-dir = ".build/cargo-build"\n'
    );
  });

  it("runs the env-sync CLI with a Rust-gate-only machine-local settings file", () => {
    const fixture = mkdtempSync(join(tmpdir(), "kd-env-sync-cli-"));
    const home = join(fixture, "home");
    const repo = join(fixture, "repo");
    try {
      mkdirSync(join(repo, ".kanna"), { recursive: true });
      mkdirSync(join(repo, "apps", "desktop", "src-tauri"), { recursive: true });
      writeFileSync(join(repo, ".kanna", "config.json"), "{}\n");
      writeFileSync(join(repo, "apps", "desktop", "src-tauri", "tauri.conf.json"), '{"identifier":"build.kanna"}\n');
      execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo, stdio: "ignore" });
      execFileSync("git", ["add", "."], { cwd: repo, stdio: "ignore" });
      execFileSync("git", ["-c", "user.name=Kanna Test", "-c", "user.email=test@kanna.invalid", "commit", "-m", "fixture"], {
        cwd: repo,
        stdio: "ignore"
      });
      const settings = buildStorageSettingsPath(home, {}, process.platform);
      mkdirSync(resolve(settings, ".."), { recursive: true });
      writeFileSync(settings, JSON.stringify({ rustGateConcurrency: 1 }));
      symlinkSync(resolve(import.meta.dirname, "../../..", "kd"), join(repo, "kd"));

      const result = spawnSync(
        "./kd",
        ["env", "sync"],
        {
          cwd: repo,
          env: { ...process.env, HOME: home, KANNA_KD_CACHE_ROOT: join(fixture, "kd-cache") },
          encoding: "utf8"
        }
      );

      expect(result.status, result.stderr).toBe(0);
      expect(result.stdout).toContain("Synced Kanna dev environment files.");
      expect(existsSync(join(repo, ".build"))).toBe(false);
      expect(readFileSync(join(repo, ".cargo", "config.toml"), "utf8")).toContain('target-dir = ".build"');
    } finally {
      rmSync(fixture, { recursive: true, force: true });
    }
  });

  it("runs the env-sync CLI with an external Rust build root", () => {
    const fixture = mkdtempSync(join(tmpdir(), "kd-env-sync-external-build-"));
    const home = join(fixture, "home");
    const repo = join(fixture, "task-fixture");
    const rustBuildRoot = join(fixture, "external-rust-builds");
    try {
      mkdirSync(join(repo, ".kanna"), { recursive: true });
      mkdirSync(join(repo, "apps", "desktop", "src-tauri"), { recursive: true });
      writeFileSync(join(repo, ".kanna", "config.json"), "{}\n");
      writeFileSync(join(repo, "apps", "desktop", "src-tauri", "tauri.conf.json"), '{"identifier":"build.kanna"}\n');
      execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo, stdio: "ignore" });
      execFileSync("git", ["add", "."], { cwd: repo, stdio: "ignore" });
      execFileSync("git", ["-c", "user.name=Kanna Test", "-c", "user.email=test@kanna.invalid", "commit", "-m", "fixture"], {
        cwd: repo,
        stdio: "ignore"
      });
      const settings = buildStorageSettingsPath(home, {}, process.platform);
      mkdirSync(resolve(settings, ".."), { recursive: true });
      writeFileSync(settings, JSON.stringify({ rustBuildRoot }));
      symlinkSync(resolve(import.meta.dirname, "../../..", "kd"), join(repo, "kd"));

      const result = spawnSync("./kd", ["env", "sync"], {
        cwd: repo,
        env: { ...process.env, HOME: home, KANNA_KD_CACHE_ROOT: join(fixture, "kd-cache") },
        encoding: "utf8"
      });

      const target = join(rustBuildRoot, "task-fixture");
      expect(result.status, result.stderr).toBe(0);
      expect(result.stdout).toContain("using external Rust build root");
      expect(lstatSync(join(repo, ".build")).isSymbolicLink()).toBe(true);
      expect(readlinkSync(join(repo, ".build"))).toBe(target);
      expect(readFileSync(join(repo, ".kanna-external-build-target"), "utf8")).toBe(`${target}\n`);
    } finally {
      rmSync(fixture, { recursive: true, force: true });
    }
  });
});

/**
 * Exercised against real Git worktrees rather than a stubbed layout: the whole
 * point of the copy is that `git worktree add` leaves ignored files behind, and
 * the primary checkout is resolved from the common Git directory.
 */
describe("machine-local repo config sync", () => {
  const fixtures: string[] = [];

  function repoWithWorktree(): { repo: string; worktree: string } {
    const fixture = mkdtempSync(join(tmpdir(), "kd-local-config-"));
    fixtures.push(fixture);
    const repo = join(fixture, "repo");
    const worktree = join(fixture, "worktree");

    mkdirSync(repo);
    execFileSync("git", ["init", "--initial-branch=main"], { cwd: repo, stdio: "ignore" });
    writeFileSync(join(repo, "tracked.txt"), "fixture\n");
    execFileSync("git", ["add", "tracked.txt"], { cwd: repo, stdio: "ignore" });
    execFileSync(
      "git",
      ["-c", "user.name=Kanna Test", "-c", "user.email=test@kanna.invalid", "commit", "-m", "fixture"],
      { cwd: repo, stdio: "ignore" }
    );
    execFileSync("git", ["worktree", "add", "-b", "fixture-worktree", worktree], {
      cwd: repo,
      stdio: "ignore"
    });
    return { repo, worktree };
  }

  function writeLocalConfig(checkout: string, contents: string): string {
    const path = join(checkout, ".kanna", "config.local.json");
    mkdirSync(join(checkout, ".kanna"), { recursive: true });
    writeFileSync(path, contents);
    return path;
  }

  afterEach(() => {
    for (const fixture of fixtures.splice(0)) {
      rmSync(fixture, { recursive: true, force: true });
    }
  });

  it("copies the primary checkout's config into a worktree that has none", () => {
    const { repo, worktree } = repoWithWorktree();
    const source = writeLocalConfig(repo, '{"agentProviders":{"*":{"provider":["claude"]}}}\n');

    const result = syncMachineLocalConfig(worktree);

    expect(result.status).toBe("copied");
    // The primary checkout is resolved through the common Git directory, so the
    // reported source is canonical — on macOS that resolves /var to /private/var.
    expect(result.source).toBe(realpathSync(source));
    expect(result.destination).toBe(join(worktree, ".kanna", "config.local.json"));
    expect(readFileSync(result.destination, "utf8")).toBe(
      '{"agentProviders":{"*":{"provider":["claude"]}}}\n'
    );
  });

  it("overwrites a stale worktree copy on every sync", () => {
    const { repo, worktree } = repoWithWorktree();
    writeLocalConfig(worktree, '{"agentProviders":{"*":{"provider":["opencode"]}}}\n');
    writeLocalConfig(repo, '{"agentProviders":{"*":{"provider":["claude"]}}}\n');

    expect(syncMachineLocalConfig(worktree).status).toBe("copied");
    expect(readFileSync(join(worktree, ".kanna", "config.local.json"), "utf8")).toBe(
      '{"agentProviders":{"*":{"provider":["claude"]}}}\n'
    );

    writeLocalConfig(repo, '{"agentProviders":{"*":{"provider":["codex"]}}}\n');

    expect(syncMachineLocalConfig(worktree).status).toBe("copied");
    expect(readFileSync(join(worktree, ".kanna", "config.local.json"), "utf8")).toBe(
      '{"agentProviders":{"*":{"provider":["codex"]}}}\n'
    );
  });

  it("keeps a worktree-local config when the primary checkout has none", () => {
    const { worktree } = repoWithWorktree();
    const local = writeLocalConfig(worktree, '{"pipeline":"no-review"}\n');

    const result = syncMachineLocalConfig(worktree);

    expect(result.status).toBe("kept-local");
    expect(readFileSync(local, "utf8")).toBe('{"pipeline":"no-review"}\n');
  });

  it("creates nothing when neither checkout has a config", () => {
    const { repo, worktree } = repoWithWorktree();

    expect(syncMachineLocalConfig(worktree).status).toBe("absent");
    expect(existsSync(join(worktree, ".kanna", "config.local.json"))).toBe(false);
    expect(existsSync(join(repo, ".kanna", "config.local.json"))).toBe(false);
  });

  it("leaves the primary checkout's own config untouched", () => {
    const { repo } = repoWithWorktree();
    const source = writeLocalConfig(repo, '{"pipeline":"single-reviewer"}\n');

    const result = syncMachineLocalConfig(repo);

    expect(result.status).toBe("primary-checkout");
    expect(readFileSync(source, "utf8")).toBe('{"pipeline":"single-reviewer"}\n');
  });
});
