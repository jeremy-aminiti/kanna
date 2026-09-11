import { describe, expect, it } from "vitest";
import {
  aptSuite,
  buildPackagesIndex,
  buildReleaseIndex,
  inReleasePath,
  packagesByHashPath,
  packagesIndexPath,
  planPublish,
  poolPath,
  sourcesListEntry,
  verifyBeforeCommit,
  type AptArtifact,
} from "../src/runtime/linux-apt";

function artifact(overrides: Partial<AptArtifact> = {}): AptArtifact {
  return {
    architecture: "amd64",
    fileName: "kanna_1.2.3-1_amd64.deb",
    sizeBytes: 4096,
    sha256: "a".repeat(64),
    controlFields: {
      Package: "kanna",
      Version: "1.2.3-1",
      Architecture: "amd64",
      Depends: "libc6 (>= 2.39)",
    },
    ...overrides,
  };
}

describe("archive layout", () => {
  it("keeps the two channels in separate suites", () => {
    expect(aptSuite("desktop-linux")).toBe("stable");
    expect(aptSuite("desktop-linux-staging")).toBe("staging");
    expect(packagesIndexPath("desktop-linux", "amd64")).not.toBe(packagesIndexPath("desktop-linux-staging", "amd64"));
  });

  /**
   * The pool path carries the version, so republishing changed bytes under a
   * version users already fetched collides rather than silently replacing what
   * they have.
   */
  it("addresses pool artifacts immutably", () => {
    expect(poolPath(artifact())).toBe("pool/main/k/kanna/kanna_1.2.3-1_amd64.deb");
    expect(poolPath(artifact({ fileName: "kanna_1.2.4-1_amd64.deb" }))).not.toBe(poolPath(artifact()));
  });
});

describe("Packages index", () => {
  it("uses computed archive metadata instead of stale control fields", () => {
    const item = artifact();
    item.controlFields.Filename = "../wrong.deb";
    item.controlFields.Size = "1";
    item.controlFields.sha256 = "stale";
    const index = buildPackagesIndex([item]);
    expect(index).not.toMatch(/wrong\.deb|stale|Size: 1\n/);
    expect(index.match(/^Filename:/gm)).toHaveLength(1);
    expect(index.match(/^Size:/gm)).toHaveLength(1);
    expect(index.match(/^SHA256:/gm)).toHaveLength(1);
  });

  it("carries the checksum and size a client checks before trusting a download", () => {
    const index = buildPackagesIndex([artifact()]);
    expect(index).toContain("Package: kanna");
    expect(index).toContain(`SHA256: ${"a".repeat(64)}`);
    expect(index).toContain("Size: 4096");
    expect(index).toContain("Filename: pool/main/k/kanna/kanna_1.2.3-1_amd64.deb");
    // Stanzas are blank-line separated and the file ends with a newline, or
    // apt silently drops the last entry.
    expect(index.endsWith("\n")).toBe(true);
  });
});

describe("Release index", () => {
  const indexes = { "dists/staging/main/binary-amd64/Packages": "Package: kanna\n" };

  it("covers each index by hash and size", () => {
    const release = buildReleaseIndex({
      channel: "desktop-linux-staging",
      architectures: ["arm64", "amd64"],
      date: new Date("2026-09-09T00:00:00Z"),
      validForHours: 168,
      indexes,
    });
    expect(release).toContain("Suite: staging");
    expect(release).toContain("Architectures: amd64 arm64");
    expect(release).toMatch(/SHA256:\n [0-9a-f]{64} 15 main\/binary-amd64\/Packages/);
  });

  /**
   * Without `Valid-Until` an abandoned or compromised channel stays trusted
   * forever: stopping publication would not stop clients from accepting the
   * last thing they saw.
   */
  it("expires", () => {
    const release = buildReleaseIndex({
      channel: "desktop-linux",
      architectures: ["amd64"],
      date: new Date("2026-09-09T00:00:00Z"),
      validForHours: 24,
      indexes,
    });
    expect(release).toContain("Date: Wed, 09 Sep 2026 00:00:00 GMT");
    expect(release).toContain("Valid-Until: Thu, 10 Sep 2026 00:00:00 GMT");
  });

  /**
   * `Acquire-By-Hash` is what stops a client that fetched `Release` just before
   * a publish from hitting a hash-sum mismatch on the index it then fetches.
   */
  it("offers by-hash index fetches", () => {
    expect(
      buildReleaseIndex({
        channel: "desktop-linux",
        architectures: ["amd64"],
        date: new Date(),
        validForHours: 24,
        indexes,
      })
    ).toContain("Acquire-By-Hash: yes");
  });
});

