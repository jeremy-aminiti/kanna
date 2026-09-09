/**
 * The audit that gives the Linux vendoring rule teeth.
 *
 * macOS gets this for free: the app bundle either contains a dependency or the
 * app does not start on a clean machine. A deb has no such property — it links
 * whatever the build host had, declares whatever a human wrote in `Depends`,
 * and installs cleanly on a machine that then fails at launch. The three ways
 * that happens are all invisible on the build machine:
 *
 * 1. A library the build host had and the baseline does not (the classic
 *    "works on my machine" release).
 * 2. A versioned symbol above the floor — `GLIBC_2.41` in a package certified
 *    for 24.04's 2.39. Installs, then dies in the loader.
 * 3. An `RPATH`/`RUNPATH` pointing at a build directory, which resolves on the
 *    builder and nowhere else.
 *
 * So the artifact closure is read from the ELF files themselves and checked
 * against `packaging/linux/runtime-policy.json`, and `Depends` is *derived*
 * from what survives rather than asserted. Parsing is separated from running
 * `readelf` so the rules are testable on macOS with recorded output.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

export interface RuntimePolicy {
  baseline: {
    distribution: string;
    kernel: string;
    maxGlibcVersion: string;
    maxGlibcxxVersion: string;
    maxCxxabiVersion: string;
  };
  architectures: Record<string, { rustTriple: string; debianArchitecture: string; elfMachine: string; interpreter: string }>;
  allowedRuntimeLibraries: Array<{ sonames: string[]; package: string; conditional?: boolean; reason: string }>;
  vendoredNotDeclared: Array<{ sonames: string[]; reason: string }>;
}

export const RUNTIME_POLICY_FILE = join("packaging", "linux", "runtime-policy.json");

export function readRuntimePolicy(repoRoot: string): RuntimePolicy {
  const path = join(repoRoot, RUNTIME_POLICY_FILE);
  const parsed = JSON.parse(readFileSync(path, "utf8")) as RuntimePolicy;
  if (!Array.isArray(parsed.allowedRuntimeLibraries) || parsed.allowedRuntimeLibraries.length === 0) {
    throw new Error(`${path} declares no allowed runtime libraries.`);
  }
  return parsed;
}

/** What one ELF file needs, as read out of its own headers. */
export interface ElfFacts {
  path: string;
  machine: string;
  interpreter: string | null;
  needed: string[];
  /** Versioned symbol requirements, `{ "GLIBC": ["2.17", "2.39"], ... }`. */
  versionRequirements: Record<string, string[]>;
  runpaths: string[];
}

/** `readelf -d -l -V -h` on one file. Static output, so the parser below can be
 *  exercised against recorded samples. */
export function readelfCommand(path: string): [string, string[]] {
  return ["readelf", ["--wide", "-h", "-l", "-d", "-V", path]];
}

const NEEDED = /\(NEEDED\)\s+Shared library: \[([^\]]+)\]/;
const RUNPATH = /\((?:RUNPATH|RPATH)\)\s+Library (?:runpath|rpath): \[([^\]]+)\]/;
const INTERP = /\[Requesting program interpreter: ([^\]]+)\]/;
const MACHINE = /^\s*Machine:\s+(.+?)\s*$/m;
const VERSION_NEED = /^\s*\S+:\s+Name:\s+([A-Za-z+_]+)_([0-9][0-9.]*)\s/;

export function parseReadelf(path: string, output: string): ElfFacts {
  const needed: string[] = [];
  const runpaths: string[] = [];
  const versionRequirements: Record<string, string[]> = {};
  let interpreter: string | null = null;

  for (const line of output.split("\n")) {
    const neededMatch = NEEDED.exec(line);
    if (neededMatch) needed.push(neededMatch[1]);
    const runpathMatch = RUNPATH.exec(line);
    // A runpath entry is colon-separated, and only one element has to be a
    // build path for the artifact to be unshippable.
    if (runpathMatch) runpaths.push(...runpathMatch[1].split(":").filter(Boolean));
    const interpMatch = INTERP.exec(line);
    if (interpMatch) interpreter = interpMatch[1];
    const versionMatch = VERSION_NEED.exec(line);
    if (versionMatch) {
      const [, family, version] = versionMatch;
      (versionRequirements[family] ??= []).push(version);
    }
  }

  const machineMatch = MACHINE.exec(output);
  return {
    path,
    machine: machineMatch ? machineMatch[1] : "",
    interpreter,
    needed: [...new Set(needed)].sort(),
    versionRequirements: Object.fromEntries(
      Object.entries(versionRequirements).map(([family, versions]) => [family, [...new Set(versions)].sort(compareVersions)])
    ),
    runpaths,
  };
}

/** Dotted numeric comparison. `2.9` must sort below `2.39`, which is exactly
 *  what a string comparison gets wrong and what a glibc floor depends on. */
