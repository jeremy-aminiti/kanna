import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  assembleLinuxPackage,
  buildLinuxBinariesCommands,
  stageLinuxPackageBinaries,
} from "../src/runtime/linux-release-build";
import { INSTALLED_EXECUTABLES } from "../src/runtime/linux-package";
import type { CommandRunner } from "../src/runtime/process";

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");

const temporaries: string[] = [];
function scratch(): string {
  const dir = mkdtempSync(join(tmpdir(), "kd-linux-release-build-"));
  temporaries.push(dir);
  return dir;
}
afterEach(() => {
  while (temporaries.length > 0) rmSync(temporaries.pop() as string, { recursive: true, force: true });
});

describe("buildLinuxBinariesCommands", () => {
  /**
   * The ordering defect this test exists for: `tauri-codegen` reads
   * `frontendDist` at compile time and panics when `apps/desktop/dist` is
   * absent — which it is on every fresh checkout and on both CI runners. A
   * command list that built the desktop crate without building the frontend
   * first could never produce a package, and nothing else would say so until a
   * CI job failed inside cargo.
   */
  it("builds the frontend before the desktop crate", () => {
    const commands = buildLinuxBinariesCommands("x86_64");
    const frontend = commands.findIndex(
      ([command, args]) => command === "pnpm" && args.join(" ") === "--dir apps/desktop build"
    );
    const desktop = commands.findIndex(([command, args]) => command === "cargo" && args.includes("kanna-desktop"));
    expect(frontend, "the frontend build is missing from the command list").toBeGreaterThanOrEqual(0);
    expect(desktop).toBeGreaterThanOrEqual(0);
    expect(frontend).toBeLessThan(desktop);
  });

  it("builds every packaged executable for the requested target", () => {
    const commands = buildLinuxBinariesCommands("arm64");
    const cargo = commands.filter(([command]) => command === "cargo");
    expect(cargo.every(([, args]) => args.includes("aarch64-unknown-linux-gnu"))).toBe(true);
    expect(cargo.every(([, args]) => args.includes("--release"))).toBe(true);
    const built = cargo.flatMap(([, args]) => args).join(" ");
    for (const crate of ["kanna-worker", "kanna-daemon", "kanna-cli", "kanna-mcp", "kanna-server", "kanna-task-transfer"]) {
      expect(built, `${crate} is never built`).toContain(crate);
    }
    expect(built).toContain("packages/terminal-recovery/Cargo.toml");
  });
});

/**
 * `assembleLinuxPackage` is orchestration whose command runner is injected for
 * exactly this: the audit's veto and the Depends substitution are decisions,
 * and they should fail here rather than on a Linux builder.
 */
