import { readFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const repoRoot = resolve(import.meta.dirname, "..", "..", "..");
const workflowsDir = resolve(repoRoot, ".github/workflows");

// Hosted CI was removed: `ci.yml` and `remote-e2e.yml` are gone, and verification
// is local (`pnpm test`, `./kd test rust`) plus the Kanna review stage. The
// config-schema Pages workflow stays because it is continuous deployment of the
// public https://schemas.kanna.build/config.schema.json contract, not a check.
const CONFIG_SCHEMA_DEPLOYMENT = "config-schema-pages.yml";
// The Linux release build. Not a return of hosted CI for the repository at
// large: it is the build lane for artifacts that cannot be produced on a
// developer's Mac, and — under the 2026-09-09 owner directive, with the Intel
// Mac unavailable — the substitute x86-64 installed-acceptance host.
const LINUX_RELEASE_CHECK = "linux-release-check.yml";
const REMOVED_CI_WORKFLOWS = ["ci.yml", "remote-e2e.yml"];

function workflowFiles(): string[] {
  return readdirSync(workflowsDir, { withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => entry.name)
    .sort();
}

describe("GitHub Actions workflow set", () => {
  it("contains exactly the intended set", () => {
    expect(workflowFiles()).toEqual([CONFIG_SCHEMA_DEPLOYMENT, LINUX_RELEASE_CHECK]);
  });

  it("keeps the config-schema Pages deployment", () => {
    expect(workflowFiles()).toContain(CONFIG_SCHEMA_DEPLOYMENT);
  });

  it("does not reintroduce the removed CI workflows", () => {
    const workflows = workflowFiles();
    for (const removed of REMOVED_CI_WORKFLOWS) {
      expect(workflows, `${removed} must stay removed`).not.toContain(removed);
    }
  });
});

/**
 * The Linux lane's guarantees, asserted on the file rather than trusted to
 * review. Each one is a property whose absence would only be discovered by a
 * Linux user, or by a macOS release that got blocked by something unrelated.
 */
describe("the Linux release check", () => {
  const workflow = readFileSync(resolve(workflowsDir, LINUX_RELEASE_CHECK), "utf8");
  const pages = readFileSync(resolve(workflowsDir, CONFIG_SCHEMA_DEPLOYMENT), "utf8");

  it("builds both required architectures natively", () => {
    expect(workflow).toContain("runner: ubuntu-24.04\n");
    expect(workflow).toContain("runner: ubuntu-24.04-arm");
    expect(workflow).toContain("architecture: x86_64");
    expect(workflow).toContain("architecture: arm64");
    // Cross-compiling the other would leave the linkage — which is exactly what
    // the dependency audit is about — untested on the machine it must run on.
    // Checked on what the workflow *runs*, not on its prose.
    const steps = workflow
      .split("\n")
      .filter((line) => /^\s*-?\s*(run|uses):/.test(line))
      .join("\n");
    expect(steps).not.toMatch(/qemu|binfmt|cross-rs|setup-cross/i);
  });

  /**
   * A build lane that could publish is a build lane that can be made to
   * publish. It holds no signing or release credential, and its permissions
   * say so.
   */
  it("is read-only and holds no publishing credential", () => {
    expect(workflow).toMatch(/permissions:\n {2}contents: read/);
    expect(workflow).not.toMatch(/packages: write|contents: write|id-token: write/);
    expect(workflow).not.toMatch(/GPG_PRIVATE_KEY|TAURI_SIGNING|APT_SIGNING/);
  });

  /**
   * The schema publish is continuous deployment of a public contract. A Linux
   * build failure must not be able to reach it — different concurrency group,
   * and the Pages permissions scoped to the job that deploys.
   */
  it("cannot block or borrow the schema deployment", () => {
    expect(workflow).toContain("group: linux-release-check-");
    expect(workflow).not.toContain("group: config-schema-pages");
    expect(pages).toMatch(/^permissions: \{\}$/m);
    expect(pages).toMatch(/deploy:[\s\S]*?permissions:\n {6}contents: read\n {6}pages: write/);
    expect(pages).toContain("inputs.action == 'linux-build'");
  });

  /**
   * The lane builds and audits; it does not run installed *acceptance*. That
   * needs a second package from another source revision to upgrade between,
   * which is not wired here — follow-up item 2 in §7 of the evidence doc. It
   * does, separately, probe whether the host prerequisites that lane needs
   * are even present (`installed-check-prerequisites`, below) — a fact-finding
   * job, not the lane itself, and the one place `loginctl enable-linger` is
   * expected to appear.
   *
   * Asserted rather than left implicit because the previous shape was worse
   * than absent: a job gated on an input that is empty on `pull_request` and
   * `push`, so it silently skipped there and, on the one trigger where it did
   * run, always failed on its own two-package check. A workflow that promises
   * acceptance it cannot perform is a false green.
   */
  it("does not claim installed acceptance it does not perform", () => {
    expect(workflow).not.toContain("installed-acceptance:");
    // Checked on what the workflow *runs*: the header comment names the manual
    // invocation on purpose, so a prose match would forbid documenting it.
    const steps = workflow
      .split("\n")
      .filter((line) => /^\s*-?\s*(run|uses):/.test(line) || /^\s{8,}/.test(line))
      .filter((line) => !/^\s*#/.test(line))
      .join("\n");
    expect(steps).not.toContain("kd test linux-installed");
    expect(steps).not.toContain("install-unit");
    expect(steps).not.toContain("apt-get install ./");
    // And it does not describe a second build it never does.
    expect(workflow).not.toMatch(/^\s*#.*merge base/im);
  });

  /** One build invocation per architecture, from this revision — which is
   *  exactly what the job does, so the YAML and the behaviour agree. */
  it("builds one package per architecture", () => {
    const builds = workflow.match(/\.\/kd build linux-package/g) ?? [];
    expect(builds).toHaveLength(1);
    expect(workflow).toContain("--architecture ${{ matrix.architecture }}");
    expect(workflow).toMatch(/if-no-files-found: error/);
  });

  it("installs the pinned Zig toolchain before building", () => {
    expect(workflow).toContain("zig_platform: x86_64-linux");
    expect(workflow).toContain(
      "zig_sha256: 02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239"
    );
    expect(workflow).toContain("zig_platform: aarch64-linux");
    expect(workflow).toContain(
      "zig_sha256: 958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f"
    );
    expect(workflow).toContain("sha256sum --check --strict -");
    expect(workflow).toContain('>> "$GITHUB_PATH"');
    expect(workflow.indexOf("- name: Install Zig 0.15.2")).toBeLessThan(
      workflow.indexOf("- name: Build the candidate package")
    );
  });

  /**
   * No job may be gated on a `workflow_dispatch`/`workflow_call` input that is
   * simply absent on `pull_request` and `push`. That is how the removed job
   * came to be silently skipped on every trigger that mattered.
   */
  it("gates no job on an input the PR and push triggers do not supply", () => {
    expect(workflow).not.toMatch(/if:\s*inputs\./);
  });

  /**
   * libghostty-vt-sys's build script emits `-lc++`/`-lc++abi` on Linux (Zig's
   * Ghostty build links libc++), which a bare `ubuntu-24.04` runner does not
   * carry. Without the dev packages, both architectures fail at link with
   * "cannot find -lc++"/"-lc++abi" after the Zig install succeeds — this is
   * a build-time linker input, not a new runtime dependency: the resulting
   * dynamic link is already the declared "conditional" exception in
   * packaging/linux/runtime-policy.json.
   *
   * Checked on the actual apt-get install command, comments stripped, so a
   * comment merely mentioning these packages can't carry the assertion.
   */
  it("installs libc++/libc++abi dev packages before building", () => {
    const installStep = workflow
      .split(/\n(?=\s*- name:)/)
      .find((step) => step.includes("- name: Install build and packaging dependencies"));
    expect(installStep).toBeDefined();
    const aptCommand = installStep!
      .split("\n")
      .filter((line) => !/^\s*#/.test(line))
      .join("\n");
    expect(aptCommand).toContain("libc++-dev");
    expect(aptCommand).toContain("libc++abi-dev");
    expect(workflow.indexOf("- name: Install build and packaging dependencies")).toBeLessThan(
      workflow.indexOf("- name: Build the candidate package")
    );
  });

  /**
   * `tests/linux-installed`'s own host check (`installedWorker.ts`'s
   * `inspectHost`) treats `systemctl --user is-system-running` as usable
   * whenever the command merely ran (its Node `child_process` exit code is
   * non-null), which is true even for "Failed to connect to bus" — it cannot
   * actually distinguish a working user manager from an absent one. This
   * probe checks the real facts on both hosted runners directly, so that
   * distinction doesn't rest on the harness's own unreliable check.
   */
  it("probes systemd/session prerequisites on both architectures before any install is attempted", () => {
    const probeStep = workflow
      .split(/\n(?=\s*- name:)/)
      .find((step) => step.includes("- name: Probe systemd, session and sudo prerequisites"));
    expect(probeStep).toBeDefined();
    for (const check of [
      "systemctl is-system-running",
      "systemctl --user is-system-running",
      "loginctl enable-linger",
      "sudo -n true",
    ]) {
      expect(probeStep).toContain(check);
    }
    const architectures = workflow.match(/architecture: (x86_64|arm64)/g);
    expect(architectures?.length).toBeGreaterThanOrEqual(4); // build + prerequisites, both archs
  });

  /**
   * Measured 2026-09-10 (run 34471789591): the hosted runners' prerequisites
   * hold (real systemd PID 1, a `running` user manager, working
   * `enable-linger`, passwordless sudo), so this job does the install-only
   * check for real — `apt install` from the built package with no `-dev`
   * packages present to mask a missing `Depends`, then the harness's
   * install-only lifecycle coverage. It is still not the upgrade lane: that
   * needs `KANNA_INSTALLED_DEB_OLD`/`_NEW` from two distinct source
   * revisions, which is not wired here (§7 of the evidence doc), so
   * `upgrade.e2e.test.ts` must not be invoked from this job.
   */
  it("installs the built package via apt and runs the install-only lifecycle probe, not the upgrade lane", () => {
    const rawStep = workflow
      .split(/\n(?=  installed-check:)/)
      .find((s) => s.startsWith("  installed-check:"));
    expect(rawStep).toBeDefined();
    // Checked on the actual commands, comments stripped, so a comment
    // mentioning upgrade.e2e.test.ts to explain why it's excluded can't trip
    // the assertion that it is, in fact, excluded.
    const step = rawStep!
      .split("\n")
      .filter((line) => !/^\s*#/.test(line))
      .join("\n");
    expect(step).toContain("apt-get install");
    expect(step).not.toMatch(/apt-get install[^\n]*-dev/);
    expect(step).toContain("installed.e2e.test.ts");
    expect(step).not.toContain("upgrade.e2e.test.ts");
    expect(step).not.toContain("kd test linux-installed");
    // Reuses this same run's own build output; no cross-run artifact fetch.
    expect(step).not.toContain("run-id:");
  });
});