describe("the publish plan", () => {
  const steps = planPublish({
    channel: "desktop-linux-staging",
    artifacts: [artifact(), artifact({ architecture: "arm64", fileName: "kanna_1.2.3-1_arm64.deb" })],
    architectures: ["amd64", "arm64"],
  });

  /**
   * The archive's entire atomicity argument, asserted rather than assumed: one
   * commit, and it is last. Anything written after `InRelease` would be visible
   * to clients that already trust the new index.
   */
  it("has exactly one commit step, at the end", () => {
    const commits = steps.filter((step) => step.kind === "commit");
    expect(commits).toHaveLength(1);
    expect(steps[steps.length - 1]).toBe(commits[0]);
    expect(commits[0]?.path).toBe(inReleasePath("desktop-linux-staging"));
  });

  it("uploads every pool artifact before any index", () => {
    const firstIndex = steps.findIndex((step) => step.kind !== "data");
    expect(steps.slice(0, firstIndex).every((step) => step.kind === "data")).toBe(true);
    expect(firstIndex).toBe(2);
  });

  it("creates an immutable by-hash index before replacing each canonical alias", () => {
    const item = artifact();
    const path = packagesByHashPath("desktop-linux-staging", "amd64", buildPackagesIndex([item]));
    expect(path).toMatch(/^dists\/staging\/main\/binary-amd64\/by-hash\/SHA256\/[0-9a-f]{64}$/);
    const position = steps.findIndex((step) => step.path === path);
    expect(position).toBeGreaterThan(-1);
    expect(steps[position]?.write).toBe("immutable");
    expect(steps[position + 1]?.path).toBe(packagesIndexPath("desktop-linux-staging", "amd64"));
    expect(steps[position + 1]?.write).toBe("replace");
  });
});

describe("verifyBeforeCommit", () => {
  const uploaded = {
    [poolPath(artifact())]: { sha256: "a".repeat(64), sizeBytes: 4096 },
  };

  it("accepts a complete, matching publication", () => {
    expect(
      verifyBeforeCommit({ artifacts: [artifact()], uploaded, requiredArchitectures: ["amd64"] })
    ).toEqual({ ok: true, problems: [] });
  });

  /**
   * Signing is what makes the index's claim trusted, so the claim is checked
   * against storage first. A mismatch here would authenticate a file nobody
   * built.
   */
  it("refuses when storage and index disagree about the bytes", () => {
    const result = verifyBeforeCommit({
      artifacts: [artifact()],
      uploaded: { [poolPath(artifact())]: { sha256: "b".repeat(64), sizeBytes: 4096 } },
      requiredArchitectures: ["amd64"],
    });
    expect(result.ok).toBe(false);
    expect(result.problems[0]).toMatch(/authenticate a file nobody built/);
  });

  it("refuses a publication missing an artifact it would advertise", () => {
    const result = verifyBeforeCommit({ artifacts: [artifact()], uploaded: {}, requiredArchitectures: ["amd64"] });
    expect(result.ok).toBe(false);
    expect(result.problems[0]).toMatch(/was not uploaded/);
  });

  /**
   * Half a release is worse than none: apt would offer an upgrade that a whole
   * architecture's users cannot install.
   */
  it("refuses a publication missing a required architecture", () => {
    const result = verifyBeforeCommit({
      artifacts: [artifact()],
      uploaded,
      requiredArchitectures: ["amd64", "arm64"],
    });
    expect(result.ok).toBe(false);
    expect(result.problems.join(" ")).toMatch(/missing required architecture\(s\) arm64/);
  });

  it("refuses artifacts that disagree on version", () => {
    const other = artifact({
      architecture: "arm64",
      fileName: "kanna_1.2.4-1_arm64.deb",
      controlFields: { Package: "kanna", Version: "1.2.4-1", Architecture: "arm64" },
    });
    const result = verifyBeforeCommit({
      artifacts: [artifact(), other],
      uploaded: { ...uploaded, [poolPath(other)]: { sha256: other.sha256, sizeBytes: other.sizeBytes } },
      requiredArchitectures: ["amd64", "arm64"],
    });
    expect(result.ok).toBe(false);
    expect(result.problems.join(" ")).toMatch(/disagree on version/);
  });
});

describe("the sources entry a user adds", () => {
  /**
   * `Signed-By` scopes the key to this repository. A key in apt's global trust
   * could authenticate a package claiming to be anything on the machine, which
   * is why `apt-key` was retired and why `trusted=yes` is never acceptable.
   */
  it("scopes the key to this repository and nothing else", () => {
    const entry = sourcesListEntry({
      channel: "desktop-linux",
      baseUrl: "https://apt.kanna.build/",
      keyringPath: "/usr/share/keyrings/kanna-archive-keyring.gpg",
      architectures: ["amd64", "arm64"],
    });
    expect(entry).toContain("Signed-By: /usr/share/keyrings/kanna-archive-keyring.gpg");
    expect(entry).toContain("Suites: stable");
    expect(entry).not.toMatch(/trusted=yes|allow-unauthenticated/);
    expect(entry).toContain("URIs: https://apt.kanna.build");
  });
});