describe("assembleLinuxPackage", () => {
  /** `readelf` output for an artifact whose closure the policy allows. */
  const CLEAN_READELF = `ELF Header:
  Machine:                           AArch64

Program Headers:
      [Requesting program interpreter: /lib/ld-linux-aarch64.so.1]

Dynamic section at offset 0x1 contains 3 entries:
 0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]
 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]

Version needs section '.gnu.version_r' contains 1 entries:
  0x0020:   Name: GLIBC_2.17  Flags: none  Version: 4
`;
  /** The same, plus a library nobody reviewed. */
  const DIRTY_READELF = CLEAN_READELF.replace(
    " 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]",
    " 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]\n" +
      " 0x0000000000000001 (NEEDED)             Shared library: [libnobodyreviewed.so.2]"
  );

  interface Recorded {
    command: string;
    args: string[];
  }

  function fixture(readelfOutput: string): {
    input: Parameters<typeof assembleLinuxPackage>[0];
    calls: Recorded[];
    outputDir: string;
  } {
    const dir = scratch();
    const binariesDir = join(dir, "bin");
    const outputDir = join(dir, "out");
    mkdirSync(binariesDir, { recursive: true });
    mkdirSync(outputDir, { recursive: true });
    for (const name of INSTALLED_EXECUTABLES) writeFileSync(join(binariesDir, name), `#!/bin/sh\n# ${name}\n`);

    const calls: Recorded[] = [];
    const runner: CommandRunner = {
      run: async (command, args) => {
        calls.push({ command, args });
        if (command === "readelf") {
          return { exitCode: 0, stdout: readelfOutput, stderr: "" };
        }
        if (command === "dpkg-deb") {
          // Stand in for the archive so the caller can hash a real file.
          writeFileSync(args[args.length - 1] as string, "deb");
          return { exitCode: 0, stdout: "", stderr: "" };
        }
        return { exitCode: 0, stdout: "", stderr: "" };
      },
    };

    return {
      calls,
      outputDir,
      input: {
        repoRoot,
        channel: "production",
        architecture: "arm64",
        version: "1.2.3",
        binariesDir,
        outputDir,
        env: {},
        runner,
      },
    };
  }

  /**
   * The audit is a gate, not a report. A package whose closure is not in the
   * runtime policy would install cleanly and fail in the loader on a user's
   * machine, so it must never reach `dpkg-deb`.
   */
  it("refuses to package an artifact the audit rejected", async () => {
    const { input, calls } = fixture(DIRTY_READELF);
    await expect(assembleLinuxPackage(input)).rejects.toThrow(/audit failed[\s\S]*libnobodyreviewed\.so\.2/);
    expect(calls.some((call) => call.command === "dpkg-deb")).toBe(false);
  });

  /** The local-iteration escape hatch marks what it produced, so an artifact
   *  built that way announces that it must not be published. */
  it("marks an overridden audit rather than hiding it", async () => {
    const { input } = fixture(DIRTY_READELF);
    const result = await assembleLinuxPackage({ ...input, allowAuditFindings: true });
    expect(result.auditOverridden).toBe(true);
    expect(result.audit.findings).not.toHaveLength(0);
  });

  it("does not mark a clean build as overridden", async () => {
    const { input } = fixture(CLEAN_READELF);
    const result = await assembleLinuxPackage({ ...input, allowAuditFindings: true });
    expect(result.auditOverridden).toBe(false);
    expect(result.audit.findings).toEqual([]);
  });

  /**
   * The whole point of the two-pass stage: the tree is written once with a
   * placeholder so the audit can read real installed paths, then rewritten with
   * the *derived* list. If the placeholder survived, the package would declare
   * dependencies nobody measured — which is the failure mode deriving Depends
   * exists to prevent.
   */
  it("replaces the placeholder Depends with the derived list before dpkg-deb runs", async () => {
    const { input, calls } = fixture(CLEAN_READELF);
    const result = await assembleLinuxPackage(input);

    expect(result.depends).toEqual(["libc6 (>= 2.39)", "libgcc-s1"]);

    // Read the control file exactly as `dpkg-deb` was handed it.
    const build = calls.find((call) => call.command === "dpkg-deb");
    expect(build).toBeTruthy();
    const treeRoot = (build as Recorded).args[(build as Recorded).args.length - 2] as string;
    const control = readFileSync(join(treeRoot, "DEBIAN", "control"), "utf8");
    expect(control).toContain("Depends: libc6 (>= 2.39), libgcc-s1");
    // The placeholder the first pass wrote must be gone.
    expect(control).not.toMatch(/^Depends: libc6$/m);
    expect(result.sha256).toMatch(/^[0-9a-f]{64}$/);
  });

  it("audits every shipped executable, not just the desktop binary", async () => {
    const { input, calls } = fixture(CLEAN_READELF);
    await assembleLinuxPackage(input);
    const audited = calls
      .filter((call) => call.command === "readelf")
      .map((call) => call.args[call.args.length - 1] as string);
    expect(audited).toHaveLength(INSTALLED_EXECUTABLES.length);
    for (const name of INSTALLED_EXECUTABLES) {
      expect(audited.some((path) => path.endsWith(`/${name}`)), `${name} was never audited`).toBe(true);
    }
  });

  it("refuses to package when a binary was never built", async () => {
    const { input } = fixture(CLEAN_READELF);
    rmSync(join(input.binariesDir as string, "kanna-worker"));
    await expect(assembleLinuxPackage(input)).rejects.toThrow(/kanna-worker is not staged/);
  });
});

describe("stageLinuxPackageBinaries", () => {
  it("names a missing build output rather than packaging around it", () => {
    const dir = scratch();
    expect(() =>
      stageLinuxPackageBinaries({ repoRoot: dir, architecture: "x86_64", destination: join(dir, "staged") })
    ).toThrow(/was not built for x86_64-unknown-linux-gnu/);
  });
});
