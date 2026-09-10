//! The installed Linux layout: one contract, shared by everything that has to
//! find an installed Kanna file.
//!
//! On macOS the answer is "the app bundle", and `current_exe()`'s directory
//! plus `../Resources` covers every consumer. A Linux package has no bundle:
//! the desktop binary, the worker, the six sidecars, the built-in `.kanna/`
//! definitions and the desktop-entry icon are all separate files that the
//! packaging step places and the runtime later has to rediscover. Writing
//! those paths twice — once in the package build and once in the code that
//! reads them — is how an installed build ends up looking for a sidecar the
//! package put somewhere else, which no unit test on the build machine can
//! catch.
//!
//! So the layout is declared once, here, and both sides consume it: `kd`'s
//! packaging step lays files down at these paths, and the runtime resolves
//! them from the same functions. `tools/kd/src/runtime/linux-package.ts`
//! mirrors the same constants for the build side and a contract test holds the
//! two in step.
//!
//! Two package names may be installed at once — `kanna` and `kanna-staging` —
//! and they share no file and no state. That is why the channel is part of the
//! layout rather than something the runtime infers later: staging installing
//! over production's files would be indistinguishable from an upgrade.

use std::path::{Path, PathBuf};

/// Which installed instance a path belongs to. Production and staging are
/// separate packages with separate prefixes, separate desktop entries and
/// separate data directories; nothing is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxInstallChannel {
    Production,
    Staging,
}

impl LinuxInstallChannel {
    /// The Debian package name. Also the directory name under the library
    /// prefix and the stem of the `/usr/bin` launcher, so one string names the
    /// whole installation.
    pub fn package_name(self) -> &'static str {
        match self {
            Self::Production => "kanna",
            Self::Staging => "kanna-staging",
        }
    }

    /// The desktop-entry id, which is also the bundle identifier the desktop
    /// already uses to pick its cloud environment and data directory. Keeping
    /// them equal is what makes an installed staging app resolve
    /// `build.kanna.staging/` rather than production's directory.
    pub fn desktop_entry_id(self) -> &'static str {
        match self {
            Self::Production => crate::DESKTOP_BUNDLE_IDENTIFIER,
            Self::Staging => crate::STAGING_DESKTOP_BUNDLE_IDENTIFIER,
        }
    }

    pub fn cloud_environment(self) -> crate::DesktopCloudEnvironment {
        match self {
            Self::Production => crate::DesktopCloudEnvironment::Production,
            Self::Staging => crate::DesktopCloudEnvironment::Staging,
        }
    }

    /// The `systemd --user` unit that starts this channel's worker. Two
    /// channels must not share a unit name or `systemctl --user restart` on
    /// one would stop the other's supervisor.
    pub fn worker_unit_name(self) -> &'static str {
        match self {
            Self::Production => "kanna-worker.service",
            Self::Staging => "kanna-staging-worker.service",
        }
    }

    pub fn from_package_name(name: &str) -> Option<Self> {
        match name {
            "kanna" => Some(Self::Production),
            "kanna-staging" => Some(Self::Staging),
            _ => None,
        }
    }
}

/// Where a channel's private files live: every Kanna-owned executable and the
/// built-in resource tree, as siblings.
///
/// Siblings is not an aesthetic choice. `sidecar_candidates_for_exe` already
/// looks beside the running executable first, so an installed desktop finds an
/// installed `kanna-daemon` through the path it already used on macOS, with no
/// installed-only branch to keep working.
pub fn install_lib_dir(prefix: &Path, channel: LinuxInstallChannel) -> PathBuf {
    prefix.join("lib").join(channel.package_name())
}

/// The desktop executable's installed path.
pub fn installed_desktop_binary(prefix: &Path, channel: LinuxInstallChannel) -> PathBuf {
    install_lib_dir(prefix, channel).join(DESKTOP_BINARY_NAME)
}

/// The `PATH` launcher. A symlink into the library directory rather than a
/// copy, so `current_exe()` resolves to the real sibling directory and the
/// candidate search above still finds the sidecars.
pub fn installed_launcher(prefix: &Path, channel: LinuxInstallChannel) -> PathBuf {
    prefix.join("bin").join(channel.package_name())
}

/// The built-in agent/workflow/task definitions, shipped as files exactly as
/// they are on macOS.
pub fn installed_resource_dir(prefix: &Path, channel: LinuxInstallChannel) -> PathBuf {
    install_lib_dir(prefix, channel)
}

pub fn installed_desktop_entry(prefix: &Path, channel: LinuxInstallChannel) -> PathBuf {
    prefix
        .join("share")
        .join("applications")
        .join(format!("{}.desktop", channel.desktop_entry_id()))
}

