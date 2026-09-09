import { lstatSync, mkdirSync, readFileSync, readlinkSync, realpathSync, rmSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { userInfo } from "node:os";
import { basename, dirname, isAbsolute, join } from "node:path";
import type { DatabaseSync as DatabaseSyncType } from "node:sqlite";

// Resolved at runtime rather than imported. esbuild's builtin list for this
// bundle's target predates `node:sqlite`, so a static import is emitted as
// `from "sqlite"` — a package that does not exist — and a cold `kd` launch
// dies with ERR_MODULE_NOT_FOUND before it runs anything. `createRequire` is
// opaque to the bundler, which is the point.
const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite") as {
  DatabaseSync: new (path: string) => DatabaseSyncType;
};

export interface DevDbTarget {
  dbName: string;
  dbPath: string;
}

const productionDbName = "kanna-v2.db";
// Every bundle identifier whose default database is a real desktop database:
// the shipped app, the staging desktop — an owner's daily driver — and the
// pre-rename identifier. Mirrors
// `kanna_runtime_defaults::database_access::PROTECTED_BUNDLE_IDENTIFIERS`, held
// in step with it by a contract test.
export const protectedBundleIdentifiers = ["build.kanna", "build.kanna.staging", "com.kanna.app"];

function resolvedDatabasePath(path: string, depth = 0): string {
  if (depth > 128) throw new Error("REFUSED: database path has too many symbolic links or ancestors.");
  // Preserve symlink/.. traversal until the filesystem resolves it.
  const absolute = isAbsolute(path) ? path : `${process.cwd()}/${path}`;
  try {
    if (lstatSync(absolute).isSymbolicLink()) {
      const link = readlinkSync(absolute);
      return resolvedDatabasePath(isAbsolute(link) ? link : `${dirname(absolute)}/${link}`, depth + 1);
    }
    return realpathSync(absolute);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    const parent = dirname(absolute);
    if (parent === absolute) throw error;
    return join(resolvedDatabasePath(parent, depth + 1), basename(absolute));
  }
}

function fileIdentity(path: string): string | undefined {
  try {
    const stat = statSync(path, { bigint: true });
    return `${stat.dev}:${stat.ino}`;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    return undefined;
  }
}

export function assertNotProductionDb(target: DevDbTarget): void {
  if (!target.dbPath || target.dbPath.startsWith("file:")) {
    throw new Error("REFUSED: database access requires a nonempty filesystem path.");
  }
  const path = resolvedDatabasePath(target.dbPath);
  const identity = fileIdentity(path);
  const home = userInfo().homedir;
  const productionAlias = identity !== undefined &&
    [join(home, "Library", "Application Support"), join(home, ".local", "share")].some(root =>
      protectedBundleIdentifiers.some(bundle =>
        fileIdentity(join(root, bundle, productionDbName)) === identity));
  if (target.dbName === productionDbName || basename(path).toLowerCase() === productionDbName || productionAlias) {
    throw new Error(
      "REFUSED: kd will not start, reset, or seed against the production database (kanna-v2.db). Run from a worktree or set KANNA_DB_NAME to a non-production name."
    );
  }
}

export function deleteSqliteDb(dbPath: string): void {
  assertNotProductionDb({ dbName: basename(dbPath), dbPath });
  mkdirSync(dirname(dbPath), { recursive: true });
  rmSync(dbPath, { force: true });
  rmSync(`${dbPath}-wal`, { force: true });
  rmSync(`${dbPath}-shm`, { force: true });
}

/**
 * Both of these used to shell out to the `sqlite3` CLI, which made a command
 * line tool a prerequisite for `kd dev up --delete-db` and for every E2E lane
 * that seeds a database. On a stock Ubuntu image it is not installed, and the
 * failure surfaced as `spawn sqlite3 ENOENT` from inside a test run — nowhere
 * near the thing that needed it.
 *
 * `node:sqlite` is bundled with the Node that `kd` already requires, so this
 * removes the dependency rather than moving it. It also keeps the repository's
 * "bundled SQLite" rule intact: nothing here links a system libsqlite3.
 */
function withDatabase<T>(dbPath: string, work: (db: DatabaseSyncType) => T): T {
  const db = new DatabaseSync(dbPath);
  try {
    return work(db);
  } finally {
    db.close();
  }
}

export function resetSqliteDb(target: DevDbTarget): void {
  assertNotProductionDb(target);
  deleteSqliteDb(target.dbPath);
  try {
    withDatabase(target.dbPath, (db) => db.exec("PRAGMA user_version;"));
  } catch (error) {
    throw new Error(`Failed to initialize ${target.dbPath}: ${(error as Error).message}`);
  }
}

export function seedSqliteDb(repoRoot: string, dbPath: string): void {
  assertNotProductionDb({ dbName: basename(dbPath), dbPath });
  const seedPath = join(repoRoot, "apps", "desktop", "tests", "e2e", "seed.sql");
  try {
    const seed = readFileSync(seedPath, "utf8");
    withDatabase(dbPath, (db) => db.exec(seed));
  } catch (error) {
    throw new Error(`Failed to seed ${dbPath}: ${(error as Error).message}`);
  }
}
