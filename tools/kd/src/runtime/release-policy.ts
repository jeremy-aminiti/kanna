import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

/**
 * The release lifecycle's tunable safety policy. Deliberately a file rather
 * than a constant in code: the soak window is a release-process decision the
 * repo owns, and an operator reading `kd release status` must be able to see
 * where the number came from.
 */
export interface ReleasePolicy {
  /**
   * Hours a staging release candidate must have been published before it may be
   * promoted to production. `0` disables the gate; there is no upper bound.
   */
  productionSoakHours: number;
  /** Linux's release requirements. Separate from macOS's because Linux ships on
   *  its own channel pair, so a Linux failure never freezes a macOS ship. */
  linux: LinuxReleasePolicy;
}

export type LinuxArchitectureName = "x86_64" | "arm64";

export interface LinuxReleasePolicy {
  /**
   * Architectures that must all build, audit and pass installed acceptance —
   * at the same version and source revision — before a Linux publish or
   * promotion. A missing required artifact fails the publish; it is never
   * silently omitted and never satisfied by an older build of the same
   * architecture.
   */
  requiredArchitectures: LinuxArchitectureName[];
  /** Built and published when available, but never a gate. */
  optionalArchitectures: LinuxArchitectureName[];
  /** Linux's own soak window, so a macOS policy change cannot silently retime
   *  a Linux promotion or vice versa. */
  productionSoakHours: number;
}

const LINUX_ARCHITECTURES: LinuxArchitectureName[] = ["x86_64", "arm64"];

export const RELEASE_POLICY_FILE = "release-policy.json";

export const DEFAULT_RELEASE_POLICY: ReleasePolicy = {
  productionSoakHours: 24,
  linux: {
    requiredArchitectures: ["x86_64", "arm64"],
    optionalArchitectures: [],
    productionSoakHours: 24
  }
};

const KNOWN_KEYS = new Set(["$schema", "productionSoakHours", "linux"]);
const KNOWN_LINUX_KEYS = new Set([
  "requiredArchitectures",
  "optionalArchitectures",
  "productionSoakHours"
]);

function parseArchitectureList(value: unknown, sourceLabel: string, key: string): LinuxArchitectureName[] {
  if (!Array.isArray(value)) {
    throw new Error(`${sourceLabel} linux.${key} must be an array of architecture names.`);
  }
  const names = value.map((entry) => {
    if (typeof entry !== "string" || !LINUX_ARCHITECTURES.includes(entry as LinuxArchitectureName)) {
      throw new Error(
        `${sourceLabel} linux.${key} has unknown architecture ${JSON.stringify(entry)}. ` +
          `Supported: ${LINUX_ARCHITECTURES.join(", ")}.`
      );
    }
    return entry as LinuxArchitectureName;
  });
  if (new Set(names).size !== names.length) {
    throw new Error(`${sourceLabel} linux.${key} lists an architecture twice.`);
  }
  return names;
}

function parseLinuxPolicy(raw: unknown, sourceLabel: string): LinuxReleasePolicy {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new Error(`${sourceLabel} linux must be a JSON object.`);
  }
  const record = raw as Record<string, unknown>;
  for (const key of Object.keys(record)) {
    if (!KNOWN_LINUX_KEYS.has(key)) {
      throw new Error(
        `${sourceLabel} has unknown key "linux.${key}". Supported keys: ${[...KNOWN_LINUX_KEYS].join(", ")}.`
      );
    }
  }
  const policy: LinuxReleasePolicy = { ...DEFAULT_RELEASE_POLICY.linux };
  if ("requiredArchitectures" in record) {
    policy.requiredArchitectures = parseArchitectureList(
      record.requiredArchitectures,
      sourceLabel,
      "requiredArchitectures"
    );
  }
  if ("optionalArchitectures" in record) {
    policy.optionalArchitectures = parseArchitectureList(
      record.optionalArchitectures,
      sourceLabel,
      "optionalArchitectures"
    );
  }
  // Disjoint, or "is this architecture a gate?" has two answers and the
  // publish check and the status report would disagree about the same build.
  const overlap = policy.requiredArchitectures.filter((name) => policy.optionalArchitectures.includes(name));
  if (overlap.length > 0) {
    throw new Error(
      `${sourceLabel} lists ${overlap.join(", ")} as both required and optional; an architecture is one or the other.`
    );
  }
  if (policy.requiredArchitectures.length === 0) {
    throw new Error(`${sourceLabel} linux.requiredArchitectures must name at least one architecture.`);
  }
  if ("productionSoakHours" in record) {
    const value = record.productionSoakHours;
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
      throw new Error(`${sourceLabel} linux.productionSoakHours must be a non-negative number of hours.`);
    }
    policy.productionSoakHours = value;
  }
  return policy;
}