pub fn installed_icon(prefix: &Path, channel: LinuxInstallChannel, size: u32) -> PathBuf {
    prefix
        .join("share")
        .join("icons")
        .join("hicolor")
        .join(format!("{size}x{size}"))
        .join("apps")
        .join(format!("{}.png", channel.desktop_entry_id()))
}

/// The installed desktop binary's name. Distinct from the package name so that
/// `/usr/bin/kanna` (the launcher) and `/usr/lib/kanna/kanna-desktop` (the real
/// executable) never collide in a `PATH` lookup.
pub const DESKTOP_BINARY_NAME: &str = "kanna-desktop";

/// The default install prefix. Debian's `/usr`; overridable so a test can lay
/// the same tree into a temporary root.
pub const DEFAULT_INSTALL_PREFIX: &str = "/usr";

/// Executables a package ships beside the desktop binary. The six sidecars the
/// desktop already spawns, plus `kanna-worker`, which is Kanna-owned and
/// therefore bundled rather than assumed present.
pub const INSTALLED_EXECUTABLES: [&str; 8] = [
    DESKTOP_BINARY_NAME,
    "kanna-worker",
    "kanna-daemon",
    "kanna-cli",
    "kanna-mcp",
    "kanna-server",
    "kanna-task-transfer",
    "kanna-terminal-recovery",
];

/// Icon sizes the package installs. The set the repo actually has PNGs for.
pub const INSTALLED_ICON_SIZES: [u32; 3] = [32, 64, 128];

/// Is this executable running from an installed package rather than a build
/// tree?
///
/// Answered from the executable's own location, not from an environment
/// variable a dev shell might carry into a packaged run. A caller uses it to
/// decide whether to look for repo-relative files at all.
pub fn installed_channel_for_exe(current_exe: &Path) -> Option<LinuxInstallChannel> {
    let dir = current_exe.parent()?;
    let name = dir.file_name()?.to_str()?;
    let channel = LinuxInstallChannel::from_package_name(name)?;
    // `.../lib/<package>` — the parent must be a `lib` directory, or a source
    // checkout that happens to have a directory named `kanna` would match.
    if dir.parent()?.file_name()?.to_str()? != "lib" {
        return None;
    }
    Some(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_channels_share_no_installed_path() {
        let prefix = Path::new("/usr");
        for (production, staging) in [
            (
                installed_desktop_binary(prefix, LinuxInstallChannel::Production),
                installed_desktop_binary(prefix, LinuxInstallChannel::Staging),
            ),
            (
                installed_launcher(prefix, LinuxInstallChannel::Production),
                installed_launcher(prefix, LinuxInstallChannel::Staging),
            ),
            (
                installed_desktop_entry(prefix, LinuxInstallChannel::Production),
                installed_desktop_entry(prefix, LinuxInstallChannel::Staging),
            ),
            (
                installed_icon(prefix, LinuxInstallChannel::Production, 128),
                installed_icon(prefix, LinuxInstallChannel::Staging, 128),
            ),
        ] {
            assert_ne!(production, staging);
        }
        assert_ne!(
            LinuxInstallChannel::Production.worker_unit_name(),
            LinuxInstallChannel::Staging.worker_unit_name()
        );
    }

    /// The layout's whole reason to put executables side by side: the existing
    /// candidate search finds them with no installed-only branch.
    #[test]
    fn sidecars_are_found_beside_the_installed_desktop_binary() {
        let exe = installed_desktop_binary(Path::new("/usr"), LinuxInstallChannel::Production);
        let candidates = crate::sidecar_candidates_for_exe(&exe, "kanna-daemon");
        assert!(candidates.contains(&PathBuf::from("/usr/lib/kanna/kanna-daemon")));
    }

    #[test]
    fn an_installed_exe_is_recognised_and_a_build_tree_one_is_not() {
        assert_eq!(
            installed_channel_for_exe(Path::new("/usr/lib/kanna/kanna-desktop")),
            Some(LinuxInstallChannel::Production)
        );
        assert_eq!(
            installed_channel_for_exe(Path::new("/usr/lib/kanna-staging/kanna-desktop")),
            Some(LinuxInstallChannel::Staging)
        );
        assert_eq!(
            installed_channel_for_exe(Path::new("/home/me/kanna/.build/debug/kanna-desktop")),
            None
        );
        // A checkout directory named `kanna` is not an install prefix.
        assert_eq!(
            installed_channel_for_exe(Path::new("/home/me/src/kanna/kanna-desktop")),
            None
        );
    }

    #[test]
    fn each_channel_resolves_its_own_data_directory() {
        assert_ne!(
            LinuxInstallChannel::Production.desktop_entry_id(),
            LinuxInstallChannel::Staging.desktop_entry_id()
        );
        assert_eq!(
            LinuxInstallChannel::Staging.cloud_environment(),
            crate::DesktopCloudEnvironment::Staging
        );
    }
}
