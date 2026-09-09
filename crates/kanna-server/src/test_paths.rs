//! Test-only paths under the shared temp directory.
//!
//! Several tasks' gates run at once on one machine, each from its own worktree,
//! so a test path built from a label alone names the same file in every one of
//! those runs. A per-process counter does not save it: every process starts its
//! counter at the same value, so two runs hand out the same names in the same
//! order. `Db::open_for_tests` deletes the file it is given, so the collision is
//! not a shared read — one run truncates another run's database mid-test, and
//! the failures surface as `table repo already exists`, `no such table`, `disk
//! I/O error`, or an assertion reading the *other* run's fixture.
//!
//! Every test path therefore lives under a root this process alone owns, named
//! for its pid. Two concurrent processes cannot share a pid and one process
//! cannot reuse a counter, so no two callers anywhere can name the same path —
//! without depending on a wall clock, whose resolution is a probability rather
//! than a guarantee. `std::env::temp_dir` honours `TMPDIR`.
//!
//! The root is also what makes the space reclaimable. Uniqueness without
//! reclamation is a leak: a suite that hands out a fresh database per test and
//! never takes one back left six figures of files in this machine's temp
//! directory and eventually failed a gate with `ENOSPC`. One directory per
//! process is a thing the *next* run can recognise and delete, which is what
//! [`sweep_abandoned_roots`] does — never touching a root whose process is
//! still alive, so a concurrent gate is safe from it.

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

/// A name no other caller — in this process or any other — produces.
pub(crate) fn unique_test_name(label: &str) -> String {
    format!(
        "{label}-{}-{}",
        std::process::id(),
        NEXT_TEST_PATH_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// A unique path under this process's root. Nothing is created.
pub(crate) fn unique_test_path(label: &str) -> PathBuf {
    test_temp_root().join(format!(
        "{label}-{}",
        NEXT_TEST_PATH_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

/// [`unique_test_path`] as a string, for the `Config` fields that hold one.
pub(crate) fn unique_test_path_string(label: &str) -> String {
    unique_test_path(label).to_string_lossy().to_string()
}

/// A unique file path with the given extension, as a string. Nothing is created.
pub(crate) fn unique_test_file(label: &str, extension: &str) -> String {
    let mut path = unique_test_path(label).into_os_string();
    path.push(".");
    path.push(extension);
    path.to_string_lossy().to_string()
}

/// A unique directory under this process's root, created.
pub(crate) fn unique_test_dir(label: &str) -> PathBuf {
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
            assert_eq!(
                path.parent(),
                Some(root.as_path()),
                "a concurrent run must not be able to produce {}",
                path.display()
            );
        }
        assert!(root.is_dir());
    }

    #[test]
    fn files_and_directories_share_the_same_uniqueness() {
        let file = unique_test_file("collision-probe", "sqlite");
        let other = unique_test_file("collision-probe", "sqlite");

        assert_ne!(file, other);
        assert!(file.ends_with(".sqlite"));
        assert!(file.contains("collision-probe-"));

        let dir = unique_test_dir("collision-probe-dir");
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sweep is what keeps the temp directory from growing without bound,
    /// and it runs beside other worktrees' gates — so it must be able to tell
    /// a finished run from a running one.
    #[test]
    fn the_sweep_collects_a_finished_run_and_spares_a_live_one() {
        let temp = std::env::temp_dir();
        // Above macOS's pid ceiling, so nothing can be running as it.
        let dead_pid = 999_000;
        assert!(!process_is_alive(dead_pid));

        // shared-temp-path: this module is the mechanism the contract enforces
        let abandoned = temp.join(format!("{ROOT_PREFIX}{dead_pid}"));
        let live = temp.join(format!("{ROOT_PREFIX}{}", std::process::id()));
        std::fs::create_dir_all(&abandoned).expect("create abandoned root");
        let ours = unique_test_dir("sweep-probe");

        sweep_abandoned_roots();

        assert!(
            !abandoned.exists(),
            "a finished run's root must be reclaimed"
        );
        assert!(live.is_dir(), "this process's own root must survive");
        assert!(ours.is_dir(), "a live run's files must survive");
    }

    #[test]
    fn a_name_without_a_path_still_carries_the_process() {
        let name = unique_test_name("probe");

        assert!(name.starts_with(&format!("probe-{}-", std::process::id())));
        assert_ne!(name, unique_test_name("probe"));
    }
}
