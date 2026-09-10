//! On Linux, the package manager owns updates — and that has to be true of the
//! *capability*, not only of the button.
//!
//! Hiding the Install action would leave `plugin:updater` registered and
//! reachable. Anything that got to it — a renderer bug, a stale code path, an
//! E2E hook — could download and install over a dpkg-managed installation,
//! replacing files the package manager believes it owns and leaving the machine
//! unable to upgrade itself afterwards. The plugin is therefore not registered
//! at all on Linux.
//!
//! These are source-level assertions and they say so. A behavioural test would
//! need a running Linux app and a live update endpoint; what this catches is the
//! regression that is otherwise completely silent — someone moving the
//! registration back out of its `cfg`, where every other test in the repository
//! still passes and only a Linux user finds out.

use std::path::PathBuf;

fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read {path:?}: {error}"))
}

#[test]
fn the_self_updater_plugin_is_not_registered_on_linux() {
    let lib = source("src/lib.rs");
    let registration = lib
        .find("tauri_plugin_updater::Builder::new()")
        .expect("the updater plugin registration should still exist for the platforms that use it");
    let guard = lib[..registration]
        .rfind("#[cfg(not(target_os = \"linux\"))]")
        .expect("the updater plugin must be registered behind a not(linux) cfg");
    // The guard has to be the *nearest* preceding cfg, or it is guarding
    // something else and the registration is unconditional again.
    assert!(
        !lib[guard..registration].contains("builder = builder.plugin(tauri_plugin_updater"),
        "the not(linux) guard must be the one that gates the updater registration"
    );
    assert!(
        lib[guard..registration].len() < 800,
        "the not(linux) guard drifted away from the updater registration; it may no longer gate it"
    );
}

/// The other half of the same decision: the read-only status command exists and
/// is exposed, so the UI has something truthful to show instead of an updater.
#[test]
fn the_read_only_package_status_command_is_exposed() {
    assert!(source("src/lib.rs").contains("commands::linux_package::linux_package_status"));
}

/// The command must stay read-only. Anything that installs, refreshes the index
/// or escalates turns a status reader into an update mechanism competing with
/// the package manager.
#[test]
fn the_package_status_command_never_writes() {
    let module = source("src/commands/linux_package.rs");
    for forbidden in ["apt-get", "sudo", "pkexec", "apt update", "install"] {
        assert!(
            !module.contains(&format!("\"{forbidden}\"")),
            "linux_package.rs must not invoke {forbidden}: this path only reads"
        );
    }
    // The two readers it is allowed to be.
    assert!(module.contains("\"dpkg-query\""));
    assert!(module.contains("\"apt-cache\""));
}
