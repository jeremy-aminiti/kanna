/**
 * Building a Kanna deb: the layout, the control metadata, and the version
 * mapping.
 *
 * The layout half of this file mirrors `crates/runtime-defaults/src/linux_install.rs`,
 * which is the runtime's copy of the same contract. A contract test holds the
 * two in step, because the failure they guard against — the package placing a
 * sidecar somewhere the desktop does not look — cannot be caught by anything
 * that runs on the build machine.
 *
 * Nothing here shells out. Staging the tree and writing control files is pure
 * enough to test on macOS; only `dpkg-deb --build` needs a Debian host, and it
 * is returned as a command for the caller to run there.
 */

import { chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, symlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

export type LinuxChannel = "production" | "staging";
export type LinuxArchitecture = "x86_64" | "arm64";

/** The desktop executable's installed name — never the package name, so the
 *  `/usr/bin` launcher and the real binary cannot collide in a PATH lookup. */
export const DESKTOP_BINARY_NAME = "kanna-desktop";

/** Everything a package ships beside the desktop binary: the six sidecars the
 *  desktop spawns plus `kanna-worker`, which is Kanna-owned and so bundled
 *  rather than assumed present. */
export const INSTALLED_EXECUTABLES = [
  DESKTOP_BINARY_NAME,
  "kanna-worker",
  "kanna-daemon",
  "kanna-cli",
  "kanna-mcp",
  "kanna-server",
  "kanna-task-transfer",
  "kanna-terminal-recovery",
] as const;

export const INSTALLED_ICON_SIZES = [32, 64, 128] as const;

export interface ChannelIdentity {
  packageName: string;
  desktopEntryId: string;
  workerUnitName: string;
  displayName: string;
}

export function channelIdentity(channel: LinuxChannel): ChannelIdentity {
  return channel === "staging"
    ? {
        packageName: "kanna-staging",
        desktopEntryId: "build.kanna.staging",
        workerUnitName: "kanna-staging-worker.service",
        displayName: "Kanna Staging",
      }
    : {
        packageName: "kanna",
        desktopEntryId: "build.kanna",
        workerUnitName: "kanna-worker.service",
        displayName: "Kanna",
      };
}

export const ARCHITECTURES: Record<LinuxArchitecture, { debian: string; rustTriple: string }> = {
  x86_64: { debian: "amd64", rustTriple: "x86_64-unknown-linux-gnu" },
  arm64: { debian: "arm64", rustTriple: "aarch64-unknown-linux-gnu" },
};

export function debianArchitecture(architecture: LinuxArchitecture): string {
  return ARCHITECTURES[architecture].debian;
}

export function rustTripleFor(architecture: LinuxArchitecture): string {
  return ARCHITECTURES[architecture].rustTriple;
}

export function architectureForRustTriple(triple: string): LinuxArchitecture | null {
  const entry = (Object.entries(ARCHITECTURES) as Array<[LinuxArchitecture, { rustTriple: string }]>).find(
    ([, value]) => value.rustTriple === triple
  );
  return entry ? entry[0] : null;
}

/**
 * Kanna's semantic version mapped into a Debian version.
 *
 * `dpkg` orders `~` *before* everything including the empty string, which is
 * what makes a staging candidate sort below the production release of the same
 * version — so a machine tracking production never has a staging build offered
 * as an upgrade, and a machine on staging moves forward when production
 * catches up. The `-1` is the Debian revision: Kanna's packaging is the only
 * packaging of this source, so it stays 1 and the upstream version carries all
 * the meaning.
 */
export function debianVersion(version: string, channel: LinuxChannel, stagingIteration?: number): string {
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(`Version ${JSON.stringify(version)} must be X.Y.Z.`);
  }
  if (channel === "production") {
    if (stagingIteration !== undefined) {
      throw new Error("A production Debian version has no staging iteration.");
    }
    return `${version}-1`;
  }
  if (!Number.isInteger(stagingIteration) || (stagingIteration as number) < 1) {
    throw new Error("A staging Debian version needs a positive integer iteration.");
  }
  return `${version}~staging.${stagingIteration}-1`;
}

