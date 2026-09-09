/**
 * `./kd build linux-package` — build one architecture's `.deb`.
 *
 * The order is the point. Build, then *audit*, then derive `Depends` from what
 * the audit found, then package. A conventional Debian build writes `Depends`
 * by hand and hopes; here the declared dependencies are a consequence of the
 * measured artifact closure, so a library that appears because the build host
 * happened to have it cannot become a silent runtime requirement — it fails
 * the audit instead.
 *
 * Everything that needs a Debian host (`readelf`, `dpkg-deb`) goes through the
 * injected runner, so the orchestration is testable anywhere.
 */

import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { join } from "node:path";
import type { CommandRunner } from "./process";
import {
  INSTALLED_EXECUTABLES,
  buildDebCommand,
  channelIdentity,
  debFileName,
  debianArchitecture,
  packageLayout,
  rustTripleFor,
  stageLinuxPackageTree,
  type LinuxArchitecture,
  type LinuxChannel,
} from "./linux-package";
import {
  auditArtifacts,
  dependsFromAudit,
  formatAuditReport,
  parseReadelf,
  readRuntimePolicy,
  readelfCommand,
  type AuditResult,
} from "./linux-elf-audit";

export interface LinuxPackageBuildInput {
  repoRoot: string;
  channel: LinuxChannel;
  architecture: LinuxArchitecture;
  version: string;
  stagingIteration?: number;
  /** Where the built executables already are. Defaults to the staging
   *  directory `buildLinuxBinariesCommands` writes into. */
  binariesDir?: string;
  outputDir: string;
  env: NodeJS.ProcessEnv;
  runner: CommandRunner;
  /** Skip the audit's veto. Only for iterating locally: a release build must
   *  never set it, and `assembleLinuxPackage` records that it was set. */
  allowAuditFindings?: boolean;
}

export interface LinuxPackageBuildResult {
  debPath: string;
  sha256: string;
  depends: string[];
  audit: AuditResult;
  auditReport: string;
  auditOverridden: boolean;
}

/** Where `kd` stages the eight executables a package ships. Inside `.build/`,
 *  so it follows the repo's artifact rule and is cleaned with everything else. */
export function linuxPackageStagingDir(repoRoot: string, architecture: LinuxArchitecture): string {
  return join(repoRoot, ".build", "linux-package", architecture);
}

/**
 * The commands that produce the eight executables for one architecture.
 *
 * `kanna-worker` is built here alongside the six sidecars because it is
 * Kanna-owned: a package that expected the user to have one would not be an
 * installable product.
 *
 * The desktop binary comes last, and the **frontend build has to come before
 * it**. `tauri-codegen` reads `frontendDist` at compile time and panics —
 * "this path doesn't exist" — when `apps/desktop/dist` is absent, which it is
 * on every fresh checkout and on both CI runners. The repo's own Rust lane
 * orders it the same way for the same reason
 * (`rust-test.ts`'s `frontend` step).
 */
export function buildLinuxBinariesCommands(architecture: LinuxArchitecture): Array<[string, string[]]> {
  const target = rustTripleFor(architecture);
  return [
    [
      "cargo",
      [
        "build", "--release", "--target", target,
        "-p", "kanna-worker",
        "-p", "kanna-daemon",
        "-p", "kanna-cli",
        "-p", "kanna-mcp",
        "-p", "kanna-server",
        "-p", "kanna-task-transfer",
      ],
    ],
    ["cargo", ["build", "--release", "--target", target, "--manifest-path", "packages/terminal-recovery/Cargo.toml"]],
    ["pnpm", ["--dir", "apps/desktop", "build"]],
    ["cargo", ["build", "--release", "--target", target, "-p", "kanna-desktop"]],
  ];
}

/**
 * Collect the built executables into one directory under their *installed*
 * names.
 *
 * Cargo scatters them across per-manifest target directories and calls the
 * desktop one `kanna-desktop` already; the package wants all eight side by
 * side. Doing the rename here rather than at package time means the staging
 * directory is exactly what gets installed, so an audit of it is an audit of
 * the product.
 */
export function stageLinuxPackageBinaries(input: {
  repoRoot: string;
  architecture: LinuxArchitecture;
  /** `.build` by default — the repo's artifact directory. */
  buildDir?: string;
  destination?: string;
}): string {
  const target = rustTripleFor(input.architecture);
  const releaseDir = join(input.repoRoot, input.buildDir ?? ".build", target, "release");
  const destination = input.destination ?? linuxPackageStagingDir(input.repoRoot, input.architecture);
  mkdirSync(destination, { recursive: true });
  for (const name of INSTALLED_EXECUTABLES) {
    const source = join(releaseDir, name);
    if (!existsSync(source)) {
      throw new Error(`${name} was not built for ${target}: expected ${source}.`);
    }
    copyFileSync(source, join(destination, name));
    chmodSync(join(destination, name), 0o755);
  }
  return destination;
}