export function compareVersions(left: string, right: string): number {
  const a = left.split(".").map(Number);
  const b = right.split(".").map(Number);
  for (let index = 0; index < Math.max(a.length, b.length); index += 1) {
    const difference = (a[index] ?? 0) - (b[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

export interface AuditFinding {
  path: string;
  kind:
    | "undeclared-library"
    | "vendored-library-linked-dynamically"
    | "version-above-floor"
    | "build-machine-path"
    | "wrong-architecture";
  detail: string;
}

export interface AuditResult {
  findings: AuditFinding[];
  /** The runtime packages the surviving closure needs — what `Depends` is built
   *  from. */
  requiredPackages: string[];
  /** Artifacts still linking a conditionally permitted library, kept visible so
   *  an exception cannot quietly become the norm. */
  conditionalUses: Array<{ path: string; soname: string; package: string }>;
}

export function auditArtifacts(
  policy: RuntimePolicy,
  architecture: string,
  artifacts: ElfFacts[]
): AuditResult {
  const target = policy.architectures[architecture];
  if (!target) {
    throw new Error(`Runtime policy declares no architecture ${JSON.stringify(architecture)}.`);
  }

  const allowed = new Map<string, { package: string; conditional: boolean }>();
  for (const entry of policy.allowedRuntimeLibraries) {
    for (const soname of entry.sonames) {
      allowed.set(soname, { package: entry.package, conditional: entry.conditional === true });
    }
  }
  const mustBeVendored = new Map<string, string>();
  for (const entry of policy.vendoredNotDeclared) {
    for (const soname of entry.sonames) mustBeVendored.set(soname, entry.reason);
  }

  const floors: Record<string, string> = {
    GLIBC: policy.baseline.maxGlibcVersion,
    GLIBCXX: policy.baseline.maxGlibcxxVersion,
    CXXABI: policy.baseline.maxCxxabiVersion,
  };

  const findings: AuditFinding[] = [];
  const requiredPackages = new Set<string>();
  const conditionalUses: AuditResult["conditionalUses"] = [];

  for (const artifact of artifacts) {
    if (artifact.machine && artifact.machine !== target.elfMachine) {
      findings.push({
        path: artifact.path,
        kind: "wrong-architecture",
        detail: `built for ${artifact.machine}, package is ${target.elfMachine}`,
      });
    }

    for (const soname of artifact.needed) {
      const vendoredReason = mustBeVendored.get(soname);
      if (vendoredReason) {
        findings.push({
          path: artifact.path,
          kind: "vendored-library-linked-dynamically",
          detail: `${soname} must be vendored, not linked: ${vendoredReason}`,
        });
        continue;
      }
      const entry = allowed.get(soname);
      if (!entry) {
        findings.push({
          path: artifact.path,
          kind: "undeclared-library",
          detail: `${soname} is not in the runtime policy's allowlist`,
        });
        continue;
      }
      requiredPackages.add(entry.package);
      if (entry.conditional) conditionalUses.push({ path: artifact.path, soname, package: entry.package });
    }

    for (const [family, versions] of Object.entries(artifact.versionRequirements)) {
      const floor = floors[family];
      if (!floor) continue;
      for (const version of versions) {
        if (compareVersions(version, floor) > 0) {
          findings.push({
            path: artifact.path,
            kind: "version-above-floor",
            detail: `needs ${family}_${version}, baseline allows ${family}_${floor}`,
          });
        }
      }
    }

    for (const runpath of artifact.runpaths) {
      // `$ORIGIN`-relative paths stay inside the installed tree; anything
      // absolute is a build-host path that will not exist on a user's machine.
      if (!runpath.startsWith("$ORIGIN")) {
        findings.push({
          path: artifact.path,
          kind: "build-machine-path",
          detail: `RUNPATH ${runpath} is not relative to the installed binary`,
        });
      }
    }
  }

  return {
    findings,
    requiredPackages: [...requiredPackages].sort(),
    conditionalUses,
  };
}

/**
 * `Depends`, derived from the audited closure.
 *
 * The glibc dependency carries the measured floor as a version constraint, so
 * apt refuses the install on an older distribution instead of letting the
 * loader fail after the package is unpacked.
 */
export function dependsFromAudit(policy: RuntimePolicy, audit: AuditResult): string[] {
  return audit.requiredPackages.map((name) =>
    name === "libc6" ? `libc6 (>= ${policy.baseline.maxGlibcVersion})` : name
  );
}

export function formatAuditReport(architecture: string, audit: AuditResult): string {
  const lines = [`Linux runtime audit — ${architecture}`];
  lines.push(`  runtime packages: ${audit.requiredPackages.join(", ") || "(none)"}`);
  if (audit.conditionalUses.length > 0) {
    lines.push("  conditional exceptions still in use:");
    for (const use of audit.conditionalUses) {
      lines.push(`    ${use.path} -> ${use.soname} (${use.package})`);
    }
  }
  if (audit.findings.length === 0) {
    lines.push("  no findings");
    return lines.join("\n");
  }
  lines.push(`  ${audit.findings.length} finding(s):`);
  for (const finding of audit.findings) {
    lines.push(`    [${finding.kind}] ${finding.path}: ${finding.detail}`);
  }
  return lines.join("\n");
}