export interface PackageLayoutInput {
  channel: LinuxChannel;
  /** Install prefix. `/usr` in a real package; a temp dir in a test. */
  prefix?: string;
}

export interface PackageLayout {
  libDir: string;
  desktopBinary: string;
  launcher: string;
  resourceDir: string;
  desktopEntry: string;
  icon: (size: number) => string;
}

/** The installed paths, relative to a prefix. The Rust side computes the same
 *  ones; see this file's header. */
export function packageLayout(input: PackageLayoutInput): PackageLayout {
  const prefix = input.prefix ?? "/usr";
  const { packageName, desktopEntryId } = channelIdentity(input.channel);
  const libDir = join(prefix, "lib", packageName);
  return {
    libDir,
    desktopBinary: join(libDir, DESKTOP_BINARY_NAME),
    launcher: join(prefix, "bin", packageName),
    resourceDir: libDir,
    desktopEntry: join(prefix, "share", "applications", `${desktopEntryId}.desktop`),
    icon: (size: number) =>
      join(prefix, "share", "icons", "hicolor", `${size}x${size}`, "apps", `${desktopEntryId}.png`),
  };
}

export interface ControlInput {
  channel: LinuxChannel;
  version: string;
  stagingIteration?: number;
  architecture: LinuxArchitecture;
  /** Runtime package dependencies, derived from the audited artifact closure —
   *  never a hand-written list, and never every library the build happened to
   *  observe. */
  depends: string[];
  installedSizeKb: number;
}

/**
 * `DEBIAN/control`.
 *
 * `Conflicts`/`Replaces` are deliberately absent: `kanna` and `kanna-staging`
 * share no path, so both may be installed at once and a developer running both
 * is a supported state rather than an accident to prevent.
 */
export function buildDebControl(input: ControlInput): string {
  const identity = channelIdentity(input.channel);
  if (input.depends.length === 0) {
    throw new Error("A package with no declared runtime dependencies has not been audited.");
  }
  const description =
    input.channel === "staging"
      ? "Kanna (staging channel)\n Orchestrates coding agent tasks across git worktrees.\n .\n This is the staging channel. It installs alongside the production package\n and shares no data with it."
      : "Kanna\n Orchestrates coding agent tasks across git worktrees.";
  return (
    [
      `Package: ${identity.packageName}`,
      `Version: ${debianVersion(input.version, input.channel, input.stagingIteration)}`,
      `Architecture: ${debianArchitecture(input.architecture)}`,
      "Maintainer: Kanna <releases@kanna.build>",
      `Installed-Size: ${input.installedSizeKb}`,
      `Depends: ${[...input.depends].sort().join(", ")}`,
      "Section: devel",
      "Priority: optional",
      "Homepage: https://kanna.build",
      `Description: ${description}`,
    ].join("\n") + "\n"
  );
}

/** The `.desktop` entry. `StartupWMClass` matters: without it the shell shows a
 *  second, unnamed icon for the running window instead of marking ours active. */
export function buildDesktopEntry(channel: LinuxChannel): string {
  const identity = channelIdentity(channel);
  return (
    [
      "[Desktop Entry]",
      "Type=Application",
      `Name=${identity.displayName}`,
      "Comment=Orchestrate coding agent tasks",
      `Exec=/usr/bin/${identity.packageName} %U`,
      `Icon=${identity.desktopEntryId}`,
      `StartupWMClass=${identity.desktopEntryId}`,
      "Terminal=false",
      "Categories=Development;IDE;",
    ].join("\n") + "\n"
  );
}

