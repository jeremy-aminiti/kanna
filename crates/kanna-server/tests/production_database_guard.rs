//! Real server startup regression for the XDG_DATA_HOME-on-macOS incident.
//! The OS sandbox is independent of Kanna's guard: even running this test on
//! pre-guard code cannot read or write the owner's SQLite database.
#![cfg(target_os = "macos")]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use kanna_runtime_defaults::database_access;
use kanna_runtime_defaults::{DESKTOP_BUNDLE_IDENTIFIER, STAGING_DESKTOP_BUNDLE_IDENTIFIER};

const APP_SUPPORT: &str = "Library/Application Support";
const XDG_SHARE: &str = ".local/share";

/// Pick a guarded path by the identifier and root it belongs to. The guarded
/// set is derived from the identifier constants, so its order is not a
/// contract and nothing here may index into it.
fn guarded(identifier: &str, root: &str) -> PathBuf {
    let directory = OsStr::new(identifier);
    database_access::production_database_paths()
        .unwrap()
        .into_iter()
        .find(|path| {
            path.parent().and_then(Path::file_name) == Some(directory)
                && path.to_string_lossy().contains(root)
        })
        .unwrap_or_else(|| panic!("{identifier} under {root} must be guarded"))
}

/// The OS account the guard protects, read back off a guarded path rather than
/// from HOME, which the surrounding test runner may have moved.
fn account_home() -> PathBuf {
    guarded(DESKTOP_BUNDLE_IDENTIFIER, APP_SUPPORT)
        .ancestors()
        .nth(4)
        .unwrap()
        .to_path_buf()
}

/// The OS fence this regression runs behind, independent of Kanna's own guard:
/// no SQLite file anywhere may be read or written, and nothing inside the
/// account's real desktop data directories may be touched at all. Even on
/// pre-guard code, and even in the cases below that deliberately authorize the
/// access, the owner's databases -- including the staging desktop's daily
/// driver -- stay unreadable and unwritten.
fn sandbox_profile() -> String {
    let mut profile = String::from(
        "(version 1)\n(allow default)\n(deny file-read-data file-write* (regex #\".*\\.(db|sqlite)(-wal|-shm|-journal)?$\"))\n",
    );
    for path in database_access::production_database_paths().unwrap() {
        profile.push_str(&format!(
            "(deny file-read-data file-write* (subpath \"{}\"))\n",
            path.parent().expect("guarded path has a parent").display()
        ));
    }
    profile
}

/// Prove the fence before running anything against it, and again afterwards: a
/// write to an ordinary database name is refused, and not one byte of any
/// guarded path can be read -- because the fence denies it, or because the
/// path does not exist on this machine.
fn assert_fence_holds(profile: &str, fixture: &Path) {
    let sentinel = fixture.join("fence.db");
    std::fs::write(&sentinel, b"untouched").unwrap();
    let fence = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", profile, "/usr/bin/touch"])
        .arg(&sentinel)
        .output()
        .expect("macOS sandbox must be available before attempting the regression");
    assert!(
        !fence.status.success(),
        "OS database fence must reject writes"
    );
    for path in database_access::production_database_paths().unwrap() {
        let read = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile, "/usr/bin/head", "-c", "1"])
            .arg(&path)
            .output()
            .expect("sandbox must launch the fence probe");
        assert!(
            !read.status.success(),
            "OS database fence must reject reads of {}",
            path.display()
        );
    }
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched");
}

