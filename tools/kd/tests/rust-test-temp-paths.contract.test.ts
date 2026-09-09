import { readFileSync, readdirSync } from "node:fs";
import { relative, resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * A Rust test path under the temp directory must be unique to the process that
 * builds it.
 *
 * Several tasks' gates run at once on this machine, each from its own worktree,
 * and `std::env::temp_dir()` is the one directory they all share. A path built
 * from a label alone therefore names the same file in every one of those runs,
 * and a per-process counter does not save it: every process starts its counter
 * at the same value and hands out the same names in the same order.
 * `Db::open_for_tests` deletes the file it is given, so the collision is not a
 * shared read — one run truncates another run's database mid-test.
 *
 * That produced a 74-failure `./kd test all` for task d2eb7fa0, and its
 * failures do not look like a collision from the inside: `table repo already
 * exists`, `no such table: worktree`, `disk I/O error`, an assertion reading
 * the *other* run's fixture, or simply a test that is slow because it is
 * fighting for the same file. Reviewers read them as machine load and closed
 * them for a week. Two copies of `kanna_server`'s test binary run at once
 * failed 44 tests between them; the same pair after the fix failed none.
 *
 * The fix is `crates/kanna-server/src/test_paths.rs` inside that crate, and
 * `process::id()` in the helpers of the crates that have their own. This
 * contract keeps the class dead the way
 * `kanna-test-fetch.contract.test.ts` keeps the `lan_trust` 403 dead: it is a
 * source scan, so it runs in `pnpm test` — the lane that runs every time —
 * rather than in a lane somebody has to remember.
 *
 * ## What it can and cannot see
 *
 * It reads the text joined onto `temp_dir()` and asks whether anything in it
 * can differ between two processes running at the same moment. A wall clock
 * cannot: two processes started together read the same nanoseconds often
 * enough to matter, which is why a timestamp alone is not accepted here.
 *
 * It reads the joined text together with the few lines above it, so a pid
 * bound to a local one statement earlier still counts. It cannot see a path
 * assembled further away than that, and it does not try. Those exist; when you
 * write one, route it through the crate's unique-path helper.
 *
 * A path that must be shared — a fixture two processes are *meant* to meet on
 * — declares itself with a `shared-temp-path:` comment giving the reason, on
 * the line of the `temp_dir()` call or the line above.
 */

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");

/** Crates whose tests run in the concurrent gates this contract protects. */
const SCANNED_DIRECTORIES = [
  "apps/desktop/src-tauri/src",
  "crates",
  "packages/terminal-recovery/src",
];

const SKIPPED_DIRECTORY_NAMES = new Set(["node_modules", "target", ".build", "vendor"]);

/**
 * Files exempt from the scan: this one quotes the patterns it bans, and
 * `test_paths.rs` is the mechanism the rest of the crate is held to.
 */
const SELF = new Set([
  "tools/kd/tests/rust-test-temp-paths.contract.test.ts",
  "crates/kanna-server/src/test_paths.rs",
]);

const EXEMPTION_MARKER = "shared-temp-path:";

/** How far above the `temp_dir()` call a uniqueness token still counts. */
const PRECEDING_LINES_IN_SCOPE = 6;

/**
 * Text that makes a name differ between two processes running at the same
 * moment. `process::id` is the primitive; the rest are the helpers and types
 * that are built on it, or that ask the OS for a fresh directory outright.
 *
 * A timestamp is deliberately absent: it is the thing that looked like
 * uniqueness and was not.
 */
const PROCESS_UNIQUE_TOKENS = [
  "process::id",
  "test_paths::",
  "unique",
  "suffix",
  "TempDir",
  "tempdir",
  "Uuid",
  "uuid",
];

function sourceFilesUnder(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) {
      return SKIPPED_DIRECTORY_NAMES.has(entry.name) ? [] : sourceFilesUnder(path);
    }
    return entry.name.endsWith(".rs") ? [path] : [];
  });
}

/**
 * The text passed to a call whose `(` sits at `openIndex`, read by matching
 * parentheses so a multi-line call is captured whole.
 */
function callArguments(source: string, openIndex: number): string {
  let depth = 0;
  for (let index = openIndex; index < source.length; index += 1) {
    const character = source[index];
    if (character === "(") depth += 1;
    else if (character === ")") {
      depth -= 1;
      if (depth === 0) return source.slice(openIndex + 1, index);
    }
  }
  return source.slice(openIndex + 1);
}

function lineNumberAt(source: string, index: number): number {
  return source.slice(0, index).split("\n").length;
}