/**
 * `DEBIAN/postinst`.
 *
 * What it deliberately does **not** do is as load-bearing as what it does. It
 * never starts or enables the worker: installing a package must not put a
 * competing canonical server on the machine, and `systemctl --user` from a
 * `root` maintainer script has no user manager to talk to anyway. It never
 * stops a daemon or kills a cgroup — live agent sessions surviving a package
 * replacement is the whole point of the daemon, and an upgrade that killed
 * them would be the one thing the installed-upgrade proof exists to forbid.
 * The operator's own launcher restart is what applies the new runtime.
 */
export function buildPostinst(channel: LinuxChannel): string {
  const identity = channelIdentity(channel);
  return `#!/bin/sh
set -e

# Desktop-database and icon-cache updates are best-effort: a machine with no
# desktop environment installed is a supported host for the worker, and a
# missing helper there must not fail the installation.
if [ "$1" = "configure" ]; then
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
  fi
  cat <<'NOTE'
${identity.displayName} is installed.

If it was running, close and relaunch it to pick up this version; a running
worker picks it up with:
  systemctl --user restart ${identity.workerUnitName}
Live agent sessions survive both — they belong to the daemon, not the launcher.
NOTE
fi

exit 0
`;
}

/**
 * `DEBIAN/prerm`.
 *
 * Removal leaves user data alone. Databases, worktrees, pairings and daemon
 * state live under the user's data directory, which no package owns; deleting
 * them would make an uninstall destroy the tasks it was never asked about.
 * `purge` does not change that either — this is deliberate, and the support
 * documentation says where the data is so a person can remove it themselves.
 */
export function buildPrerm(channel: LinuxChannel): string {
  const identity = channelIdentity(channel);
  return `#!/bin/sh
set -e

if [ "$1" = "remove" ]; then
  cat <<'NOTE'
${identity.displayName} package files are being removed.

Your tasks, worktrees and database are NOT removed. A running worker keeps
running until you stop it:
  systemctl --user disable --now ${identity.workerUnitName}
NOTE
fi

exit 0
`;
}

export interface StageTreeInput {
  channel: LinuxChannel;
  /** Root of the package tree — becomes `/` in the installed package. */
  root: string;
  /** Directory holding the built executables, named exactly as installed. */
  binariesDir: string;
  /** Repo `.kanna/` definitions to ship as built-in resources. */
  builtinResourcesDir: string;
  /** Directory holding `<size>x<size>.png` icon sources. */
  iconsDir: string;
  control: Omit<ControlInput, "channel" | "installedSizeKb"> & { installedSizeKb?: number };
}

export interface StagedTree {
  root: string;
  controlPath: string;
  installedPaths: string[];
}

/**
 * Lay the package tree down on disk.
 *
 * Every executable must be present: a package that silently shipped seven of
 * eight binaries would install cleanly and fail at the moment a task tried to
 * spawn, which is far from the build that produced it.
 */