#[test]
fn installed_worker_fallback_requires_desktop_authorization_without_any_test_context() {
    let fixture = tempfile::tempdir().unwrap();
    // Model an installed executable, not a binary whose worktree location
    // independently triggers the isolation veto.
    let server = fixture.path().join("kanna-server");
    std::fs::copy(env!("CARGO_BIN_EXE_kanna-server"), &server).unwrap();
    assert!(kanna_runtime_defaults::worktree_root_for_path(&server).is_none());
    let account_home = account_home();
    let config = fixture.path().join("server.toml");
    let profile = sandbox_profile();
    assert_fence_holds(&profile, fixture.path());

    // A worker that loses --db-path materializes the canonical fallback in
    // server.toml. Exercise both platform paths for the shipped app and for
    // staging -- the owner's daily driver, whose database is just as real --
    // plus the server's own omitted selection. Naming one of them explicitly
    // is not desktop authorization.
    for db_path in [
        None,
        Some(guarded(DESKTOP_BUNDLE_IDENTIFIER, APP_SUPPORT)),
        Some(guarded(DESKTOP_BUNDLE_IDENTIFIER, XDG_SHARE)),
        Some(guarded(STAGING_DESKTOP_BUNDLE_IDENTIFIER, APP_SUPPORT)),
        Some(guarded(STAGING_DESKTOP_BUNDLE_IDENTIFIER, XDG_SHARE)),
    ] {
        let selection = db_path
            .map(|path| format!("db_path = {:?}\n", path.to_str().unwrap()))
            .unwrap_or_default();
        std::fs::write(&config, format!(
            "relay_url = \"\"\ndevice_token = \"\"\nversion = \"test\"\nenvironment = \"development\"\ntransfer_port = 4455\ndaemon_dir = {:?}\n{selection}",
            fixture.path().join("daemon").to_str().unwrap()
        )).unwrap();
        let output = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile.as_str()])
            .arg(&server)
            .env_clear()
            .env("HOME", &account_home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("KANNA_SERVER_CONFIG", &config)
            .current_dir(fixture.path())
            .output()
            .expect("installed server should launch");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "server must refuse startup");
        assert!(
            stderr.contains("requires deliberate KANNA_DESKTOP_DB_ACCESS=desktop authorization"),
            "must refuse for missing authorization, independently of test context: {stderr}"
        );
    }
    assert_fence_holds(&profile, fixture.path());
}

#[test]
fn xdg_only_test_isolation_cannot_open_the_desktop_database_even_with_inherited_authorization() {
    let fixture = tempfile::tempdir().unwrap();
    // Neither executable nor cwd may identify a worktree: this test must fail
    // if the macOS XDG veto is removed, even with desktop authorization.
    let server = fixture.path().join("kanna-server");
    std::fs::copy(env!("CARGO_BIN_EXE_kanna-server"), &server).unwrap();
    assert!(kanna_runtime_defaults::worktree_root_for_path(&server).is_none());
    // Use the OS account even when the surrounding test runner changed HOME.
    let account_home = account_home();
    let config = fixture.path().join("server.toml");
    std::fs::write(&config, format!(
        "relay_url = \"\"\ndevice_token = \"\"\nversion = \"test\"\nenvironment = \"development\"\ntransfer_port = 4455\ndaemon_dir = {:?}\n",
        fixture.path().join("daemon").to_str().unwrap()
    )).unwrap();

    // Deny all SQLite access, regardless of the eventual path. Configuration
    // and dynamic libraries remain readable. This is a test safety fence, not
    // the product protection being asserted below.
    let profile = sandbox_profile();
    assert_fence_holds(&profile, fixture.path());

    for inherited_desktop_authorization in [false, true] {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .args(["-p", profile.as_str()])
            .arg(&server)
            .env_clear()
            .env("HOME", &account_home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("KANNA_SERVER_CONFIG", &config)
            .env("XDG_DATA_HOME", fixture.path().join("xdg"))
            .current_dir(fixture.path());
        if inherited_desktop_authorization {
            command.env("KANNA_DESKTOP_DB_ACCESS", "desktop");
        }
        let output = command.output().expect("server should launch");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "server must refuse startup");
        assert!(
            stderr.contains("REFUSED: isolated/test/worktree process"),
            "{stderr}"
        );
        assert!(
            stderr.contains("XDG_DATA_HOME does not isolate macOS"),
            "{stderr}"
        );
    }
    assert_fence_holds(&profile, fixture.path());
}

#[path = "../src/worktree_cleanup/command.rs"]
mod cleanup_command;

