//! Permission to open the desktop database is separate from path resolution.
//!
//! Call immediately before SQLite opens a file, and before relocation modifies
//! either source or destination. Merely naming a path is never authorization.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DESKTOP_ACCESS_ENV: &str = "KANNA_DESKTOP_DB_ACCESS";
pub const ISOLATED_ENV: &str = "KANNA_DB_ISOLATED";

/// Every bundle identifier whose default database holds real desktop data for
/// this account: the shipped app, the staging app -- an owner's daily driver,
/// not a scratch instance -- and the pre-rename identifier still holding data.
///
/// Derived from the identifier constants rather than restating their strings,
/// so renaming one moves its protection with it instead of silently leaving
/// the database it names open to any process that resolves the old path.
pub const PROTECTED_BUNDLE_IDENTIFIERS: [&str; 3] = [
    crate::DESKTOP_BUNDLE_IDENTIFIER,
    crate::STAGING_DESKTOP_BUNDLE_IDENTIFIER,
    crate::LEGACY_DESKTOP_BUNDLE_IDENTIFIER,
];

/// Account-relative roots holding those identifiers' data directories: macOS
/// Application Support and the Linux default. Both are protected on both
/// platforms, so a caller cannot reach one by running the other's resolver.
const PROTECTED_ROOTS: [&str; 2] = ["Library/Application Support", ".local/share"];

/// Validate an actual database access. Test binaries must pass `true` even
/// though this library itself is compiled without `cfg(test)` for consumers.
pub fn check(path: &Path, test_binary: bool) -> Result<(), String> {
    let protected = protected_account_paths().as_ref().map_err(String::clone)?;
    let isolated = test_binary
        || std::env::var_os(ISOLATED_ENV).is_some()
        || (cfg!(target_os = "macos") && std::env::var_os("XDG_DATA_HOME").is_some())
        || std::env::var_os("KANNA_TASK_ID").is_some()
        || std::env::var_os("KANNA_WORKTREE").is_some()
        || std::env::var_os("KANNA_E2E_TEST_SQL").is_some()
        || std::env::var_os("TAURI_WEBDRIVER_PORT").is_some()
        || [std::env::current_dir(), std::env::current_exe()]
            .into_iter()
            .filter_map(Result::ok)
            .any(|path| crate::worktree_root_for_path(&path).is_some());
    let desktop = std::env::var(DESKTOP_ACCESS_ENV).as_deref() == Ok("desktop");
    check_resolved(
        path,
        &protected.paths,
        &protected.resolved,
        desktop,
        isolated,
    )
}

/// The account's protected paths together with their resolved form.
struct ProtectedPaths {
    paths: Vec<PathBuf>,
    resolved: Vec<PathBuf>,
}

/// Resolve the protected set once for the life of the process.
///
/// `check` runs immediately before every SQLite open, and re-walking the
/// account's data directories on each one made the guard's cost grow with the
/// number of identifiers it protects -- over a millisecond per open here once
/// the live staging database joined the set, which is latency every caller
/// pays. These are fixed absolute paths under an account home that cannot
/// change while the process runs, so resolving them repeatedly bought nothing.
///
/// This caches only how a *path* is spelled. Aliasing of a file that exists --
/// a symlink or hard link created after this ran -- is still caught live by
/// the inode comparison in `check_resolved`, which is the check that matters
/// once there is a file to alias.
fn protected_account_paths() -> &'static Result<ProtectedPaths, String> {
    static PROTECTED: OnceLock<Result<ProtectedPaths, String>> = OnceLock::new();
    PROTECTED.get_or_init(|| {
        let paths = production_database_paths()?;
        let resolved = paths
            .iter()
            .map(|path| resolve_existing_ancestor(path))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ProtectedPaths { paths, resolved })
    })
}

/// Locations protected for this OS account, independent of environment
/// overrides. "Production" here means a real desktop database rather than a
/// development one: the staging desktop's database is somebody's live data and
/// is protected exactly like the shipped app's. Returning these paths does not
/// authorize opening them.
pub fn production_database_paths() -> Result<Vec<PathBuf>, String> {
    // Protect the account's standard locations independently of environment
    // overrides. Temporary HOME/XDG fixture roots remain ordinary custom
    // databases; they never hide these locations.
    Ok(production_database_paths_for_home(&account_home()?))
}