export function stageLinuxPackageTree(input: StageTreeInput): StagedTree {
  const layout = packageLayout({ channel: input.channel, prefix: join(input.root, "usr") });
  const identity = channelIdentity(input.channel);
  const installed: string[] = [];

  mkdirSync(layout.libDir, { recursive: true });
  for (const name of INSTALLED_EXECUTABLES) {
    const source = join(input.binariesDir, name);
    if (!existsSync(source)) {
      throw new Error(`Package is missing ${name}: expected it at ${source}.`);
    }
    const destination = join(layout.libDir, name);
    copyFileSync(source, destination);
    chmodSync(destination, 0o755);
    installed.push(destination);
  }

  // The launcher is a symlink, not a copy: `current_exe()` must resolve into
  // the library directory or the runtime's sibling search finds no sidecars.
  mkdirSync(dirname(layout.launcher), { recursive: true });
  symlinkSync(`../lib/${identity.packageName}/${DESKTOP_BINARY_NAME}`, layout.launcher);
  installed.push(layout.launcher);

  const resources = join(layout.resourceDir, ".kanna");
  for (const section of ["agents", "workflows", "tasks"]) {
    const source = join(input.builtinResourcesDir, section);
    if (!existsSync(source)) {
      throw new Error(`Built-in resources are missing ${section}: expected ${source}.`);
    }
    cpSync(source, join(resources, section), { recursive: true });
  }
  installed.push(resources);

  mkdirSync(dirname(layout.desktopEntry), { recursive: true });
  writeFileSync(layout.desktopEntry, buildDesktopEntry(input.channel));
  installed.push(layout.desktopEntry);

  for (const size of INSTALLED_ICON_SIZES) {
    const source = join(input.iconsDir, `${size}x${size}.png`);
    if (!existsSync(source)) {
      throw new Error(`Package is missing its ${size}px icon: expected ${source}.`);
    }
    const destination = layout.icon(size);
    mkdirSync(dirname(destination), { recursive: true });
    copyFileSync(source, destination);
    installed.push(destination);
  }

  const controlDir = join(input.root, "DEBIAN");
  mkdirSync(controlDir, { recursive: true });
  const controlPath = join(controlDir, "control");
  writeFileSync(
    controlPath,
    buildDebControl({
      ...input.control,
      channel: input.channel,
      installedSizeKb: input.control.installedSizeKb ?? estimateInstalledSizeKb(installed),
    })
  );
  for (const [name, contents] of [
    ["postinst", buildPostinst(input.channel)],
    ["prerm", buildPrerm(input.channel)],
  ] as const) {
    const path = join(controlDir, name);
    writeFileSync(path, contents);
    chmodSync(path, 0o755);
  }

  // The root itself too: `dpkg-deb` records it as `./` in the archive, so a
  // 0775 build directory would ship a group-writable filesystem root entry.
  chmodSync(input.root, 0o755);
  normalizePackageModes(input.root);

  return { root: input.root, controlPath, installedPaths: installed };
}

/**
 * Give every packaged file a mode the *package* chose.
 *
 * `writeFileSync`, `mkdirSync` and `cpSync` all take their permissions from the
 * builder's umask, so a build machine with a group-writable default produces a
 * package that installs group-writable files onto every user's machine — the
 * kind of defect that is invisible in the tree and only appears once somebody
 * runs `dpkg-deb --contents`. It was: the first real package built from this
 * code shipped `usr/share/applications/build.kanna.desktop` as 0664.
 *
 * Executables keep 0755 because they were chmodded deliberately above.
 */
function normalizePackageModes(root: string): void {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isSymbolicLink()) continue;
    if (entry.isDirectory()) {
      chmodSync(path, 0o755);
      normalizePackageModes(path);
      continue;
    }
    chmodSync(path, statSync(path).mode & 0o111 ? 0o755 : 0o644);
  }
}

function estimateInstalledSizeKb(paths: string[]): number {
  let bytes = 0;
  for (const path of paths) {
    try {
      bytes += readFileSync(path).byteLength;
    } catch {
      // Directories and symlinks; their own size is noise at this resolution.
    }
  }
  return Math.max(1, Math.ceil(bytes / 1024));
}

/** The command that turns a staged tree into a `.deb`. Runs where `dpkg-deb`
 *  exists — a Linux builder — which is why it is returned rather than run. */
export function buildDebCommand(treeRoot: string, outputPath: string): [string, string[]] {
  return ["dpkg-deb", ["--root-owner-group", "--build", treeRoot, outputPath]];
}

/** The package file name. Debian's convention, and the name the apt pool and
 *  the release manifest both refer to. */
export function debFileName(input: {
  channel: LinuxChannel;
  version: string;
  stagingIteration?: number;
  architecture: LinuxArchitecture;
}): string {
  const identity = channelIdentity(input.channel);
  const version = debianVersion(input.version, input.channel, input.stagingIteration);
  return `${identity.packageName}_${version}_${debianArchitecture(input.architecture)}.deb`;
}