#[test]
fn close_cleanup_restores_authorized_server_context_after_isolated_teardown() {
    // Re-exec outside the checkout, with the environment of an installed
    // desktop server. The test runner itself remains isolated.
    let fixture = tempfile::tempdir().unwrap();
    if std::env::var_os("KANNA_CLEANUP_GUARD_PROBE").is_none() {
        let probe = fixture.path().join("guard-test");
        std::fs::copy(std::env::current_exe().unwrap(), &probe).unwrap();
        let output = Command::new(probe)
            .args([
                "--exact",
                "close_cleanup_restores_authorized_server_context_after_isolated_teardown",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("KANNA_DESKTOP_DB_ACCESS", "desktop")
            .env("KANNA_CLEANUP_GUARD_PROBE", "1")
            .current_dir(fixture.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let server = fixture.path().join("kanna-server");
    std::fs::copy(env!("CARGO_BIN_EXE_kanna-server"), &server).unwrap();
    let repo = fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    };
    git(&["init"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ]);
    let worktree = repo.join(".kanna-worktrees/task-cleanup");
    git(&[
        "worktree",
        "add",
        "-b",
        "task-cleanup",
        worktree.to_str().unwrap(),
    ]);
    let profile = sandbox_profile();
    assert_fence_holds(&profile, fixture.path());

    // This is the command builder used by append_close_cleanup_to_teardown.
    // The preceding repo teardown sees task isolation; only cleanup returns
    // to the authorized parent context and leaves the git worktree directory.
    // Closing a task on the staging desktop must survive its own guard exactly
    // as it does on the shipped one.
    for identifier in [DESKTOP_BUNDLE_IDENTIFIER, STAGING_DESKTOP_BUNDLE_IDENTIFIER] {
        let cleanup = cleanup_command::cleanup_shell_command(
            server.to_str().unwrap(),
            guarded(identifier, APP_SUPPORT).to_str().unwrap(),
            repo.to_str().unwrap(),
            "missing-test-task",
        );
        let command = format!("test \"$KANNA_TASK_ID\" = task-cleanup && test \"$KANNA_WORKTREE\" = 1 || exit 90; {cleanup}");
        for isolated in [false, true] {
            let mut child = Command::new("/usr/bin/sandbox-exec");
            child
                .args(["-p", profile.as_str(), "/bin/sh", "-c", &command])
                .env("KANNA_TASK_ID", "task-cleanup")
                .env("KANNA_WORKTREE", "1")
                .current_dir(&worktree);
            if isolated {
                child.env("XDG_DATA_HOME", fixture.path().join("xdg"));
            }
            let output = child.output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success());
            if isolated {
                assert!(
                    stderr.contains("REFUSED: isolated/test/worktree"),
                    "{identifier}: {stderr}"
                );
            } else {
                // Reaching SQLite's OS fence proves the actual cleanup
                // environment passed database_access::check. No production
                // SQLite access occurs.
                assert!(
                    stderr.contains("unable to open database file"),
                    "{identifier}: {stderr}"
                );
                assert!(!stderr.contains("REFUSED:"), "{identifier}: {stderr}");
            }
        }
    }
    assert_fence_holds(&profile, fixture.path());
}

/// Guarding the staging desktop must not lock the staging desktop out. This is
/// the environment `MobileServerManager::server_spawn_env` hands its sidecar --
/// desktop authorization, no isolation marker -- and the server must get past
/// the guard to SQLite, where only the OS fence stops it.
#[test]
fn authorized_desktop_server_opens_the_shipped_and_staging_databases() {
    let fixture = tempfile::tempdir().unwrap();
    let server = fixture.path().join("kanna-server");
    std::fs::copy(env!("CARGO_BIN_EXE_kanna-server"), &server).unwrap();
    assert!(kanna_runtime_defaults::worktree_root_for_path(&server).is_none());
    let account_home = account_home();
    let config = fixture.path().join("server.toml");
    let profile = sandbox_profile();
    assert_fence_holds(&profile, fixture.path());

    for identifier in [DESKTOP_BUNDLE_IDENTIFIER, STAGING_DESKTOP_BUNDLE_IDENTIFIER] {
        let db_path = guarded(identifier, APP_SUPPORT);
        std::fs::write(&config, format!(
            "relay_url = \"\"\ndevice_token = \"\"\nversion = \"test\"\nenvironment = \"development\"\ntransfer_port = 4455\ndaemon_dir = {:?}\ndb_path = {:?}\n",
            fixture.path().join("daemon").to_str().unwrap(),
            db_path.to_str().unwrap()
        )).unwrap();
        let output = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile.as_str()])
            .arg(&server)
            .env_clear()
            .env("HOME", &account_home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("KANNA_SERVER_CONFIG", &config)
            .env(database_access::DESKTOP_ACCESS_ENV, "desktop")
            .current_dir(fixture.path())
            .output()
            .expect("installed server should launch");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{identifier}: the OS fence must stop the server short of SQLite"
        );
        assert!(
            !stderr.contains("REFUSED:"),
            "{identifier}: an authorized desktop server must pass the guard: {stderr}"
        );
        assert!(
            stderr.contains("Failed to open database at"),
            "{identifier}: the server must have reached SQLite: {stderr}"
        );
    }
    assert_fence_holds(&profile, fixture.path());
}
