import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  DEFAULT_RELEASE_PLATFORM,
  parseReleasePlatform,
  releasePlatform,
  tagBelongsToPlatform,
} from "../src/runtime/release-platform";
import {
  DEFAULT_RELEASE_POLICY,
  checkLinuxArchitectureSet,
  parseReleasePolicy,
  soakHoursForPlatform,
} from "../src/runtime/release-policy";

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");

describe("release platforms", () => {
  /**
   * The reason the channels are split at all: a Linux build failure must never
   * be able to freeze, advance, prune or satisfy a macOS ship, or the other way
   * round. Nothing they name may collide.
   */
  it("shares no channel, tag or branch between the platforms", () => {
    const macos = releasePlatform("macos");
    const linux = releasePlatform("linux");
    expect(macos.stagingChannelTag).not.toBe(linux.stagingChannelTag);
    expect(macos.productionChannelTag).not.toBe(linux.productionChannelTag);
    expect(macos.stagingTag("1.2.3", 4)).not.toBe(linux.stagingTag("1.2.3", 4));
    expect(macos.productionTag("1.2.3")).not.toBe(linux.productionTag("1.2.3"));
    expect(macos.seriesBranch("1.2.3")).not.toBe(linux.seriesBranch("1.2.3"));
    expect(macos.signingKey).not.toBe(linux.signingKey);
  });

  /**
   * A bare `vX.Y.Z` on this repository is the macOS release, and GitHub's own
   * "latest release" resolves from those. A Linux tag must never become the
   * thing the existing macOS updater endpoint follows.
   */
  it("keeps each platform's tags invisible to the other's lookups", () => {
    const macos = releasePlatform("macos");
    const linux = releasePlatform("linux");
    expect(tagBelongsToPlatform(macos, "v1.2.3")).toBe(true);
    expect(tagBelongsToPlatform(macos, "linux-v1.2.3")).toBe(false);
    expect(tagBelongsToPlatform(linux, "linux-v1.2.3")).toBe(true);
    expect(tagBelongsToPlatform(linux, "v1.2.3")).toBe(false);
    expect(linux.productionTag("1.2.3").startsWith("v")).toBe(false);
  });

  it("recognises only its own series branches", () => {
    expect(releasePlatform("macos").isSeriesBranch("release/1.2")).toBe(true);
    expect(releasePlatform("macos").isSeriesBranch("release/linux/1.2")).toBe(false);
    expect(releasePlatform("linux").isSeriesBranch("release/linux/1.2")).toBe(true);
    expect(releasePlatform("linux").isSeriesBranch("release/1.2")).toBe(false);
  });

  /**
   * The macOS descriptor has to reproduce the constants the existing release
   * engine already uses, or introducing the descriptor would silently repoint
   * production.
   */
  it("reproduces the macOS channel names the release engine already uses", () => {
    const release = readFileSync(resolve(repoRoot, "tools/kd/src/runtime/release.ts"), "utf8");
    const macos = releasePlatform("macos");
    expect(release).toContain(`const STAGING_CHANNEL_TAG = "${macos.stagingChannelTag}";`);
    expect(release).toContain(`const STAGING_MANIFEST_NAME = "${macos.manifestName}";`);
  });

  /**
   * `kd release` acts on shared remote channel state, not on this machine. A
   * host-derived default would mean the same command did different things to
   * production depending on who typed it.
   */
  it("defaults to macOS regardless of the host", () => {
    expect(DEFAULT_RELEASE_PLATFORM).toBe("macos");
    expect(parseReleasePlatform(undefined)).toBe("macos");
    expect(parseReleasePlatform("linux")).toBe("linux");
    expect(() => parseReleasePlatform("windows")).toThrow(/macos or linux/);
  });
});

describe("the Linux release policy", () => {
  it("requires both launch architectures by default", () => {
    expect(DEFAULT_RELEASE_POLICY.linux.requiredArchitectures).toEqual(["x86_64", "arm64"]);
    expect(DEFAULT_RELEASE_POLICY.linux.optionalArchitectures).toEqual([]);
  });

  it("is the repository's committed policy too", () => {
    const policy = parseReleasePolicy(
      JSON.parse(readFileSync(resolve(repoRoot, "release-policy.json"), "utf8")),
      "release-policy.json"
    );
    expect(policy.linux.requiredArchitectures).toEqual(["x86_64", "arm64"]);
  });

  /** Each platform's soak is its own number, so a change to one cannot
   *  silently retime the other's promotion. */
  it("soaks the two platforms independently", () => {
    const policy = parseReleasePolicy(
      { productionSoakHours: 48, linux: { productionSoakHours: 12 } },
      "test"
    );
    expect(soakHoursForPlatform(policy, "macos")).toBe(48);
    expect(soakHoursForPlatform(policy, "linux")).toBe(12);
  });

  it("rejects an architecture that is both required and optional", () => {
    expect(() =>
      parseReleasePolicy(
        { linux: { requiredArchitectures: ["x86_64"], optionalArchitectures: ["x86_64"] } },
        "test"
      )
    ).toThrow(/both required and optional/);
  });

  it("rejects unknown keys and unknown architectures rather than ignoring them", () => {
    expect(() => parseReleasePolicy({ linux: { nope: 1 } }, "test")).toThrow(/linux.nope/);
    expect(() => parseReleasePolicy({ linux: { requiredArchitectures: ["ppc64"] } }, "test")).toThrow(/ppc64/);
    expect(() => parseReleasePolicy({ linux: { requiredArchitectures: [] } }, "test")).toThrow(/at least one/);
  });
});

describe("checkLinuxArchitectureSet", () => {
  const policy = DEFAULT_RELEASE_POLICY;
  const good = [
    { architecture: "x86_64", version: "1.2.3", sourceRevision: "abc" },
    { architecture: "arm64", version: "1.2.3", sourceRevision: "abc" },
  ];

  it("accepts a complete set at one version and one revision", () => {
    expect(checkLinuxArchitectureSet(policy, good)).toEqual({ ok: true, problems: [] });
  });

  /** The build that half-succeeded is the dangerous one: it looks like a
   *  release and installs for half the users. */
  it("refuses a set missing a required architecture", () => {
    const result = checkLinuxArchitectureSet(policy, [good[0] as (typeof good)[number]]);
    expect(result.ok).toBe(false);
    expect(result.problems.join(" ")).toMatch(/missing required architecture\(s\): arm64/);
  });

  /**
   * Two artifacts from different commits would install as one release and
   * behave as two, and the mismatch would only surface as a bug report from
   * whichever half of the users got the other architecture.
   */
  it("refuses artifacts built from different source revisions or versions", () => {
    expect(
      checkLinuxArchitectureSet(policy, [
        good[0] as (typeof good)[number],
        { architecture: "arm64", version: "1.2.3", sourceRevision: "def" },
      ]).problems.join(" ")
    ).toMatch(/different source revisions/);
    expect(
      checkLinuxArchitectureSet(policy, [
        good[0] as (typeof good)[number],
        { architecture: "arm64", version: "1.2.4", sourceRevision: "abc" },
      ]).problems.join(" ")
    ).toMatch(/disagree on version/);
  });

  it("accepts an optional architecture and does not require it", () => {
    const optional = parseReleasePolicy(
      { linux: { requiredArchitectures: ["x86_64"], optionalArchitectures: ["arm64"] } },
      "test"
    );
    expect(checkLinuxArchitectureSet(optional, good).ok).toBe(true);
    expect(checkLinuxArchitectureSet(optional, [good[0] as (typeof good)[number]]).ok).toBe(true);
  });
});