/** Read one artifact's ELF facts through `readelf` on the build host. */
export async function readElfFacts(runner: CommandRunner, repoRoot: string, env: NodeJS.ProcessEnv, path: string) {
  const [command, args] = readelfCommand(path);
  const result = await runner.run(command, args, { cwd: repoRoot, env });
  if (result.exitCode !== 0) {
    throw new Error(`readelf failed on ${path}: ${result.stderr || result.stdout}`);
  }
  return parseReadelf(path, result.stdout);
}

/**
 * Stage, audit, package.
 *
 * The tree is staged twice on purpose: once to have real installed paths for
 * the audit to read, and again with the derived `Depends` written into
 * `control`. Auditing the *staged* files rather than the build outputs is what
 * makes the finding describe the artifact a user will actually run.
 */
export async function assembleLinuxPackage(input: LinuxPackageBuildInput): Promise<LinuxPackageBuildResult> {
  const policy = readRuntimePolicy(input.repoRoot);
  const binariesDir = input.binariesDir ?? linuxPackageStagingDir(input.repoRoot, input.architecture);
  for (const name of INSTALLED_EXECUTABLES) {
    if (!existsSync(join(binariesDir, name))) {
      throw new Error(`${name} is not staged in ${binariesDir}. Build it before packaging.`);
    }
  }

  const treeRoot = join(input.outputDir, `${channelIdentity(input.channel).packageName}-tree`);
  rmSync(treeRoot, { recursive: true, force: true });
  mkdirSync(input.outputDir, { recursive: true });

  const stageInput = {
    channel: input.channel,
    root: treeRoot,
    binariesDir,
    builtinResourcesDir: join(input.repoRoot, ".kanna"),
    iconsDir: join(input.repoRoot, "apps", "desktop", "src-tauri", "icons"),
    control: {
      version: input.version,
      stagingIteration: input.stagingIteration,
      architecture: input.architecture,
      // Placeholder: replaced below by the audited closure. Never shipped —
      // the tree is rebuilt with the derived list before `dpkg-deb` runs.
      depends: ["libc6"] as string[],
    },
  };
  stageLinuxPackageTree(stageInput);

  const layout = packageLayout({ channel: input.channel, prefix: join(treeRoot, "usr") });
  const facts = [];
  for (const name of INSTALLED_EXECUTABLES) {
    facts.push(await readElfFacts(input.runner, input.repoRoot, input.env, join(layout.libDir, name)));
  }
  const audit = auditArtifacts(policy, input.architecture, facts);
  const auditReport = formatAuditReport(input.architecture, audit);
  if (audit.findings.length > 0 && input.allowAuditFindings !== true) {
    throw new Error(
      `Linux runtime audit failed; the package would not run on ${policy.baseline.distribution}.\n${auditReport}`
    );
  }

  const depends = dependsFromAudit(policy, audit);
  rmSync(treeRoot, { recursive: true, force: true });
  stageLinuxPackageTree({ ...stageInput, control: { ...stageInput.control, depends } });

  const debPath = join(
    input.outputDir,
    debFileName({
      channel: input.channel,
      version: input.version,
      stagingIteration: input.stagingIteration,
      architecture: input.architecture,
    })
  );
  const [command, args] = buildDebCommand(treeRoot, debPath);
  const built = await input.runner.run(command, args, { cwd: input.repoRoot, env: input.env });
  if (built.exitCode !== 0) {
    throw new Error(`dpkg-deb failed: ${built.stderr || built.stdout}`);
  }

  return {
    debPath,
    sha256: createHash("sha256").update(readFileSync(debPath)).digest("hex"),
    depends,
    audit,
    auditReport,
    auditOverridden: input.allowAuditFindings === true && audit.findings.length > 0,
  };
}

/** One line an operator or a CI log can read without opening the JSON. */
export function formatLinuxPackageResult(input: {
  channel: LinuxChannel;
  architecture: LinuxArchitecture;
  result: LinuxPackageBuildResult;
}): string {
  const lines = [
    `${input.result.debPath} (${debianArchitecture(input.architecture)})`,
    `  sha256: ${input.result.sha256}`,
    `  Depends: ${input.result.depends.join(", ")}`,
    input.result.auditReport,
  ];
  if (input.result.auditOverridden) {
    lines.push("  WARNING: audit findings were overridden. This artifact must not be published.");
  }
  return lines.join("\n");
}
