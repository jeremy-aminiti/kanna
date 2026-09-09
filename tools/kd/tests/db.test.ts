import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";
import { assertNotProductionDb, deleteSqliteDb, seedSqliteDb, resetSqliteDb } from "../src/runtime/db";

describe("dev database safety", () => {
  it("refuses production database names and paths", () => {
    expect(() => assertNotProductionDb({ dbName: "kanna-v2.db", dbPath: "/tmp/dev.db" })).toThrow(
      "production database"
    );
    expect(() => assertNotProductionDb({ dbName: "dev.db", dbPath: "/Users/test/Library/Application Support/build.kanna/kanna-v2.db" })).toThrow(
      "production database"
    );
  });

  it("deletes sqlite sidecars and recreates an openable dev database before startup", async () => {
    const dir = mkdtempSync(join(tmpdir(), "kd-db-"));
    const dbPath = join(dir, "dev.db");
    writeFileSync(dbPath, "old");
    writeFileSync(`${dbPath}-wal`, "wal");
    writeFileSync(`${dbPath}-shm`, "shm");
    resetSqliteDb({ dbName: "dev.db", dbPath });

    expect(existsSync(dbPath)).toBe(true);
    expect(existsSync(`${dbPath}-wal`)).toBe(false);
    expect(existsSync(`${dbPath}-shm`)).toBe(false);
    // Openable, and openable by SQLite rather than by whatever wrote "old".
    const db = new DatabaseSync(dbPath);
    try {
      expect(db.prepare("PRAGMA user_version").get()).toEqual({ user_version: 0 });
    } finally {
      db.close();
    }
  });

  it("seeds without needing a sqlite3 command line tool", () => {
    // The `sqlite3` CLI is not installed on a stock Ubuntu image, and its
    // absence used to surface as `spawn sqlite3 ENOENT` from inside an E2E run.
    const dir = mkdtempSync(join(tmpdir(), "kd-db-seed-"));
    const dbPath = join(dir, "dev.db");
    resetSqliteDb({ dbName: "dev.db", dbPath });
    const repoRoot = join(dir, "repo");
    const seedDir = join(repoRoot, "apps", "desktop", "tests", "e2e");
    mkdirSync(seedDir, { recursive: true });
    writeFileSync(join(seedDir, "seed.sql"), "CREATE TABLE seeded (id INTEGER);\n");

    seedSqliteDb(repoRoot, dbPath);

    const db = new DatabaseSync(dbPath);
    try {
      expect(
        db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'seeded'").get(),
      ).toEqual({ name: "seeded" });
    } finally {
      db.close();
    }
  });

  it("refuses to reset the production database before touching the disk", () => {
    expect(() => resetSqliteDb({ dbName: "kanna-v2.db", dbPath: "/tmp/kanna-v2.db" })).toThrow(
      "production database",
    );
  });

  it("refuses production at the direct delete and seed boundaries", () => {
    const dir = mkdtempSync(join(tmpdir(), "kd-db-guard-"));
    const dbPath = join(dir, "kanna-v2.db");
    writeFileSync(dbPath, "owner data");
    expect(() => deleteSqliteDb(dbPath)).toThrow("production database");
    expect(() => seedSqliteDb(dir, dbPath)).toThrow("production database");
    expect(readFileSync(dbPath, "utf8")).toBe("owner data");
    rmSync(dir, { recursive: true });
  });

  it("refuses aliases even before the production file exists", () => {
    const dir = mkdtempSync(join(tmpdir(), "kd-db-alias-"));
    const production = join(dir, "kanna-v2.db");
    const alias = join(dir, "dev.db");
    symlinkSync(production, alias);
    expect(() => deleteSqliteDb(alias)).toThrow("production database");
    writeFileSync(production, "owner data");
    expect(() => deleteSqliteDb(alias)).toThrow("production database");
    expect(readFileSync(production, "utf8")).toBe("owner data");
    rmSync(dir, { recursive: true });
  });
  it("resolves symlink parents before interpreting dot-dot", () => {
    const dir = mkdtempSync(join(tmpdir(), "kd-db-parent-alias-"));
    const parent = join(dir, "real", "parent");
    mkdirSync(join(parent, "child"), { recursive: true });
    writeFileSync(join(parent, "kanna-v2.db"), "owner data");
    symlinkSync("kanna-v2.db", join(parent, "dev.db"));
    symlinkSync(join(parent, "child"), join(dir, "alias"));
    expect(() => assertNotProductionDb({
      dbName: "dev.db", dbPath: `${dir}/alias/../dev.db`
    })).toThrow("production database");
    rmSync(dir, { recursive: true });
  });
});