function isExempt(source: string, index: number): boolean {
  const currentLineStart = source.lastIndexOf("\n", index) + 1;
  const currentLineEnd = source.indexOf("\n", index);
  const currentLine = source.slice(currentLineStart, currentLineEnd === -1 ? undefined : currentLineEnd);
  const previousLineStart = source.lastIndexOf("\n", currentLineStart - 2) + 1;
  const previousLine = source.slice(previousLineStart, currentLineStart);
  return [currentLine, previousLine].some((line) => {
    const marker = line.indexOf(EXEMPTION_MARKER);
    return marker !== -1 && line.slice(marker + EXEMPTION_MARKER.length).trim().length > 0;
  });
}

export interface SharedTempPath {
  file: string;
  line: number;
  snippet: string;
}

/**
 * Every `temp_dir().join(...)` in `source` whose name two concurrent processes
 * could both produce. Exported so the detector's own behaviour is pinned by
 * the fixtures below rather than only by the repository happening to be clean.
 */
export function findSharedTempPaths(file: string, source: string): SharedTempPath[] {
  const findings: SharedTempPath[] = [];
  for (const match of source.matchAll(/temp_dir\(\)\s*\.\s*join\s*\(/g)) {
    const openIndex = match.index + match[0].length - 1;
    const joined = callArguments(source, openIndex);
    // A helper that binds `let pid = std::process::id();` a statement above and
    // interpolates `{pid}` is as unique as one that calls it inline, so the
    // preceding lines count as part of the expression.
    const scope = source.slice(0, openIndex).split("\n").slice(-PRECEDING_LINES_IN_SCOPE).join("\n") + joined;
    if (PROCESS_UNIQUE_TOKENS.some((token) => scope.includes(token))) continue;
    if (isExempt(source, match.index)) continue;
    findings.push({
      file,
      line: lineNumberAt(source, match.index),
      snippet: joined.replace(/\s+/g, " ").trim().slice(0, 120),
    });
  }
  return findings;
}

describe("Rust temp paths in test code", () => {
  it("names nothing a concurrently running gate could also name", () => {
    const findings = SCANNED_DIRECTORIES.flatMap((directory) =>
      sourceFilesUnder(resolve(repoRoot, directory)).flatMap((path) => {
        const file = relative(repoRoot, path);
        return SELF.has(file) ? [] : findSharedTempPaths(file, readFileSync(path, "utf8"));
      }),
    );

    expect(
      findings.map(
        (finding) =>
          `${finding.file}:${finding.line} builds a temp path from "${finding.snippet}", ` +
          "which another worktree's gate produces identically. Route it through the crate's " +
          "unique-path helper (kanna-server: crate::test_paths) or add std::process::id().",
      ),
    ).toEqual([]);
  });

  it("accepts a path carrying the process id and rejects the same path without it", () => {
    const withPid = 'std::env::temp_dir().join(format!("kanna-db-{}", std::process::id()))';
    const withoutPid = 'std::env::temp_dir().join(format!("kanna-db-{nanos}"))';

    expect(findSharedTempPaths("probe.rs", withPid)).toEqual([]);
    expect(findSharedTempPaths("probe.rs", withoutPid)).toHaveLength(1);
  });

  it("counts a process id bound a statement above the join", () => {
    const bound = [
      "let pid = std::process::id();",
      "let nanos = now();",
      'let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}"));',
    ].join("\n");
    const unbound = [
      "let nanos = now();",
      'let path = std::env::temp_dir().join(format!("{prefix}-{nanos}"));',
    ].join("\n");

    expect(findSharedTempPaths("probe.rs", bound)).toEqual([]);
    expect(findSharedTempPaths("probe.rs", unbound)).toHaveLength(1);
  });

  it("reads a multi-line join whole rather than stopping at the first line", () => {
    const source = [
      "let dir = std::env::temp_dir().join(format!(",
      '    "kanna-db-{}-{}",',
      "    std::process::id(),",
      "    counter,",
      "));",
    ].join("\n");

    expect(findSharedTempPaths("probe.rs", source)).toEqual([]);
  });

  it("honours a declared shared fixture and refuses a bare marker", () => {
    const declared = [
      "// shared-temp-path: both processes meet on this handoff socket by design",
      'let dir = std::env::temp_dir().join("kanna-handoff-rendezvous");',
    ].join("\n");
    const bare = [
      "// shared-temp-path:",
      'let dir = std::env::temp_dir().join("kanna-handoff-rendezvous");',
    ].join("\n");

    expect(findSharedTempPaths("probe.rs", declared)).toEqual([]);
    expect(findSharedTempPaths("probe.rs", bare)).toHaveLength(1);
  });
});
