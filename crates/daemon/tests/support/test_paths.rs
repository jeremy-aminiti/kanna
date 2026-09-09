//! Test-only paths under the shared temp directory, in the shape of
//! `crates/kanna-server/src/test_paths.rs`.
//!
//! Every path lives under a root this process alone owns, named for its pid,
//! so no two concurrent gates can name the same file — and so the root is
//! something the *next* run can recognise and delete. A test that is killed
//! mid-run never reaches its own cleanup; a name unique to the moment it ran
//! (a nanosecond stamp) is one nothing will ever reclaim. The Mac Studio
//! sweep of 2026-09-09 found this crate's handoff and worktree-isolation
//! fixtures among the largest survivors in `/private/tmp`, 20–25 MB each.
//!
//! The root prefix is the one `kanna-server` uses, so either crate's sweep
//! collects the other's abandoned roots: one naming rule, one mechanism.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Separates callers within this process; the root's pid separates the processes.
static NEXT_TEST_PATH_ID: AtomicU64 = AtomicU64::new(0);

const ROOT_PREFIX: &str = "kanna-test-";

/// The directory this process owns. Created once, swept once.
fn test_temp_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("{ROOT_PREFIX}{}", std::process::id()));
        // A long-dead run holding this pid may have left its tree here, and its
        // files must not be mistaken for ours.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test temp root");
        sweep_abandoned_roots();
        root
    })
}

/// Deletes the roots of runs that are over, and only those.
///
/// A pid whose process is gone cannot come back, so its tree is garbage. A pid
/// that answers is either a live gate from another worktree or something else
/// entirely that has taken the number over; both are left alone, and the next
/// run collects whatever this one skipped.
fn sweep_abandoned_roots() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(ROOT_PREFIX))
        else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        if pid == std::process::id() as i32 || process_is_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

fn process_is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission and existence checks without
    // delivering anything.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // `EPERM` is a live process owned by somebody else; only `ESRCH` is gone.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// A unique path under this process's root. Nothing is created.
pub fn unique_test_path(label: &str) -> PathBuf {
    test_temp_root().join(format!(
        "{label}-{}",
        NEXT_TEST_PATH_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A unique directory under this process's root, created.
pub fn unique_test_dir(label: &str) -> PathBuf {
    let dir = unique_test_path(label);
    std::fs::create_dir_all(&dir).expect("create test temp directory");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_test_path_sits_in_this_process_root_under_a_fresh_name() {
        let first = unique_test_path("collision-probe");
        let second = unique_test_path("collision-probe");

        assert_ne!(first, second);
        let root = std::env::temp_dir().join(format!("{ROOT_PREFIX}{}", std::process::id()));
        for path in [&first, &second] {
            assert_eq!(path.parent(), Some(root.as_path()));
        }
        assert!(root.is_dir());
        assert!(unique_test_dir("collision-probe-dir").is_dir());
    }

    #[test]
    fn the_sweep_collects_a_finished_run_and_spares_a_live_one() {
        let temp = std::env::temp_dir();
        // Above macOS's pid ceiling, so nothing can be running as it.
        let dead_pid = 999_000;
        assert!(!process_is_alive(dead_pid));

        // shared-temp-path: this module is the mechanism the contract enforces
        let abandoned = temp.join(format!("{ROOT_PREFIX}{dead_pid}"));
        std::fs::create_dir_all(&abandoned).expect("create abandoned root");
        let ours = unique_test_dir("sweep-probe");

        sweep_abandoned_roots();

        assert!(
            !abandoned.exists(),
            "a finished run's root must be reclaimed"
        );
        assert!(ours.is_dir(), "a live run's files must survive");
    }
}
