import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  buildStorageSettingsPath,
  configureExternalWorkspaceBuild,
  readExternalBuildRoot,
  readRustGateConcurrency
} from "../src/runtime/build-storage";

describe("build storage", () => {
  const fixtures: string[] = [];

  afterEach(() => fixtures.splice(0).forEach((fixture) => rmSync(fixture, { recursive: true, force: true })));

  it("resolves its machine-local settings path on macOS and XDG", () => {
    expect(buildStorageSettingsPath("/Users/ada", {}, "darwin")).toBe(
      "/Users/ada/Library/Caches/kanna/build-storage.local.json"
    );
    expect(buildStorageSettingsPath("/home/ada", { XDG_CACHE_HOME: "/cache" }, "linux")).toBe(
      "/cache/kanna/build-storage.local.json"
    );
  });

  it("rejects malformed and non-absolute settings", () => {
    const home = fixture();
    const path = buildStorageSettingsPath(home, {}, "darwin");
    mkdirSync(join(home, "Library", "Caches", "kanna"), { recursive: true });
    writeFileSync(path, "not json");
    expect(() => readExternalBuildRoot(home, {}, "darwin")).toThrow(/Invalid JSON/);
    writeFileSync(path, JSON.stringify({ rustBuildRoot: "relative" }));
    expect(() => readExternalBuildRoot(home, {}, "darwin")).toThrow(/absolute/);
    writeFileSync(path, JSON.stringify({ rustBuildRoot: "/volume/builds", rustGateConcurrency: 0 }));
    expect(() => readRustGateConcurrency(home, {}, "darwin")).toThrow(/positive integer/);
  });

  it("allows a Rust-gate-only settings file without an external build root", () => {
    const home = fixture();
    const path = buildStorageSettingsPath(home, {}, "darwin");
    mkdirSync(join(home, "Library", "Caches", "kanna"), { recursive: true });
    writeFileSync(path, JSON.stringify({ rustGateConcurrency: 1 }));

    expect(readExternalBuildRoot(home, {}, "darwin")).toBeUndefined();
    expect(readRustGateConcurrency(home, {}, "darwin")).toBe(1);
  });

  it("configures a new workspace target and durable record", () => {
    const root = fixture();
    const workspace = join(root, "task-123");
    mkdirSync(workspace);
    expect(configureExternalWorkspaceBuild(workspace, join(root, "volume")).changed).toBe(true);
    const target = join(root, "volume", "task-123");
    expect(lstatSync(join(workspace, ".build")).isSymbolicLink()).toBe(true);
    expect(readFileSync(join(workspace, ".kanna-external-build-target"), "utf8")).toBe(`${target}\n`);
  });

  it("records an already-matching link before returning", () => {
    const root = fixture();
    const workspace = join(root, "task-123");
    const target = join(root, "volume", "task-123");
    mkdirSync(workspace);
    mkdirSync(target, { recursive: true });
    symlinkSync(target, join(workspace, ".build"));
    expect(configureExternalWorkspaceBuild(workspace, join(root, "volume"))).toEqual({ target, changed: false });
    expect(readFileSync(join(workspace, ".kanna-external-build-target"), "utf8")).toBe(`${target}\n`);
  });

  it("refuses a conflicting record even when the link matches", () => {
    const root = fixture();
    const workspace = join(root, "task-123");
    const target = join(root, "volume", "task-123");
    mkdirSync(workspace);
    mkdirSync(target, { recursive: true });
    symlinkSync(target, join(workspace, ".build"));
    writeFileSync(join(workspace, ".kanna-external-build-target"), `${join(root, "other", "task-123")}\n`);
    expect(() => configureExternalWorkspaceBuild(workspace, join(root, "volume"))).toThrow(/Refusing to replace/);
    expect(existsSync(join(workspace, ".build"))).toBe(true);
  });

  function fixture(): string {
    const root = mkdtempSync(join(tmpdir(), "kd-build-storage-"));
    fixtures.push(root);
    return root;
  }
});
