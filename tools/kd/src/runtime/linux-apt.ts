/**
 * The signed apt archive that *is* the Linux update channel.
 *
 * apt does not verify a `.deb`. It verifies `InRelease` — a clearsigned index
 * — and follows a checksum chain from there: `InRelease` covers the `Packages`
 * index, `Packages` covers each package's SHA-256, and only then is the
 * downloaded file trusted. So there is no such thing as "signing the deb" on
 * this path, and every property that matters is a property of the *ordering*
 * in which the archive is written.
 *
 * That ordering is the whole design here:
 *
 * 1. Immutable pool artifacts go up first, at content-addressed paths nothing
 *    already published refers to. Uploading them changes what any client sees:
 *    nothing points at them yet.
 * 2. Indexes are built and every referenced artifact's checksum is verified
 *    against what was actually uploaded.
 * 3. `InRelease` is replaced last, in one write. That write is the commit
 *    point: before it, clients see the previous release entirely; after it,
 *    the new one entirely. There is no window in which a client can see an
 *    index referring to a package that is not there.
 *
 * An interrupted publish therefore leaves orphaned pool files and a valid
 * older archive, which is recoverable by re-running. The failure it makes
 * impossible is the one that matters: a partial archive that apt trusts.
 *
 * References: apt-secure(8) for what is authenticated, and Debian Policy
 * §5.6.12 for version ordering.
 */

import { createHash } from "node:crypto";

export type AptChannel = "desktop-linux-staging" | "desktop-linux";

/** The suite name inside the archive. One per channel, so a machine tracking
 *  one never sees the other's packages at all. */
export function aptSuite(channel: AptChannel): string {
  return channel === "desktop-linux" ? "stable" : "staging";
}

export const APT_COMPONENT = "main";

export interface AptArtifact {
  /** Debian architecture: `amd64` or `arm64`. */
  architecture: string;
  /** File name, exactly as it appears in the pool. */
  fileName: string;
  sizeBytes: number;
  sha256: string;
  /** The package's own control stanza, as `dpkg-deb --field` produced it. */
  controlFields: Record<string, string>;
}

/**
 * The pool path for an artifact.
 *
 * Immutable by construction: the path contains the version, so republishing a
 * changed build under a version that already shipped would collide rather than
 * silently replace what users already fetched.
 */
export function poolPath(artifact: AptArtifact): string {
  const name = artifact.controlFields.Package ?? "kanna";
  return `pool/${APT_COMPONENT}/${name[0]}/${name}/${artifact.fileName}`;
}

export function packagesIndexPath(channel: AptChannel, architecture: string): string {
  return `dists/${aptSuite(channel)}/${APT_COMPONENT}/binary-${architecture}/Packages`;
}

export function releaseIndexPath(channel: AptChannel): string {
  return `dists/${aptSuite(channel)}/Release`;
}

export function inReleasePath(channel: AptChannel): string {
  return `dists/${aptSuite(channel)}/InRelease`;
}

/**
 * One architecture's `Packages` index.
 *
 * `Filename` is archive-root-relative, which is what lets one archive serve
 * both channels' suites from the same pool without a client ever resolving a
 * path outside the suite it subscribed to.
 */
export function buildPackagesIndex(artifacts: AptArtifact[]): string {
  return (
    artifacts
      .map((artifact) => {
        const fields = { ...artifact.controlFields };
        // Order follows Debian's convention: Package first, then the fields a
        // client needs to decide before it downloads anything.
        const head = ["Package", "Version", "Architecture"]
          .map((key) => `${key}: ${fields[key]}`)
          .join("\n");
        const rest = Object.entries(fields)
          .filter(([key]) => !["Package", "Version", "Architecture"].includes(key))
          .map(([key, value]) => `${key}: ${value}`)
          .join("\n");
        return [
          head,
          rest,
          `Filename: ${poolPath(artifact)}`,
          `Size: ${artifact.sizeBytes}`,
          `SHA256: ${artifact.sha256}`,
        ]
          .filter((section) => section.length > 0)
          .join("\n");
      })
      .join("\n\n") + "\n"
  );
}

export interface ReleaseIndexInput {
  channel: AptChannel;
  architectures: string[];
  /** Publication time. Explicit rather than `now`, so the same inputs produce
   *  the same index and a republish is a comparable artifact. */
  date: Date;
  /** How long a client may keep trusting this index. An archive that never
   *  expires cannot be rolled back by simply stopping publication, so a
   *  compromised or abandoned channel would be trusted forever. */
  validForHours: number;
  origin?: string;
  /** `path -> contents` for every index this Release covers. */
  indexes: Record<string, string>;
}

/**
 * The `Release` index, which is the document that gets signed.
 *
 * `Acquire-By-Hash` is on: a client fetching an index by its hash cannot race a
 * publish that replaced the index between its `Release` fetch and its index
 * fetch — the classic "hash sum mismatch" a mid-publish client hits.
 */
export function buildReleaseIndex(input: ReleaseIndexInput): string {
  const validUntil = new Date(input.date.getTime() + input.validForHours * 3_600_000);
  const lines = [
    `Origin: ${input.origin ?? "Kanna"}`,
    `Label: ${input.origin ?? "Kanna"}`,
    `Suite: ${aptSuite(input.channel)}`,
    `Codename: ${aptSuite(input.channel)}`,
    `Architectures: ${[...input.architectures].sort().join(" ")}`,
    `Components: ${APT_COMPONENT}`,
    `Date: ${input.date.toUTCString()}`,
    `Valid-Until: ${validUntil.toUTCString()}`,
    "Acquire-By-Hash: yes",
    `Description: Kanna ${aptSuite(input.channel)} channel`,
  ];
  for (const [algorithm, digest] of [
    ["MD5Sum", "md5"],
    ["SHA256", "sha256"],
  ] as const) {
    lines.push(`${algorithm}:`);
    for (const path of Object.keys(input.indexes).sort()) {
      const contents = input.indexes[path] as string;
      const bytes = Buffer.from(contents, "utf8");
      const suitePrefix = `dists/${aptSuite(input.channel)}/`;
      const relative = path.startsWith(suitePrefix) ? path.slice(suitePrefix.length) : path;
      lines.push(` ${createHash(digest).update(bytes).digest("hex")} ${bytes.byteLength} ${relative}`);
    }
  }
  return lines.join("\n") + "\n";
}

