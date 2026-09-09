import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  auditArtifacts,
  compareVersions,
  dependsFromAudit,
  formatAuditReport,
  parseReadelf,
  readRuntimePolicy,
  type ElfFacts,
} from "../src/runtime/linux-elf-audit";

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");
const policy = readRuntimePolicy(repoRoot);

/**
 * A trimmed but real-shaped `readelf --wide -h -l -d -V` transcript. Recorded
 * output rather than a live run so the rules are exercised on macOS, where the
 * packaging work is written and where no ELF exists to read.
 */
const SAMPLE = `ELF Header:
  Class:                             ELF64
  Machine:                           AArch64
  Type:                              DYN (Position-Independent Executable file)

Program Headers:
      [Requesting program interpreter: /lib/ld-linux-aarch64.so.1]

Dynamic section at offset 0x1 contains 30 entries:
 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]
 0x0000000000000001 (NEEDED)             Shared library: [libm.so.6]
 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]
 0x000000000000001d (RUNPATH)            Library runpath: [$ORIGIN/../lib]

Version needs section '.gnu.version_r' contains 2 entries:
 0x0000: Version need aux entry 1
  0x0020:   Name: GLIBC_2.17  Flags: none  Version: 4
  0x0030:   Name: GLIBC_2.34  Flags: none  Version: 3
  0x0040:   Name: GLIBC_2.9  Flags: none  Version: 5
`;

function facts(overrides: Partial<ElfFacts> = {}): ElfFacts {
  return { ...parseReadelf("/usr/lib/kanna/kanna-daemon", SAMPLE), ...overrides };
}

describe("parseReadelf", () => {
  it("reads the machine, interpreter, needed set, runpath and version needs", () => {
    const parsed = parseReadelf("/usr/lib/kanna/kanna-daemon", SAMPLE);
    expect(parsed.machine).toBe("AArch64");
    expect(parsed.interpreter).toBe("/lib/ld-linux-aarch64.so.1");
    expect(parsed.needed).toEqual(["libc.so.6", "libgcc_s.so.1", "libm.so.6"]);
    expect(parsed.runpaths).toEqual(["$ORIGIN/../lib"]);
    expect(parsed.versionRequirements.GLIBC).toEqual(["2.9", "2.17", "2.34"]);
  });
});

/**
 * Numeric ordering, not lexical: `2.9` sorting above `2.39` would let a binary
 * needing `GLIBC_2.41` pass a 2.39 floor.
 */
describe("compareVersions", () => {
  it("orders dotted versions numerically", () => {
    expect(compareVersions("2.9", "2.39")).toBeLessThan(0);
    expect(compareVersions("2.41", "2.39")).toBeGreaterThan(0);
    expect(compareVersions("2.39", "2.39.0")).toBe(0);
  });
});

describe("auditArtifacts", () => {
  it("passes an artifact whose whole closure is in the policy", () => {
    const audit = auditArtifacts(policy, "arm64", [facts()]);
    expect(audit.findings).toEqual([]);
    expect(audit.requiredPackages).toEqual(["libc6", "libgcc-s1"]);
  });

  it("rejects a library nobody reviewed", () => {
    const audit = auditArtifacts(policy, "arm64", [facts({ needed: ["libc.so.6", "libfancy.so.2"] })]);
    expect(audit.findings).toEqual([
      expect.objectContaining({ kind: "undeclared-library", detail: expect.stringContaining("libfancy.so.2") }),
    ]);
  });

  /**
   * The one that catches a silently broken static link. `libc++` is supposed to
   * come from the pinned Zig toolchain statically; a base Ubuntu image has no
   * package for it, so a dynamic reference is a package that cannot start.
   */
  it("rejects a library that was supposed to be vendored", () => {
    const audit = auditArtifacts(policy, "arm64", [facts({ needed: ["libc++.so.1"] })]);
    expect(audit.findings[0]).toMatchObject({ kind: "vendored-library-linked-dynamically" });
  });

  it("rejects a versioned symbol above the baseline floor", () => {
    const audit = auditArtifacts(policy, "arm64", [
      facts({ versionRequirements: { GLIBC: ["2.41"], GLIBCXX: ["3.4.32"] } }),
    ]);
    expect(audit.findings).toEqual([
      expect.objectContaining({ kind: "version-above-floor", detail: expect.stringContaining("GLIBC_2.41") }),
    ]);
  });

  it("rejects a RUNPATH that only resolves on the build machine", () => {
    const audit = auditArtifacts(policy, "arm64", [facts({ runpaths: ["/home/builder/.build/release/deps"] })]);
    expect(audit.findings[0]).toMatchObject({ kind: "build-machine-path" });
  });

  it("rejects an artifact built for the other architecture", () => {
    const audit = auditArtifacts(policy, "x86_64", [facts()]);
    expect(audit.findings[0]).toMatchObject({ kind: "wrong-architecture" });
  });

  /**
   * OpenSSL is permitted only until the remaining `native-tls` consumers are
   * vendored. Recording which artifact still uses it is what keeps a temporary
   * exception from turning into the steady state unnoticed.
   */
  it("records conditional exceptions instead of hiding them", () => {
    const audit = auditArtifacts(policy, "arm64", [
      facts({ path: "/usr/lib/kanna/kanna-server", needed: ["libc.so.6", "libssl.so.3"] }),
    ]);
    expect(audit.findings).toEqual([]);
    expect(audit.conditionalUses).toEqual([
      { path: "/usr/lib/kanna/kanna-server", soname: "libssl.so.3", package: "libssl3t64" },
    ]);
    expect(formatAuditReport("arm64", audit)).toContain("conditional exceptions still in use");
  });

  it("refuses an architecture the policy does not declare", () => {
    expect(() => auditArtifacts(policy, "riscv64", [facts()])).toThrow(/riscv64/);
  });
});

/**
 * `Depends` is derived from the audited closure, never written by hand — and it
 * carries the glibc floor, so apt refuses an older distribution up front
 * instead of letting the loader fail after the package is unpacked.
 */
describe("dependsFromAudit", () => {
  it("constrains glibc to the measured baseline", () => {
    const audit = auditArtifacts(policy, "arm64", [facts()]);
    expect(dependsFromAudit(policy, audit)).toEqual([`libc6 (>= ${policy.baseline.maxGlibcVersion})`, "libgcc-s1"]);
  });
});

describe("the runtime policy file", () => {
  it("declares both launch architectures with their Debian names", () => {
    expect(policy.architectures.x86_64.debianArchitecture).toBe("amd64");
    expect(policy.architectures.arm64.debianArchitecture).toBe("arm64");
  });

  it("keeps SQLite and the Zig C++ runtime on the vendored side", () => {
    const vendored = policy.vendoredNotDeclared.flatMap((entry) => entry.sonames);
    expect(vendored).toContain("libsqlite3.so.0");
    expect(vendored).toContain("libc++.so.1");
  });

  it("gives every allowed library a supplying package and a reason", () => {
    for (const entry of policy.allowedRuntimeLibraries) {
      expect(entry.package).toBeTruthy();
      expect(entry.reason.length).toBeGreaterThan(20);
      expect(entry.sonames.length).toBeGreaterThan(0);
    }
  });
});
