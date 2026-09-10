/**
 * `./kd test headless-worker` — Linux Phase 1's exit gate.
 *
 * The lane drives a real `kanna-worker`, which supervises a real daemon and a
 * real server, so it needs those binaries built first. Building them here
 * rather than expecting the caller to is what makes the lane runnable from a
 * clean checkout on either platform.
 */

export function buildHeadlessWorkerBinariesCommand(): [string, string[]] {
  return [
    "cargo",
    [
      "build",
      "-p",
      "kanna-worker",
      "-p",
      "kanna-daemon",
      "-p",
      "kanna-server",
      "-p",
      "kanna-cli",
    ],
  ];
}

export function buildHeadlessWorkerGateCommand(): [string, string[]] {
  return ["pnpm", ["--dir", "tests/headless-worker", "run", "test:gate"]];
}

/**
 * `./kd test linux-installed` — the installed-package and two-version upgrade
 * acceptance lane.
 *
 * It takes artifacts rather than building them. A lane that built its own
 * package would prove things about this checkout; the gate is about a release
 * artifact installing and upgrading on a machine, so the packages come from
 * outside and their paths are recorded with the evidence.
 */
export function buildLinuxInstalledGateCommand(): [string, string[]] {
  return ["pnpm", ["--dir", "tests/linux-installed", "run", "test:gate"]];
}

export interface LinuxInstalledLaneEnv {
  oldDeb: string;
  newDeb: string;
  channel: "production" | "staging";
}

/**
 * The environment the lane reads.
 *
 * Both packages are required even for the install-only file: the upgrade proof
 * is the reason this lane exists, and a run that quietly covered only the
 * install would report a pass for the gate Phase 1 deferred here.
 */
export function linuxInstalledLaneEnv(
  input: LinuxInstalledLaneEnv,
  base: NodeJS.ProcessEnv
): NodeJS.ProcessEnv {
  return {
    ...base,
    KANNA_INSTALLED_DEB: input.newDeb,
    KANNA_INSTALLED_DEB_OLD: input.oldDeb,
    KANNA_INSTALLED_DEB_NEW: input.newDeb,
    KANNA_INSTALLED_CHANNEL: input.channel,
  };
}
