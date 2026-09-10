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
   * The one that catches a silently broken bundling decision. The server owns
   * SQLite's schema and its migrations, so a distribution-supplied engine would
   * put the database's behaviour outside Kanna's control — a dynamic reference
   * means the bundling stopped happening.
   */
  it("rejects a library that was supposed to be vendored", () => {
    const audit = auditArtifacts(policy, "arm64", [facts({ needed: ["libsqlite3.so.0"] })]);
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

/**
 * `Depends: a | b` is Debian alternation — *either* package satisfies it. Two
 * distinct runtime libraries need two entries, or a machine that installed only
 * one would satisfy apt and then fail in the loader on the other soname.
 */
describe("dependsFromAudit for artifacts needing two libraries from one exception", () => {
  it("yields one package per soname, with no alternation", () => {
    const audit = auditArtifacts(policy, "arm64", [
      facts({
        path: "/usr/lib/kanna/kanna-daemon",
        needed: ["libc.so.6", "libc++.so.1", "libc++abi.so.1"],
      }),
    ]);
    expect(audit.findings).toEqual([]);
    const depends = dependsFromAudit(policy, audit);
    expect(depends).toContain("libc++1");
    expect(depends).toContain("libc++abi1");
    for (const entry of depends) {
      expect(entry, `${entry} uses Debian alternation`).not.toContain("|");
    }
    // Both are reported as still-live exceptions, individually.
    expect(audit.conditionalUses.map((use) => use.package).sort()).toEqual(["libc++1", "libc++abi1"]);
  });

  it("declares no allowlist package as an alternation", () => {
    for (const entry of policy.allowedRuntimeLibraries) {
      expect(entry.package, `${entry.sonames.join(", ")} declares an alternation`).not.toContain("|");
    }
  });
});

describe("the runtime policy file", () => {
  it("declares both launch architectures with their Debian names", () => {
    expect(policy.architectures.x86_64.debianArchitecture).toBe("amd64");
    expect(policy.architectures.arm64.debianArchitecture).toBe("arm64");
  });

  it("keeps SQLite on the vendored side", () => {
    expect(policy.vendoredNotDeclared.flatMap((entry) => entry.sonames)).toContain("libsqlite3.so.0");
  });

  /**
   * Both exceptions are temporary by construction, so both must carry the
   * evidence and the follow-up that would retire them. An entry that only said
   * "allowed" would become permanent by forgetting.
   */
  it("makes every conditional exception explain itself", () => {
    const conditional = policy.allowedRuntimeLibraries.filter((entry) => entry.conditional === true);
    expect(conditional.length).toBeGreaterThan(0);
    for (const entry of conditional) {
      expect(entry.reason).toMatch(/vendor|static|follow-up/i);
    }
  });

  it("gives every allowed library a supplying package and a reason", () => {
    for (const entry of policy.allowedRuntimeLibraries) {
      expect(entry.package).toBeTruthy();
      expect(entry.reason.length).toBeGreaterThan(20);
      expect(entry.sonames.length).toBeGreaterThan(0);
    }
  });
});

/**
 * The real closure, measured on a Linux build rather than imagined.
 *
 * The libc++/libc++abi/libgcc_s/libm/libc set for the five non-GTK binaries was
 * first taken from the Phase 2 aarch64 binaries on 2026-09-09 with
 * `readelf --wide -d`. `libunwind.so.1` and the `kanna-worker` row were added
 * 2026-09-10 from CI run 34439249468's real `readelf`-backed audit on Ubuntu
 * 24.04 (noble), both amd64 and arm64: installing `libc++abi-dev` there pulls
 * in `libunwind-18` as a transitive dependency of the distro's own
 * `libc++abi.so.1`, so the five binaries that already link libc++/libc++abi
 * link this too. Pinning it here is what stops the policy from drifting into a
 * description of what somebody assumed: a new dependency appearing in a Kanna
 * binary fails this test on any developer's machine, months before it would
 * fail a user's launch.
 */
describe("the measured artifact closure", () => {
  const MEASURED: Record<string, string[]> = {
    "kanna-cli": ["libssl.so.3", "libcrypto.so.3", "libgcc_s.so.1", "libc.so.6"],
    "kanna-daemon": [
      "libc++.so.1", "libc++abi.so.1", "libunwind.so.1", "libgcc_s.so.1", "libm.so.6", "libc.so.6",
    ],
    "kanna-mcp": ["libssl.so.3", "libcrypto.so.3", "libgcc_s.so.1", "libc.so.6"],
    "kanna-server": [
      "libc++.so.1", "libc++abi.so.1", "libunwind.so.1", "libssl.so.3", "libcrypto.so.3",
      "libgcc_s.so.1", "libm.so.6", "libc.so.6",
    ],
    "kanna-task-transfer": [
      "libc++.so.1", "libc++abi.so.1", "libunwind.so.1", "libgcc_s.so.1", "libm.so.6", "libc.so.6",
    ],
    "kanna-terminal-recovery": [
      "libc++.so.1", "libc++abi.so.1", "libunwind.so.1", "libgcc_s.so.1", "libc.so.6",
    ],
    "kanna-worker": [
      "libc++.so.1", "libc++abi.so.1", "libunwind.so.1", "libgcc_s.so.1", "libm.so.6", "libc.so.6",
    ],
    "kanna-desktop": [
      "libgio-2.0.so.0", "libgobject-2.0.so.0", "libglib-2.0.so.0", "libz.so.1",
      "libgdk-3.so.0", "libpango-1.0.so.0", "libgdk_pixbuf-2.0.so.0",
      "libcairo-gobject.so.2", "libcairo.so.2", "libwebkit2gtk-4.1.so.0",
      "libgtk-3.so.0", "libsoup-3.0.so.0", "libjavascriptcoregtk-4.1.so.0",
      "libgcc_s.so.1", "libm.so.6", "libc.so.6", "ld-linux-aarch64.so.1",
    ],
  };

  const FIVE_CXX_CONSUMERS = [
    "kanna-daemon", "kanna-server", "kanna-task-transfer", "kanna-terminal-recovery", "kanna-worker",
  ];

  // Both shipping architectures, taken from the policy itself rather than
  // repeated here, so the test can't silently drift from what the policy
  // actually declares.
  const LAUNCH_ARCHITECTURES = Object.entries(policy.architectures).map(([name, target]) => ({
    name,
    machine: target.elfMachine,
    interpreter: target.interpreter,
    loaderSoname: target.interpreter.split("/").pop()!,
  }));

  /** `kanna-desktop`'s NEEDED set includes the dynamic loader itself; the
   *  soname that appears there is architecture-specific. */
  function measuredFor(loaderSoname: string): Record<string, string[]> {
    return {
      ...MEASURED,
      "kanna-desktop": MEASURED["kanna-desktop"].map((soname) =>
        soname.startsWith("ld-linux-") ? loaderSoname : soname
      ),
    };
  }

  for (const arch of LAUNCH_ARCHITECTURES) {
    const measured = measuredFor(arch.loaderSoname);

    it(`is entirely covered by the policy (${arch.name})`, () => {
      const artifacts: ElfFacts[] = Object.entries(measured).map(([name, needed]) => ({
        path: `/usr/lib/kanna/${name}`,
        machine: arch.machine,
        interpreter: arch.interpreter,
        needed,
        versionRequirements: { GLIBC: ["2.17", "2.39"] },
        runpaths: [],
      }));
      const audit = auditArtifacts(policy, arch.name, artifacts);
      expect(audit.findings).toEqual([]);
    });

    /**
     * Both exceptions are load-bearing today and both are meant to go away,
     * so the audit names every artifact still using them. This assertion is
     * the record of how many that currently is — it should shrink, and a
     * change either way should be deliberate.
     */
    it(`reports exactly the artifacts still using a conditional exception (${arch.name})`, () => {
      const artifacts: ElfFacts[] = Object.entries(measured).map(([name, needed]) => ({
        path: name,
        machine: arch.machine,
        interpreter: null,
        needed,
        versionRequirements: {},
        runpaths: [],
      }));
      const audit = auditArtifacts(policy, arch.name, artifacts);
      const openssl = audit.conditionalUses.filter((use) => use.soname === "libssl.so.3").map((use) => use.path);
      const libcxx = audit.conditionalUses.filter((use) => use.soname === "libc++.so.1").map((use) => use.path);
      const libunwind = audit.conditionalUses
        .filter((use) => use.soname === "libunwind.so.1")
        .map((use) => use.path);
      expect(openssl.sort()).toEqual(["kanna-cli", "kanna-mcp", "kanna-server"]);
      expect(libcxx.sort()).toEqual(FIVE_CXX_CONSUMERS);
      // libunwind.so.1 is a transitive consequence of the same accepted
      // libc++abi exception, so it is conditionally used by exactly the same
      // five artifacts.
      expect(libunwind.sort()).toEqual(FIVE_CXX_CONSUMERS);
    });

    /**
     * `libunwind-18` must reach `Depends` exactly once, alongside the already
     * accepted libc++/libc++abi packages, and its build-only `-dev` sibling
     * must never appear as a runtime dependency.
     */
    it(`derives libunwind-18 in Depends exactly once, and never the -dev package (${arch.name})`, () => {
      const artifacts: ElfFacts[] = Object.entries(measured).map(([name, needed]) => ({
        path: name,
        machine: arch.machine,
        interpreter: arch.interpreter,
        needed,
        versionRequirements: {},
        runpaths: [],
      }));
      const audit = auditArtifacts(policy, arch.name, artifacts);
      expect(audit.findings).toEqual([]);
      const depends = dependsFromAudit(policy, audit);
      expect(depends.filter((entry) => entry === "libunwind-18")).toHaveLength(1);
      expect(depends).not.toContain("libunwind-18-dev");
    });
  }
});
