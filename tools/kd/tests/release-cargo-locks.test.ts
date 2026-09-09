import { existsSync, readFileSync, readdirSync } from "node:fs";
import { relative, resolve, sep } from "node:path";
import { parse as parseToml } from "smol-toml";
import { describe, expect, it } from "vitest";

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");
const crateUniverseExtension = "@@rules_rust+//crate_universe:extensions.bzl%crate";

interface CargoLockPackage {
  name?: unknown;
  dependencies?: unknown;
}

function parseTomlFile(path: string): Record<string, unknown> {
  return parseToml(readFileSync(resolve(repoRoot, path), "utf8")) as Record<string, unknown>;
}

function expectRecord(value: unknown, context: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${context} is not an object`);
  }
  return value as Record<string, unknown>;
}

function registryManifestDependencies(manifestPath: string): string[] {
  const manifest = parseTomlFile(manifestPath);
  const dependencies = expectRecord(manifest.dependencies, `${manifestPath} [dependencies]`);

  return Object.entries(dependencies)
    .filter(([, specification]) => {
      if (!specification || typeof specification !== "object" || Array.isArray(specification)) {
        return true;
      }
      // Workspace path dependencies use repository-native Bazel targets, not crate-universe labels.
      return typeof (specification as Record<string, unknown>).path !== "string";
    })
    .map(([name]) => name)
    .sort();
}

function extractBracedBlock(source: string, openingBrace: number, context: string): string {
  let depth = 0;
  for (let index = openingBrace; index < source.length; index += 1) {
    if (source[index] === "{") {
      depth += 1;
    } else if (source[index] === "}") {
      depth -= 1;
      if (depth === 0) {
        return source.slice(openingBrace, index + 1);
      }
    }
  }
  throw new Error(`${context} has no closing brace`);
}

function crateUniverseDirectDependencies(repository: string, packagePath: string): string[] {
  const moduleLock = expectRecord(
    JSON.parse(readFileSync(resolve(repoRoot, "MODULE.bazel.lock"), "utf8")) as unknown,
    "MODULE.bazel.lock"
  );
  const extensions = expectRecord(moduleLock.moduleExtensions, "MODULE.bazel.lock moduleExtensions");
  const extension = expectRecord(extensions[crateUniverseExtension], crateUniverseExtension);
  const general = expectRecord(extension.general, `${crateUniverseExtension} general`);
  const repoSpecs = expectRecord(general.generatedRepoSpecs, "crate universe generatedRepoSpecs");
  const repoSpec = expectRecord(repoSpecs[repository], `${repository} generated repo`);
  const attributes = expectRecord(repoSpec.attributes, `${repository} attributes`);
  const contents = expectRecord(attributes.contents, `${repository} contents`);
  const defs = contents["defs.bzl"];
  if (typeof defs !== "string") {
    throw new Error(`${repository} has no generated defs.bzl`);
  }

  const normalDependenciesStart = defs.indexOf("_NORMAL_DEPENDENCIES = {");
  const normalAliasesStart = defs.indexOf("_NORMAL_ALIASES = {", normalDependenciesStart);
  if (normalDependenciesStart < 0 || normalAliasesStart < 0) {
    throw new Error(`${repository} defs.bzl has no normal dependency map`);
  }
  const normalDependencies = defs.slice(normalDependenciesStart, normalAliasesStart);
  const packageMarker = `    "${packagePath}": {`;
  const packageStart = normalDependencies.indexOf(packageMarker);
  if (packageStart < 0) {
    throw new Error(`${repository} has no direct dependencies for ${packagePath}`);
  }
  const openingBrace = normalDependencies.indexOf("{", packageStart);
  const packageDependencies = extractBracedBlock(
    normalDependencies,
    openingBrace,
    `${repository} ${packagePath} dependency map`
  );

  return Array.from(packageDependencies.matchAll(/^\s+"([^"]+)": Label\("@[^/]+\/\//gm))
    .map((match) => match[1])
    .sort();
}

function catalogManifestDependencies(): string[] {
  const manifest = parseTomlFile("crates/kanna-tool-catalog/Cargo.toml");
  const dependencies = {
    ...expectRecord(manifest.dependencies, "kanna-tool-catalog Cargo.toml [dependencies]"),
    ...expectRecord(manifest["dev-dependencies"], "kanna-tool-catalog Cargo.toml [dev-dependencies]"),
  };
  if (!dependencies || typeof dependencies !== "object" || Array.isArray(dependencies)) {
    throw new Error("kanna-tool-catalog Cargo.toml has no [dependencies] table");
  }

  return Object.entries(dependencies as Record<string, unknown>)
    .map(([name, specification]) => {
      if (!specification || typeof specification !== "object" || Array.isArray(specification)) {
        return name;
      }
      const packageName = (specification as Record<string, unknown>).package;
      return typeof packageName === "string" ? packageName : name;
    })
    .sort();
}

function lockedCatalogDependencies(lockPath: string): string[] {
  const lock = parseTomlFile(lockPath);
  if (!Array.isArray(lock.package)) {
    throw new Error(`${lockPath} has no [[package]] entries`);
  }

  const catalog = (lock.package as CargoLockPackage[]).find(
    (lockedPackage) => lockedPackage.name === "kanna-tool-catalog"
  );
  if (!catalog || !Array.isArray(catalog.dependencies)) {
    throw new Error(`${lockPath} has no kanna-tool-catalog dependency list`);
  }

  return catalog.dependencies
    .map((dependency) => {
      if (typeof dependency !== "string") {
        throw new Error(`${lockPath} contains a non-string kanna-tool-catalog dependency`);
      }
      return dependency.split(" ", 1)[0];
    })
    .sort();
}

describe("Bazel release Cargo locks", () => {
  it("exposes every release sidecar registry dependency through all_crate_deps", () => {
    const releaseSidecars = [
      {
        manifestPath: "crates/kanna-cli/Cargo.toml",
        packagePath: "crates/kanna-cli",
        repository: "kanna_cli_crates"
      },
      {
        manifestPath: "crates/kanna-mcp/Cargo.toml",
        packagePath: "crates/kanna-mcp",
        repository: "kanna_mcp_crates"
      },
      {
        manifestPath: "crates/kanna-server/Cargo.toml",
        packagePath: "crates/kanna-server",
        repository: "kanna_server_crates"
      }
    ];

    for (const sidecar of releaseSidecars) {
      expect(
        crateUniverseDirectDependencies(sidecar.repository, sidecar.packagePath),
        sidecar.repository
      ).toEqual(registryManifestDependencies(sidecar.manifestPath));
    }
  });

  it("keeps every direct tool-catalog dependency in each release sidecar graph", () => {
    const manifestDependencies = catalogManifestDependencies();
    const releaseLocks = [
      "crates/kanna-cli/Cargo.lock",
      "crates/kanna-mcp/Cargo.lock",
      "crates/kanna-server/Cargo.lock"
    ];

    for (const lockPath of releaseLocks) {
      expect(lockedCatalogDependencies(lockPath), lockPath).toEqual(manifestDependencies);
    }
  });

  it("derives the CLI catalog target from the CLI synthetic workspace and lock", () => {
    const moduleBazel = readFileSync(resolve(repoRoot, "MODULE.bazel"), "utf8");
    const catalogBuild = readFileSync(
      resolve(repoRoot, "crates/kanna-tool-catalog/BUILD.bazel"),
      "utf8"
    );

    expect(moduleBazel).toMatch(
      /name = "kanna_cli_crates",\s+cargo_lockfile = "\/\/crates\/kanna-cli:Cargo\.lock",\s+manifests = \["\/\/:Cargo\.cli\.toml"\]/
    );
    expect(catalogBuild).toContain(
      'deps = all_crate_deps_for_cli(normal = True, package_name = "crates/kanna-tool-catalog")'
    );
  });
});

// --- Cargo path dependency vs. Bazel dependency parity ------------------------
//
// Only the notarized Bazel release build compiles the desktop `rust_library`
// targets and the x86_64 sidecar variants, so a Cargo `path = ...` dependency
// added without the matching Bazel wiring is invisible to `cargo` and to every
// ordinary build — it surfaces as an `unresolved import` two hundred seconds
// into a release build. These cases are what make it visible to `./kd test
// all`. Two shipped that way out of the Linux Phase 1 merge (PR #1370):
// `kanna_terminal_recovery_x86_64` missing
// `//crates/runtime-defaults:kanna_runtime_defaults`, and `crates/server-process`
// having no BUILD.bazel at all while all three desktop libs imported it.
//
// These cases read the BUILD.bazel files as Starlark text rather than shelling
// out to Bazel, so `pnpm test` stays fast and needs no Bazel install.

/** Crate directories that deliberately have no Bazel target at all. */
const crateDirsWithoutBazelTargets = new Map<string, string>([
  [
    // kanna-worker is the Linux per-user supervisor (docs/specs/linux-desktop-support.md).
    // The Bazel graph exists to build the notarized macOS app and its sidecars;
    // kanna-worker ships in neither, so nothing release-built depends on it and
    // it is built by Cargo only. Give it a BUILD.bazel and delete this entry the
    // day a Bazel-built target takes it as a dependency.
    "crates/kanna-worker",
    "Linux-only supervisor; not part of the macOS release graph"
  ]
]);

/** Directories searched for workspace crates, alongside the desktop crate. */
const crateSearchRoots = ["crates", "packages"];
const desktopCrateDir = "apps/desktop/src-tauri";

interface PathDependency {
  /** Cargo package name, e.g. `kanna-server-process`. */
  name: string;
  /** Repository-relative directory the `path = ...` resolves to. */
  dir: string;
  /** Cargo manifest table the dependency was declared in. */
  table: string;
}

interface WorkspaceCrate {
  dir: string;
  /** Rust crate name of the crate's library, i.e. what a dependent `use`s. */
  libCrateName: string;
  pathDependencies: PathDependency[];
}

interface BazelTarget {
  rule: string;
  name: string;
  crateName: string | null;
  /** Whether the target compiles the crate's own `src/` tree. */
  compilesCrateSources: boolean;
  /** Labels named by the target's `deps` attribute. */
  deps: string[];
}

/**
 * Advance past a Starlark string, comment, or ordinary character, returning the
 * index of the next character to inspect. Keeps bracket matching from tripping
 * over the Python heredocs embedded in the desktop genrules.
 */
function skipStarlarkAtom(source: string, index: number): number | null {
  const char = source[index];
  if (char === "#") {
    const newline = source.indexOf("\n", index);
    return newline < 0 ? source.length : newline;
  }
  if (char !== '"' && char !== "'") {
    return null;
  }
  const triple = source.slice(index, index + 3);
  const quote = triple === '"""' || triple === "'''" ? triple : char;
  let cursor = index + quote.length;
  while (cursor < source.length) {
    if (source[cursor] === "\\") {
      cursor += 2;
      continue;
    }
    if (source.startsWith(quote, cursor)) {
      return cursor + quote.length;
    }
    cursor += 1;
  }
  throw new Error(`unterminated Starlark string at offset ${index}`);
}

/** Extract `(...)` starting at `openingParen`, inclusive of both parentheses. */
function extractCallBlock(source: string, openingParen: number, context: string): string {
  let depth = 0;
  let index = openingParen;
  while (index < source.length) {
    const skipped = skipStarlarkAtom(source, index);
    if (skipped !== null) {
      index = skipped;
      continue;
    }
    const char = source[index];
    if (char === "(") {
      depth += 1;
    } else if (char === ")") {
      depth -= 1;
      if (depth === 0) {
        return source.slice(openingParen, index + 1);
      }
    }
    index += 1;
  }
  throw new Error(`${context} has no closing parenthesis`);
}

/**
 * Read one top-level attribute expression out of a rule call block. Returns the
 * raw Starlark source of the value, so `all_crate_deps(...) + [...]` comes back
 * whole.
 */
function readRuleAttribute(block: string, attribute: string): string | null {
  const pattern = new RegExp(`\\b${attribute}\\s*=\\s*`, "g");
  let match: RegExpExecArray | null;
  while ((match = pattern.exec(block)) !== null) {
    const valueStart = match.index + match[0].length;
    let depth = 0;
    let index = valueStart;
    let callDepth = 0;
    // Depth of the enclosing rule call is 1 at the attribute list level.
    for (let scan = 0; scan < match.index; scan += 1) {
      const skipped = skipStarlarkAtom(block, scan);
      if (skipped !== null) {
        scan = skipped - 1;
        continue;
      }
      const char = block[scan];
      if (char === "(" || char === "[" || char === "{") callDepth += 1;
      else if (char === ")" || char === "]" || char === "}") callDepth -= 1;
    }
    if (callDepth !== 1) {
      continue;
    }
    while (index < block.length) {
      const skipped = skipStarlarkAtom(block, index);
      if (skipped !== null) {
        index = skipped;
        continue;
      }
      const char = block[index];
      if (char === "(" || char === "[" || char === "{") {
        depth += 1;
      } else if (char === ")" || char === "]" || char === "}") {
        if (depth === 0) {
          return block.slice(valueStart, index);
        }
        depth -= 1;
      } else if (char === "," && depth === 0) {
        return block.slice(valueStart, index);
      }
      index += 1;
    }
  }
  return null;
}

function readStringAttribute(block: string, attribute: string): string | null {
  const value = readRuleAttribute(block, attribute);
  if (value === null) {
    return null;
  }
  const literal = value.match(/"([^"]*)"/);
  return literal ? literal[1] : null;
}

function parseBazelTargets(buildFilePath: string): BazelTarget[] {
  const source = readFileSync(resolve(repoRoot, buildFilePath), "utf8");
  const targets: BazelTarget[] = [];
  const rulePattern = /^(rust_library|rust_binary)\(/gm;
  let match: RegExpExecArray | null;
  while ((match = rulePattern.exec(source)) !== null) {
    const openingParen = match.index + match[1].length;
    const block = extractCallBlock(source, openingParen, `${buildFilePath} ${match[1]}`);
    const name = readStringAttribute(block, "name");
    if (!name) {
      throw new Error(`${buildFilePath} has a ${match[1]} target without a name`);
    }
    const srcs = readRuleAttribute(block, "srcs") ?? "";
    const deps = readRuleAttribute(block, "deps") ?? "";
    targets.push({
      rule: match[1],
      name,
      crateName: readStringAttribute(block, "crate_name"),
      compilesCrateSources: srcs.includes('"src/**'),
      deps: Array.from(deps.matchAll(/"((?:\/\/|:)[^"]+)"/g)).map((label) => label[1])
    });
    rulePattern.lastIndex = openingParen + block.length;
  }
  return targets;
}

function bazelTargetsFor(crateDir: string): BazelTarget[] | null {
  if (!existsSync(resolve(repoRoot, crateDir, "BUILD.bazel"))) {
    return null;
  }
  return parseBazelTargets(`${crateDir}/BUILD.bazel`);
}

/**
 * Labels a target depends on, following `:local_target` edges inside the same
 * BUILD file. `//crates/task-transfer` reaches `//crates/runtime-defaults`
 * through `:kanna_task_transfer_lib`, and that counts.
 */
function transitiveDepLabels(targets: BazelTarget[], root: BazelTarget): Set<string> {
  const byName = new Map(targets.map((target) => [target.name, target]));
  const labels = new Set<string>();
  const queue = [root];
  const visited = new Set<string>([root.name]);
  while (queue.length > 0) {
    const target = queue.pop();
    if (!target) break;
    for (const label of target.deps) {
      labels.add(label);
      if (!label.startsWith(":")) continue;
      const local = byName.get(label.slice(1));
      if (local && !visited.has(local.name)) {
        visited.add(local.name);
        queue.push(local);
      }
    }
  }
  return labels;
}

/**
 * Directories of the workspace crates a target names *directly*, e.g.
 * `//crates/runtime-defaults:kanna_runtime_defaults` -> `crates/runtime-defaults`.
 * Compared by directory rather than by label because one crate legitimately has
 * several per-crate-universe targets (`kanna_agent_protocol_for_server` vs
 * `..._for_task_transfer`) that are the same Cargo dependency.
 */
function directWorkspaceDepDirs(target: BazelTarget): string[] {
  return Array.from(
    new Set(
      target.deps
        .filter((label) => label.startsWith("//crates/") || label.startsWith("//packages/"))
        .map((label) => label.slice(2).split(":", 1)[0])
    )
  ).sort();
}

function collectPathDependencies(manifest: Record<string, unknown>, crateDir: string): PathDependency[] {
  const dependencies: PathDependency[] = [];
  const tables: [string, unknown][] = [["[dependencies]", manifest.dependencies]];
  // Bazel builds no tests for these crates, so [dev-dependencies] is out of scope.
  const targetTables = manifest.target;
  if (targetTables && typeof targetTables === "object" && !Array.isArray(targetTables)) {
    for (const [predicate, table] of Object.entries(targetTables as Record<string, unknown>)) {
      const specific = expectRecord(table, `${crateDir} [target.${predicate}]`);
      tables.push([`[target.${predicate}.dependencies]`, specific.dependencies]);
    }
  }

  for (const [table, value] of tables) {
    if (!value || typeof value !== "object" || Array.isArray(value)) continue;
    for (const [name, specification] of Object.entries(value as Record<string, unknown>)) {
      if (!specification || typeof specification !== "object" || Array.isArray(specification)) continue;
      const path = (specification as Record<string, unknown>).path;
      if (typeof path !== "string") continue;
      dependencies.push({
        name,
        dir: relative(repoRoot, resolve(repoRoot, crateDir, path)).split(sep).join("/"),
        table
      });
    }
  }
  return dependencies.sort((left, right) => left.dir.localeCompare(right.dir));
}

function readWorkspaceCrate(crateDir: string): WorkspaceCrate {
  const manifest = parseTomlFile(`${crateDir}/Cargo.toml`);
  const packageTable = expectRecord(manifest.package, `${crateDir}/Cargo.toml [package]`);
  const packageName = packageTable.name;
  if (typeof packageName !== "string") {
    throw new Error(`${crateDir}/Cargo.toml has no package name`);
  }
  const libTable = manifest.lib;
  const libName =
    libTable && typeof libTable === "object" && !Array.isArray(libTable)
      ? (libTable as Record<string, unknown>).name
      : undefined;

  return {
    dir: crateDir,
    libCrateName: typeof libName === "string" ? libName : packageName.replace(/-/g, "_"),
    pathDependencies: collectPathDependencies(manifest, crateDir)
  };
}

function workspaceCrates(): WorkspaceCrate[] {
  const crateDirs = [desktopCrateDir];
  for (const root of crateSearchRoots) {
    for (const entry of readdirSync(resolve(repoRoot, root), { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const crateDir = `${root}/${entry.name}`;
      if (existsSync(resolve(repoRoot, crateDir, "Cargo.toml"))) {
        crateDirs.push(crateDir);
      }
    }
  }
  return crateDirs.sort().map(readWorkspaceCrate);
}

describe("Bazel workspace path dependencies", () => {
  it("gives every Bazel-built workspace crate a BUILD.bazel", () => {
    const missing = workspaceCrates()
      .map((crate) => crate.dir)
      .filter((dir) => !crateDirsWithoutBazelTargets.has(dir))
      .filter((dir) => !existsSync(resolve(repoRoot, dir, "BUILD.bazel")));

    expect(
      missing,
      "workspace crates without a BUILD.bazel; add one, or allow-list the crate " +
        "in crateDirsWithoutBazelTargets with the reason it is outside the Bazel graph"
    ).toEqual([]);

    const staleAllowances = Array.from(crateDirsWithoutBazelTargets)
      .filter(([dir]) => !existsSync(resolve(repoRoot, dir, "Cargo.toml")))
      .map(([dir, reason]) => `${dir} (${reason})`);
    expect(staleAllowances, "allow-listed crates that no longer exist; drop the entry").toEqual([]);
  });

  it("names every Cargo path dependency in every Bazel target that compiles the crate", () => {
    const problems: string[] = [];

    for (const crate of workspaceCrates()) {
      const targets = bazelTargetsFor(crate.dir);
      if (!targets) continue;
      const compilingTargets = targets.filter((target) => target.compilesCrateSources);

      for (const dependency of crate.pathDependencies) {
        const dependencyTargets = bazelTargetsFor(dependency.dir);
        if (!dependencyTargets) {
          problems.push(
            `${dependency.dir} has no BUILD.bazel but ${crate.dir}/Cargo.toml ` +
              `${dependency.table} depends on it by path`
          );
          continue;
        }
        const dependencyCrateName = readWorkspaceCrate(dependency.dir).libCrateName;
        const libraryTargets = dependencyTargets.filter(
          (target) => target.rule === "rust_library" && target.crateName === dependencyCrateName
        );
        if (libraryTargets.length === 0) {
          problems.push(
            `${dependency.dir}/BUILD.bazel has no rust_library producing crate ` +
              `${dependencyCrateName}, required by ${crate.dir}`
          );
          continue;
        }
        const acceptableLabels = new Set(
          libraryTargets.map((target) => `//${dependency.dir}:${target.name}`)
        );

        for (const target of compilingTargets) {
          const reachable = transitiveDepLabels(targets, target);
          if (Array.from(acceptableLabels).some((label) => reachable.has(label))) continue;
          problems.push(
            `//${crate.dir}:${target.name} does not depend on ${dependency.dir} ` +
              `(expected one of ${Array.from(acceptableLabels).sort().join(", ")}), ` +
              `but ${crate.dir}/Cargo.toml ${dependency.table} takes ${dependency.name} as a path dependency`
          );
        }
      }
    }

    expect(problems.sort()).toEqual([]);
  });

  it("keeps every architecture and environment variant of a target on the same workspace deps", () => {
    // rules_rust passes `--extern` for direct deps only, so a binary that
    // reaches a crate through its own `_lib` still fails to compile if its
    // `main.rs` names that crate itself. Text cannot tell which modules use
    // what, but variants of one target compile identical sources, so any
    // difference between an arm64 target and its x86_64 twin — or between the
    // production and staging desktop libs — is a wiring mistake. This is the
    // shape that broke `kanna_terminal_recovery_x86_64` (PR #1374): the lib and
    // the arm64 binary named `//crates/runtime-defaults`, the x86_64 binary did
    // not, and only a release build compiles it.
    const problems: string[] = [];

    for (const crate of workspaceCrates()) {
      const targets = bazelTargetsFor(crate.dir);
      if (!targets) continue;

      // Grouped by rule and crate name, not by crate_root: the three desktop
      // libraries all produce `kanna_desktop_lib` from the same src/ tree but
      // enter it through src/lib.rs, src/lib_bazel.rs and
      // src/lib_staging_bazel.rs, and the production/staging pair is exactly
      // one of the pairs that has to stay in step.
      const variantGroups = new Map<string, BazelTarget[]>();
      for (const target of targets) {
        if (!target.compilesCrateSources || !target.crateName) continue;
        const group = `${target.rule} ${target.crateName}`;
        variantGroups.set(group, [...(variantGroups.get(group) ?? []), target]);
      }

      for (const [group, variants] of variantGroups) {
        if (variants.length < 2) continue;
        const [reference, ...rest] = variants;
        const expected = directWorkspaceDepDirs(reference);
        for (const variant of rest) {
          const actual = directWorkspaceDepDirs(variant);
          if (actual.join("\n") === expected.join("\n")) continue;
          problems.push(
            `//${crate.dir}:${variant.name} and //${crate.dir}:${reference.name} are ` +
              `${group} variants but name different workspace deps ` +
              `([${actual.join(", ")}] vs [${expected.join(", ")}])`
          );
        }
      }
    }

    expect(problems.sort()).toEqual([]);
  });
});