export function parseReleasePolicy(raw: unknown, sourceLabel: string): ReleasePolicy {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new Error(`${sourceLabel} must contain a JSON object.`);
  }
  const record = raw as Record<string, unknown>;
  for (const key of Object.keys(record)) {
    if (!KNOWN_KEYS.has(key)) {
      throw new Error(
        `${sourceLabel} has unknown key ${JSON.stringify(key)}. Supported keys: productionSoakHours, linux.`
      );
    }
  }
  const policy: ReleasePolicy = {
    ...DEFAULT_RELEASE_POLICY,
    linux: { ...DEFAULT_RELEASE_POLICY.linux }
  };
  if ("linux" in record) {
    policy.linux = parseLinuxPolicy(record.linux, sourceLabel);
  }
  if ("productionSoakHours" in record) {
    const value = record.productionSoakHours;
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
      throw new Error(`${sourceLabel} productionSoakHours must be a non-negative number of hours.`);
    }
    policy.productionSoakHours = value;
  }
  return policy;
}

export function releasePolicyPath(repoRoot: string): string {
  return join(repoRoot, RELEASE_POLICY_FILE);
}

/**
 * Reads the repo's release policy. A missing file is the documented default —
 * a repo that never opts in still gets the standard soak gate — but a present
 * file that does not parse is an error, never a silent fallback.
 */
export function readReleasePolicy(repoRoot: string): ReleasePolicy {
  const path = releasePolicyPath(repoRoot);
  if (!existsSync(path)) return { ...DEFAULT_RELEASE_POLICY };
  let parsed: unknown;
  try {
    parsed = JSON.parse(readFileSync(path, "utf8")) as unknown;
  } catch (error) {
    throw new Error(`${path} is not valid JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  return parseReleasePolicy(parsed, path);
}

/**
 * The soak window that governs a promotion on this platform.
 *
 * Separate numbers rather than one shared value: the two channels soak
 * independently, so a Linux policy change must not silently retime a macOS
 * promotion.
 */
export function soakHoursForPlatform(policy: ReleasePolicy, platform: "macos" | "linux"): number {
  return platform === "linux" ? policy.linux.productionSoakHours : policy.productionSoakHours;
}

export interface ArchitectureSetProblem {
  missing: LinuxArchitectureName[];
  unexpected: string[];
}

/**
 * Is this set of built artifacts a complete Linux release?
 *
 * "Complete" is every required architecture present, at one version and one
 * source revision. The same-source check is not a formality: two artifacts
 * built from different commits would install as one release and behave as two,
 * and the mismatch would only surface as a bug report from whichever half of
 * the users got the other architecture.
 */
export function checkLinuxArchitectureSet(
  policy: ReleasePolicy,
  artifacts: Array<{ architecture: string; version: string; sourceRevision: string }>
): { ok: boolean; problems: string[] } {
  const problems: string[] = [];
  const present = new Set(artifacts.map((artifact) => artifact.architecture));

  const missing = policy.linux.requiredArchitectures.filter((name) => !present.has(name));
  if (missing.length > 0) {
    problems.push(
      `missing required architecture(s): ${missing.join(", ")}. A Linux publish needs every required artifact; ` +
        `it is never satisfied by the ones that did build.`
    );
  }

  const known = new Set<string>([...policy.linux.requiredArchitectures, ...policy.linux.optionalArchitectures]);
  const unexpected = [...present].filter((name) => !known.has(name));
  if (unexpected.length > 0) {
    problems.push(`artifact(s) for architecture(s) the policy does not declare: ${unexpected.join(", ")}.`);
  }

  const versions = new Set(artifacts.map((artifact) => artifact.version));
  if (versions.size > 1) {
    problems.push(`artifacts disagree on version: ${[...versions].sort().join(", ")}.`);
  }
  const revisions = new Set(artifacts.map((artifact) => artifact.sourceRevision));
  if (revisions.size > 1) {
    problems.push(`artifacts were built from different source revisions: ${[...revisions].sort().join(", ")}.`);
  }

  return { ok: problems.length === 0, problems };
}