/// The protected desktop database `path` names, if it names one, resolved the
/// same way [`check`] resolves it (symlinks, hard links and parent aliases
/// included). A launcher that must never open the desktop's database at all
/// asks this before it writes the path anywhere. Answering never authorizes
/// access.
pub fn protected_desktop_database(path: &Path) -> Result<Option<PathBuf>, String> {
    let protected = production_database_paths()?;
    let resolved = protected
        .iter()
        .map(|production| resolve_existing_ancestor(production))
        .collect::<Result<Vec<_>, _>>()?;
    protected_match(path, &protected, &resolved)
}

/// The same set for an explicit home, so the derivation can be exercised
/// without the account the tests are running as.
pub fn production_database_paths_for_home(home: &Path) -> Vec<PathBuf> {
    PROTECTED_ROOTS
        .iter()
        .flat_map(|root| {
            PROTECTED_BUNDLE_IDENTIFIERS
                .map(|bundle| home.join(root).join(bundle).join(crate::DEFAULT_DB_NAME))
        })
        .collect()
}

/// Resolve the protected paths for this call. The tests below drive the guard
/// through fixtures they create as they go, so their resolution stays live
/// rather than coming from the process-wide cache `check` uses.
#[cfg(test)]
fn check_against(
    path: &Path,
    protected: &[PathBuf],
    desktop: bool,
    isolated: bool,
) -> Result<(), String> {
    let resolved = protected
        .iter()
        .map(|production| resolve_existing_ancestor(production))
        .collect::<Result<Vec<_>, _>>()?;
    check_resolved(path, protected, &resolved, desktop, isolated)
}

fn check_resolved(
    path: &Path,
    protected: &[PathBuf],
    resolved_protected: &[PathBuf],
    desktop: bool,
    isolated: bool,
) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.to_string_lossy().starts_with("file:") {
        return Err(
            "REFUSED: database access requires a filesystem path, not an empty path or SQLite URI"
                .into(),
        );
    }
    if protected_match(path, protected, resolved_protected)?.is_none() {
        return Ok(());
    }
    if isolated {
        return Err(format!(
            "REFUSED: isolated/test/worktree process cannot access desktop production database {} (including legacy paths). Supply an isolated database path; XDG_DATA_HOME does not isolate macOS.",
            path.display()
        ));
    }
    if !desktop {
        return Err(format!(
            "REFUSED: opening desktop production database {} requires deliberate {DESKTOP_ACCESS_ENV}=desktop authorization. Supply an isolated database path for tests; XDG_DATA_HOME does not isolate macOS.",
            path.display()
        ));
    }
    Ok(())
}

/// Which of `protected` `path` names, by resolved path or by inode.
fn protected_match(
    path: &Path,
    protected: &[PathBuf],
    resolved_protected: &[PathBuf],
) -> Result<Option<PathBuf>, String> {
    let resolved = resolve_existing_ancestor(path)?;
    // One stat for the caller, not one per protected path: this runs before
    // every database open. `None` means there is no file yet, so no alias of
    // one can exist either and only the path comparison can match.
    let identity = file_identity(path);
    for (production, canonical_production) in protected.iter().zip(resolved_protected) {
        let same_path = resolved == *canonical_production
            || (cfg!(target_os = "macos")
                && resolved
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&canonical_production.to_string_lossy()));
        if same_path || matches!(identity, Some(id) if Some(id) == file_identity(production)) {
            return Ok(Some(production.clone()));
        }
    }
    Ok(None)
}

/// Resolve existing ancestors too: a fresh installation has no database yet,
/// and a symlink to its parent must not evade the fresh-file guard.
fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    resolve_path(path, 0)
}

fn resolve_path(path: &Path, depth: usize) -> Result<PathBuf, String> {
    if depth > 128 {
        return Err("REFUSED: database path has too many ancestors or symbolic links".into());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    if std::fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        let target = std::fs::read_link(&absolute).map_err(|e| e.to_string())?;
        let target = if target.is_absolute() {
            target
        } else {
            absolute
                .parent()
                .ok_or("symlink has no parent")?
                .join(target)
        };
        return resolve_path(&target, depth + 1);
    }
    match std::fs::canonicalize(&absolute) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = absolute.parent().ok_or_else(|| error.to_string())?;
            let name = absolute.file_name().ok_or_else(|| error.to_string())?;
            Ok(resolve_path(parent, depth + 1)?.join(name))
        }
        Err(error) => Err(format!(
            "cannot validate database path {}: {error}",
            path.display()
        )),
    }
}

