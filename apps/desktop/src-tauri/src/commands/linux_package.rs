//! What the package manager says about this installation.
//!
//! On Linux, Kanna does not update itself: the deb comes from a signed apt
//! archive and `apt` performs the upgrade. That is a deliberate choice, and it
//! makes the desktop's job here narrow and unusual — the UI has to tell the
//! truth about a process it does not control.
//!
//! Everything in this module is therefore **read-only**. It never runs `apt
//! update`, never installs, never asks for root, and never downloads. It reads
//! two things a normal user can read — what dpkg has installed, and what apt's
//! *already cached* index offers — and reports them, including reporting
//! honestly when it cannot tell. A UI that guessed here would either offer an
//! upgrade that does not exist or hide one that does.
//!
//! `LC_ALL=C` on every invocation: these outputs are localized, and a parser
//! that worked in English and silently returned "no update" in German would be
//! indistinguishable from a machine that is up to date.

use serde::Serialize;
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LinuxPackageStatus {
    /// Does a package manager own this installation's updates?
    ///
    /// Answered by the binary rather than by the renderer, because the
    /// renderer's platform string describes the webview and the question is
    /// about how this installation was *delivered*. It is also the one flag
    /// the update UI branches on, so having it come from the same place as the
    /// versions keeps them from ever disagreeing.
    pub package_managed: bool,
    pub package_name: String,
    pub installed_version: Option<String>,
    /// What apt would install from its cached index. `None` when the machine's
    /// index has never been fetched or no longer lists the package.
    pub candidate_version: Option<String>,
    pub update_available: bool,
    /// True when the answer above is "we cannot tell", not "you are current".
    /// The UI must say so rather than showing a reassuring nothing.
    pub metadata_unavailable: bool,
    /// Why, in words a person can act on.
    pub detail: Option<String>,
}

impl LinuxPackageStatus {
    fn unknown(package_name: &str, detail: &str) -> Self {
        Self {
            package_managed: cfg!(target_os = "linux"),
            package_name: package_name.to_string(),
            installed_version: None,
            candidate_version: None,
            update_available: false,
            metadata_unavailable: true,
            detail: Some(detail.to_string()),
        }
    }
}

/// The package name for the running instance, from the installed layout.
///
/// Derived from the executable's own path rather than from a build-time
/// constant, so a staging build running from a production prefix — or a dev
/// binary running from a checkout — reports what it actually is.
#[cfg(target_os = "linux")]
fn package_name_for_current_exe() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    kanna_runtime_defaults::linux_install::installed_channel_for_exe(&exe)
        .map(|channel| channel.package_name().to_string())
}

// Only the Linux `status()` calls these; on other platforms they are compiled
// so the parsing rules stay under `cargo test` everywhere, which is where the
// locale and `(none)` cases are actually exercised.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn run_c_locale(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// apt's candidate for a package, from `apt-cache policy`'s already-cached
/// index. Never triggers a fetch.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_candidate(output: &str) -> Option<String> {
    let value = output
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Candidate:"))?
        .trim();
    // apt prints `(none)` when the package is known but has no installable
    // version — an answer, and not a version.
    if value.is_empty() || value == "(none)" {
        return None;
    }
    Some(value.to_string())
}

/// Is `candidate` newer than `installed`?
///
/// Answered by `dpkg --compare-versions`, which is the only implementation of
/// Debian's ordering that is definitionally correct. Reimplementing it would
/// mean reimplementing `~`-sorting, epochs and the digit/non-digit alternation
/// — and getting `~` wrong is exactly what would offer a staging build as an
/// upgrade to a production machine.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn dpkg_says_newer(installed: &str, candidate: &str) -> bool {
    Command::new("dpkg")
        .args(["--compare-versions", candidate, "gt", installed])
        .env("LC_ALL", "C")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub fn status() -> LinuxPackageStatus {
    let Some(package_name) = package_name_for_current_exe() else {
        return LinuxPackageStatus::unknown(
            "kanna",
            "This build is not running from an installed package, so there is no package to check.",
        );
    };

    let installed = run_c_locale("dpkg-query", &["-W", "-f=${Version}", &package_name])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let candidate = run_c_locale("apt-cache", &["policy", &package_name])
        .and_then(|output| parse_candidate(&output));

    match (&installed, &candidate) {
        (Some(installed), Some(candidate)) => LinuxPackageStatus {
            package_managed: true,
            package_name,
            installed_version: Some(installed.clone()),
            candidate_version: Some(candidate.clone()),
            update_available: dpkg_says_newer(installed, candidate),
            metadata_unavailable: false,
            detail: None,
        },
        (Some(installed), None) => LinuxPackageStatus {
            package_managed: true,
            package_name,
            installed_version: Some(installed.clone()),
            candidate_version: None,
            update_available: false,
            metadata_unavailable: true,
            // The honest reading: apt has nothing cached for this package, so
            // "no update" would be a guess. Refreshing needs root, which is
            // the user's to do.
            detail: Some(
                "Your package index has no entry for this package. Run `sudo apt update` to refresh it.".to_string(),
            ),
        },
        _ => LinuxPackageStatus::unknown(
            &package_name,
            "dpkg does not report this package as installed.",
        ),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn status() -> LinuxPackageStatus {
    let mut status =
        LinuxPackageStatus::unknown("kanna", "Package-manager updates are a Linux path.");
    status.package_managed = false;
    status
}

#[tauri::command]
pub fn linux_package_status() -> LinuxPackageStatus {
    status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_candidate_out_of_apt_policy_output() {
        let output = "kanna:\n  Installed: 1.2.3-1\n  Candidate: 1.2.4-1\n  Version table:\n";
        assert_eq!(parse_candidate(output).as_deref(), Some("1.2.4-1"));
    }

    /// `(none)` is apt's answer for "known package, nothing installable". It is
    /// information, not a version, and treating it as one would show the user a
    /// literal "(none)" as an available update.
    #[test]
    fn treats_no_installable_candidate_as_absent() {
        assert_eq!(parse_candidate("kanna:\n  Candidate: (none)\n"), None);
        assert_eq!(parse_candidate("kanna:\n  Candidate:\n"), None);
        assert_eq!(parse_candidate("N: Unable to locate package kanna\n"), None);
    }

    #[test]
    fn an_uninstalled_build_reports_that_rather_than_being_up_to_date() {
        let status = LinuxPackageStatus::unknown("kanna", "not installed");
        assert!(status.metadata_unavailable);
        assert!(!status.update_available);
        assert!(status.detail.is_some());
    }

    /// The flag the update UI branches on. On macOS it must be false, or the
    /// self-updater path — the only one that works there — would be skipped.
    #[test]
    fn package_management_follows_the_platform_the_binary_was_built_for() {
        assert_eq!(status().package_managed, cfg!(target_os = "linux"));
    }
}
