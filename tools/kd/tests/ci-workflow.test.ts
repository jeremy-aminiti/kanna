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
   * The whole reason the acceptance job exists. A run that found one package
   * would exercise the install half and report a pass for the upgrade gate
   * Phase 1 deferred to this phase.
   */
  it("refuses to call a single-package run an upgrade proof", () => {
    expect(workflow).toContain("./kd test linux-installed");
    expect(workflow).toContain("--old-artifact");
    expect(workflow).toContain("--new-artifact");
    expect(workflow).toMatch(/need two packages to prove an upgrade/);
  });

  /** Its own login session, or `systemctl --user` has no manager to talk to
   *  and the worker cannot be started the way an operator starts it. */
  it("gives the worker a real user manager", () => {
    expect(workflow).toContain("loginctl enable-linger");
  });
});
