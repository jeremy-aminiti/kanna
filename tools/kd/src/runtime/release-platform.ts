/**
 * What "a release" means for each platform, in one place.
 *
 * Linux does not fit through the macOS release path, and forcing it would be
 * the wrong design rather than a shortcut avoided. A macOS release is a signed
 * bundle plus a `latest.json` the Tauri updater polls; a Linux release is a
 * `.deb` per architecture in a signed apt archive that `apt` polls. The
 * artifact, the trust anchor, the thing that advertises "current" and the
 * component that performs the upgrade are different at every step.
 *
 * What the two share is the *lineage discipline* — immutable per-version tags,
 * a release-series branch, a soak window before promotion, and a refusal to
 * publish over a candidate that diverged. That is what a descriptor names, so
 * both platforms are governed by the same rules while each keeps its own
 * distribution mechanism.
 *
 * The separation is also the point of the channel split: a Linux build failure
 * must never freeze a macOS ship. Nothing here lets one platform's tags,
 * branches or channel state satisfy or block the other's.
 */

export type ReleasePlatform = "macos" | "linux";

export interface ReleasePlatformDescriptor {
  platform: ReleasePlatform;
  /** The GitHub release that *points at* the current candidate. */
  stagingChannelTag: string;
  productionChannelTag: string;
  /** Immutable per-version tags. Prefixed on Linux so a Linux version can
   *  never be mistaken for — or satisfy a version-floor lookup against — a
   *  macOS one. */
  stagingTag: (version: string, iteration: number) => string;
  productionTag: (version: string) => string;
  /** Release-series branch, so a series on one platform cannot prune or
   *  advance the other's history. */
  seriesBranch: (version: string) => string;
  isSeriesBranch: (branch: string) => boolean;
  /** How the channel advertises what is current. */
  channelKind: "updater-manifest" | "apt-archive";
  /** The updater manifest asset name, where there is one. */
  manifestName?: string;
  /** Which signing key custody this platform's artifacts use. Deliberately not
   *  the same key: an apt archive signature and a Tauri updater signature
   *  authenticate different things to different verifiers. */
  signingKey: "tauri-updater" | "apt-repository";
}

const MACOS: ReleasePlatformDescriptor = {
  platform: "macos",
  stagingChannelTag: "desktop-staging",
  productionChannelTag: "desktop",
  stagingTag: (version, iteration) => `v${version}-staging.${iteration}`,
  productionTag: (version) => `v${version}`,
  seriesBranch: (version) => `release/${seriesOf(version)}`,
  isSeriesBranch: (branch) => /^release\/\d+\.\d+$/.test(branch),
  channelKind: "updater-manifest",
  manifestName: "latest-staging.json",
  signingKey: "tauri-updater",
};

const LINUX: ReleasePlatformDescriptor = {
  platform: "linux",
  stagingChannelTag: "desktop-linux-staging",
  productionChannelTag: "desktop-linux",
  // `linux-` prefixed and never `v`-prefixed: a bare `vX.Y.Z` on this
  // repository means the macOS release, and GitHub's own "latest release" is
  // resolved from those. A Linux tag must not become the thing the macOS
  // updater endpoint follows.
  stagingTag: (version, iteration) => `linux-v${version}-staging.${iteration}`,
  productionTag: (version) => `linux-v${version}`,
  seriesBranch: (version) => `release/linux/${seriesOf(version)}`,
  isSeriesBranch: (branch) => /^release\/linux\/\d+\.\d+$/.test(branch),
  channelKind: "apt-archive",
  signingKey: "apt-repository",
};

function seriesOf(version: string): string {
  const match = /^(\d+)\.(\d+)\.\d+$/.exec(version);
  if (!match) throw new Error(`Version ${JSON.stringify(version)} must be X.Y.Z.`);
  return `${match[1]}.${match[2]}`;
}

export function releasePlatform(platform: ReleasePlatform): ReleasePlatformDescriptor {
  return platform === "linux" ? LINUX : MACOS;
}

export const RELEASE_PLATFORMS: ReleasePlatform[] = ["macos", "linux"];

/**
 * The default when `--platform` is omitted.
 *
 * macOS, unconditionally — including when the command runs on a Linux
 * developer's machine. `kd release` operates on shared remote channel state,
 * not on the local machine, so making it depend on the host would mean the
 * same command did different things to production depending on who typed it.
 */
export const DEFAULT_RELEASE_PLATFORM: ReleasePlatform = "macos";

export function parseReleasePlatform(value: string | undefined): ReleasePlatform {
  if (value === undefined) return DEFAULT_RELEASE_PLATFORM;
  if (value === "macos" || value === "linux") return value;
  throw new Error(`Unknown --platform ${JSON.stringify(value)}. Expected macos or linux.`);
}

/**
 * Does a tag belong to this platform's lineage?
 *
 * Asked as an exclusion as well as a match: `v1.2.3` and `linux-v1.2.3` must
 * each be invisible to the other's version-floor and ancestry lookups, or a
 * Linux release would satisfy a macOS "already promoted" check.
 */
export function tagBelongsToPlatform(descriptor: ReleasePlatformDescriptor, tag: string): boolean {
  const linuxShaped = tag.startsWith("linux-v");
  return descriptor.platform === "linux" ? linuxShaped : !linuxShaped && tag.startsWith("v");
}