/// The identity of the file a path names, so two spellings of one database are
/// recognized as the same file -- including through a hard link, which no
/// amount of path resolution reveals.
#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// HOME is caller-controlled isolation, not the identity of the account whose
/// database we protect. Query the account without changing process environment.
#[cfg(unix)]
fn account_home() -> Result<PathBuf, String> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers refer to writable storage valid for this
        // call. pw_dir is copied before that backing buffer is dropped.
        let code = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 || result.is_null() {
            return Err("REFUSED: cannot determine account home for database protection".into());
        }
        // SAFETY: successful getpwuid_r initialized entry.
        let directory = unsafe { (*result).pw_dir };
        if directory.is_null() {
            return Err("REFUSED: account has no home directory".into());
        }
        // SAFETY: a non-null pw_dir is a NUL-terminated string in buffer.
        let directory = unsafe { CStr::from_ptr(directory) };
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(directory.to_bytes()));
        if !home.is_absolute() {
            return Err("REFUSED: account home is not absolute".into());
        }
        return Ok(home);
    }
}

#[cfg(not(unix))]
fn account_home() -> Result<PathBuf, String> {
    Err("REFUSED: database protection is not implemented for this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root no other test can be handed. The clock alone does not guarantee
    /// that: `create_dir_all` succeeds on a directory that already exists, so
    /// two tests starting within one tick would silently share a root and see
    /// each other's files.
    fn fixture() -> (PathBuf, PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "kanna-db-access-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let production = root.join("build.kanna/kanna-v2.db");
        (root, production)
    }

    #[test]
    fn fresh_install_requires_deliberate_access_and_isolation_always_wins() {
        let (root, production) = fixture();
        let protected = vec![production.clone()];
        assert!(check_against(&production, &protected, false, false)
            .unwrap_err()
            .contains(DESKTOP_ACCESS_ENV));
        assert!(check_against(&production, &protected, true, false).is_ok());
        for desktop in [false, true] {
            assert!(check_against(&production, &protected, desktop, true)
                .unwrap_err()
                .contains("isolated/test/worktree"));
        }
        assert!(
            !production.parent().unwrap().exists(),
            "checking must not create directories"
        );
        assert!(check_against(&root.join("test.db"), &protected, false, true).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_database_is_untouched_on_refusal() {
        let (root, production) = fixture();
        std::fs::create_dir_all(production.parent().unwrap()).unwrap();
        std::fs::write(&production, b"owner data").unwrap();
        assert!(
            check_against(&production, std::slice::from_ref(&production), false, false).is_err()
        );
        assert_eq!(std::fs::read(&production).unwrap(), b"owner data");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_hardlinks_and_parent_aliases_cannot_bypass_the_guard() {
        use std::os::unix::fs::symlink;
        let (root, production) = fixture();
        std::fs::create_dir_all(production.parent().unwrap()).unwrap();
        let directory_alias = root.join("alias");
        symlink(production.parent().unwrap(), &directory_alias).unwrap();
        let protected = vec![production.clone()];
        assert!(check_against(
            &directory_alias.join("kanna-v2.db"),
            &protected,
            false,
            false
        )
        .is_err());
        let missing_alias = root.join("missing.db");
        symlink(&production, &missing_alias).unwrap();
        assert!(check_against(&missing_alias, &protected, false, false).is_err());
        std::fs::write(&production, b"owner data").unwrap();
        for (name, hardlink) in [("symlink.db", false), ("hardlink.db", true)] {
            let alias = root.join(name);
            if hardlink {
                std::fs::hard_link(&production, &alias).unwrap();
            } else {
                symlink(&production, &alias).unwrap();
            }
            assert!(check_against(&alias, &protected, false, false).is_err());
            assert!(check_against(&alias, &protected, true, true).is_err());
        }
        assert!(check_against(
            &production
                .parent()
                .unwrap()
                .join("../build.kanna/kanna-v2.db"),
            &protected,
            false,
            false
        )
        .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The guarded set must stay bound to the identifier constants and to the
    /// product's own data-directory derivations. Restating a literal here, or
    /// in `production_database_paths_for_home`, is what this test exists to
    /// fail on: a renamed identifier would otherwise leave the real database
    /// unguarded while every path resolver quietly followed the new name.
    #[test]
    fn the_guarded_set_is_bound_to_the_desktop_identifier_constants() {
        let home = Path::new("/Users/binding-fixture");
        let protected = production_database_paths_for_home(home);

        // Where the product itself puts each desktop's database. The staging
        // app's data directory is the one the crate derives for its daemon dir
        // (`<app support>/<identifier>/Kanna`), so its parent is the directory
        // the staging database lives in.
        let staging_data_dir = crate::daemon_dir_for_bundle_identifier_for_home(
            crate::STAGING_DESKTOP_BUNDLE_IDENTIFIER,
            false,
            home,
        )
        .parent()
        .expect("staging daemon dir has a parent")
        .to_path_buf();
        for expected in [
            crate::canonical_desktop_db_path_for_home(home),
            staging_data_dir.join(crate::DEFAULT_DB_NAME),
            crate::legacy_desktop_db_path_for_home(home),
        ] {
            assert!(
                protected.contains(&expected),
                "{} must be guarded",
                expected.display()
            );
        }

        // Nothing in the set is a literal that outlived its constant, and each
        // identifier is guarded under every protected root, not just its own
        // platform's: a Linux resolver must not reach a macOS-shaped path.
        for identifier in PROTECTED_BUNDLE_IDENTIFIERS {
            for root in PROTECTED_ROOTS {
                let expected = home
                    .join(root)
                    .join(identifier)
                    .join(crate::DEFAULT_DB_NAME);
                assert!(
                    protected.contains(&expected),
                    "{} must be guarded",
                    expected.display()
                );
            }
        }
        assert_eq!(
            protected.len(),
            PROTECTED_BUNDLE_IDENTIFIERS.len() * PROTECTED_ROOTS.len(),
            "the guarded set is exactly the identifier constants under every root: {protected:?}"
        );
    }

    /// Nothing is guarded that is not a real desktop: every guarded identifier
    /// is one the crate recognizes as a shipped environment, or the pre-rename
    /// identifier. A stale string that no shipped app answers to would grow the
    /// set without protecting anything.
    #[test]
    fn every_guarded_identifier_names_a_real_desktop() {
        for identifier in PROTECTED_BUNDLE_IDENTIFIERS {
            assert!(
                crate::desktop_cloud_environment_for_bundle_identifier(identifier, false).is_some()
                    || identifier == crate::LEGACY_DESKTOP_BUNDLE_IDENTIFIER,
                "{identifier} is guarded but is not a shipped desktop identifier"
            );
        }
    }

    /// `check` resolves the protected paths once per process, so by the time a
    /// database is opened that resolution may predate the database existing.
    /// A file created -- or aliased -- afterwards must still be refused, which
    /// is what keeps the cache from being a hole in the guard.
    #[cfg(unix)]
    #[test]
    fn a_stale_resolved_spelling_still_catches_a_database_created_afterwards() {
        let (root, production) = fixture();
        std::fs::create_dir_all(production.parent().unwrap()).unwrap();
        // Resolved while nothing is there yet, exactly as a fresh install.
        let stale = vec![resolve_existing_ancestor(&production).unwrap()];
        std::fs::write(&production, b"owner data").unwrap();
        let hardlink = root.join("dev.db");
        std::fs::hard_link(&production, &hardlink).unwrap();
        let protected = vec![production.clone()];
        for reached_by in [&production, &hardlink] {
            assert!(
                check_resolved(reached_by, &protected, &stale, false, false).is_err(),
                "{} must still be refused",
                reached_by.display()
            );
        }
        assert_eq!(std::fs::read(&production).unwrap(), b"owner data");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_launcher_can_ask_which_desktop_database_a_path_names() {
        let (root, production) = fixture();
        let protected = vec![production.clone()];
        let resolved = protected
            .iter()
            .map(|path| resolve_existing_ancestor(path))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            protected_match(&production, &protected, &resolved).unwrap(),
            Some(production.clone())
        );
        assert_eq!(
            protected_match(&root.join("worker/kanna-worker.db"), &protected, &resolved).unwrap(),
            None
        );
        assert!(!production.parent().unwrap().exists());
        for real in production_database_paths().unwrap() {
            assert_eq!(protected_desktop_database(&real).unwrap(), Some(real));
        }
        assert_eq!(
            protected_desktop_database(&root.join("own.db")).unwrap(),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sqlite_uris_and_empty_paths_are_refused() {
        for path in ["", "file:kanna-v2.db?mode=rwc"] {
            assert!(check_against(Path::new(path), &[], true, false).is_err());
        }
    }
}