/** The `gpg` invocation that turns a `Release` into a clearsigned `InRelease`.
 *  The key is named, never defaulted: signing with whatever key happens to be
 *  first in a keyring is how a staging key ends up on a production archive. */
export function signInReleaseCommand(input: {
  releasePath: string;
  outputPath: string;
  keyFingerprint: string;
  homeDir?: string;
}): [string, string[]] {
  return [
    "gpg",
    [
      ...(input.homeDir ? ["--homedir", input.homeDir] : []),
      "--batch",
      "--yes",
      "--local-user",
      input.keyFingerprint,
      "--clearsign",
      "--digest-algo",
      "SHA512",
      "--output",
      input.outputPath,
      input.releasePath,
    ],
  ];
}

export interface PublishStep {
  /** `data` steps are safe to repeat and safe to interrupt: nothing points at
   *  what they write until the commit step. `commit` is the single write that
   *  makes the new archive current. */
  kind: "data" | "index" | "commit";
  path: string;
  reason: string;
}

/**
 * The publish order, as a plan rather than as control flow.
 *
 * Returned so it can be asserted: the property "the commit is last and there is
 * exactly one" is the archive's whole atomicity argument, and it should fail a
 * unit test rather than a user's `apt update`.
 */
export function planPublish(input: {
  channel: AptChannel;
  artifacts: AptArtifact[];
  architectures: string[];
}): PublishStep[] {
  const steps: PublishStep[] = input.artifacts.map((artifact) => ({
    kind: "data",
    path: poolPath(artifact),
    reason: "immutable pool artifact; nothing refers to it yet",
  }));
  for (const architecture of [...input.architectures].sort()) {
    steps.push({
      kind: "index",
      path: packagesIndexPath(input.channel, architecture),
      reason: "package index; not trusted until Release covers it",
    });
  }
  steps.push({
    kind: "index",
    path: releaseIndexPath(input.channel),
    reason: "unsigned index; apt ignores it without a signature",
  });
  steps.push({
    kind: "commit",
    path: inReleasePath(input.channel),
    reason: "the commit point: clients see the previous archive entirely before this write and the new one entirely after",
  });
  return steps;
}

export interface ArtifactVerification {
  ok: boolean;
  problems: string[];
}

/**
 * Everything the publisher must be sure of before it writes `InRelease`.
 *
 * Checked against what was actually uploaded, not against what the build
 * claimed: the index is about to tell every client that a file with this
 * checksum is at this path, and the signature is about to make that claim
 * trusted.
 */
export function verifyBeforeCommit(input: {
  artifacts: AptArtifact[];
  /** `poolPath -> sha256`, read back from the storage that now holds them. */
  uploaded: Record<string, { sha256: string; sizeBytes: number }>;
  requiredArchitectures: string[];
}): ArtifactVerification {
  const problems: string[] = [];

  for (const artifact of input.artifacts) {
    const path = poolPath(artifact);
    const stored = input.uploaded[path];
    if (!stored) {
      problems.push(`${path} was not uploaded, but the index would advertise it.`);
      continue;
    }
    if (stored.sha256 !== artifact.sha256) {
      problems.push(
        `${path} has checksum ${stored.sha256} in storage but ${artifact.sha256} in the index; ` +
          `signing this would authenticate a file nobody built.`
      );
    }
    if (stored.sizeBytes !== artifact.sizeBytes) {
      problems.push(`${path} is ${stored.sizeBytes} bytes in storage and ${artifact.sizeBytes} in the index.`);
    }
  }

  const present = new Set(input.artifacts.map((artifact) => artifact.architecture));
  const missing = input.requiredArchitectures.filter((architecture) => !present.has(architecture));
  if (missing.length > 0) {
    problems.push(
      `the publication is missing required architecture(s) ${missing.join(", ")}; ` +
        `publishing would advertise a release half its users cannot install.`
    );
  }

  const versions = new Set(input.artifacts.map((artifact) => artifact.controlFields.Version));
  if (versions.size > 1) {
    problems.push(`artifacts disagree on version: ${[...versions].sort().join(", ")}.`);
  }

  return { ok: problems.length === 0, problems };
}

/**
 * The `sources.list` entry a user adds, and the one thing about it that is not
 * cosmetic: `signed-by`.
 *
 * A key added to apt's global trust (the retired `apt-key`) can sign *any*
 * repository the machine has configured. Scoping the key to this one source is
 * what keeps a Kanna key from being able to authenticate a package claiming to
 * be anything else.
 */
export function sourcesListEntry(input: {
  channel: AptChannel;
  baseUrl: string;
  keyringPath: string;
  architectures: string[];
}): string {
  return (
    [
      "Types: deb",
      `URIs: ${input.baseUrl.replace(/\/+$/, "")}`,
      `Suites: ${aptSuite(input.channel)}`,
      `Components: ${APT_COMPONENT}`,
      `Architectures: ${[...input.architectures].sort().join(" ")}`,
      `Signed-By: ${input.keyringPath}`,
    ].join("\n") + "\n"
  );
}
