use crate::db::Db;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};

// Up to 16 visible repos, each with its conventional build/task-temp path,
// plus the server's OS temp volume. Never walk a repository or parse build files.
pub(super) const MAX_STORAGE_PATHS: usize = 49;
const MAX_REPOS: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StorageStats {
    volume_id: String,
    paths: Vec<StoragePath>,
    total_bytes: u64,
    available_bytes: u64,
    free_bytes: u64,
    read_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoragePath {
    role: String,
    path: String,
    measured_path: String,
}

pub(super) fn collect(db: &Db, errors: &mut Vec<String>) -> Vec<StorageStats> {
    let mut paths = vec![("serverTemp", std::env::temp_dir())];
    match db.list_repos() {
        Ok(repos) => {
            if repos.len() > MAX_REPOS {
                errors.push(format!(
                    "storage inspection limited to {MAX_REPOS} visible repositories"
                ));
            }
            for repo in repos.into_iter().take(MAX_REPOS) {
                let root = PathBuf::from(repo.path);
                paths.push(("repo", root.clone()));
                paths.push(("conventionalBuild", root.join(".build")));
                paths.push(("conventionalTaskTemp", root.join(".tmp")));
            }
        }
        Err(error) => errors.push(format!("storage repository listing: {error}")),
    }
    let mut volumes = BTreeMap::new();
    for (role, path) in paths {
        match probe(&path) {
            Ok((device, measured, stats)) => {
                let volume = volumes.entry(device).or_insert(stats);
                volume.paths.push(StoragePath {
                    role: role.into(),
                    path: super::bounded_text(&path.to_string_lossy(), 1024),
                    measured_path: super::bounded_text(&measured.to_string_lossy(), 1024),
                });
            }
            Err(error) => errors.push(format!(
                "storage {role} {}: {error}",
                super::bounded_text(&path.to_string_lossy(), 256)
            )),
        }
    }
    volumes.into_values().collect()
}

pub(super) fn least_available_bytes(rows: &[StorageStats]) -> Option<u64> {
    rows.iter().map(|row| row.available_bytes).min()
}

fn existing_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    for ancestor in path.ancestors() {
        match std::fs::canonicalize(ancestor) {
            Ok(path) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no existing backing directory",
    ))
}

fn probe(path: &Path) -> Result<(u64, PathBuf, StorageStats), String> {
    let measured = existing_ancestor(path).map_err(|e| e.to_string())?;
    let device = std::fs::metadata(&measured)
        .map_err(|e| e.to_string())?
        .dev();
    let c_path = CString::new(measured.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut info = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    if unsafe { libc::statvfs(c_path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let info = unsafe { info.assume_init() };
    // libc's field widths differ on macOS and Linux.
    #[allow(clippy::unnecessary_cast)]
    let (block, total, free, available) = (
        info.f_frsize as u64,
        info.f_blocks as u64,
        info.f_bfree as u64,
        info.f_bavail as u64,
    );
    let bytes = |blocks: u64| {
        blocks
            .checked_mul(block)
            .ok_or_else(|| "storage byte count overflow".to_string())
    };
    Ok((
        device,
        measured,
        StorageStats {
            volume_id: format!("device:{device}"),
            paths: vec![],
            total_bytes: bytes(total)?,
            free_bytes: bytes(free)?,
            available_bytes: bytes(available)?,
            read_only: info.f_flag & libc::ST_RDONLY != 0,
        },
    ))
}

pub(super) fn bound_remote(rows: &mut Vec<StorageStats>) {
    rows.truncate(MAX_STORAGE_PATHS);
    let mut remaining = MAX_STORAGE_PATHS;
    for row in rows {
        row.volume_id = super::bounded_text(&row.volume_id, 128);
        row.paths.truncate(remaining);
        remaining -= row.paths.len();
        for path in &mut row.paths {
            path.role = super::bounded_text(&path.role, 64);
            path.path = super::bounded_text(&path.path, 1024);
            path.measured_path = super::bounded_text(&path.measured_path, 1024);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_missing_paths_share_backing_volume() {
        let root = tempfile::tempdir().unwrap();
        let first = probe(root.path()).unwrap();
        let build = probe(&root.path().join(".build/not-created")).unwrap();
        let temp = probe(&root.path().join(".tmp")).unwrap();
        assert_eq!(first.0, build.0);
        assert_eq!(first.0, temp.0);
        assert_eq!(first.1, build.1);
        assert!(first.2.total_bytes > 0);
        assert!(first.2.available_bytes <= first.2.free_bytes);
    }
}
