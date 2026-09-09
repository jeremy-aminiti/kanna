//! `kanna-server`'s file logger.
//!
//! Two properties this file exists to guarantee, both learned the hard way on
//! a staging machine that filled its disk:
//!
//! - **A runaway logger cannot eat the disk.** One misrouted client retried a
//!   request that could only ever fail, at ~100/s for eleven hours, and the
//!   log grew to 22.8 GB and 123 million lines with no rotation and no cap.
//!   Rotation plus cleanup bounds one process at
//!   [`max_retained_bytes`], whatever it is logging.
//! - **Every line carries a timestamp.** The default flexi_logger format does
//!   not include one, so the flood could not be correlated with anything —
//!   not the relay, not the daemon, not the incident. `detailed_format` is
//!   what the daemon has always used; the server now matches it.
//!
//! The stable `kanna-server.log` name is a symlink to the running process's
//! file, exactly as `kanna-daemon.log` is, so the path in every runbook keeps
//! working while the file behind it rotates.

use std::path::{Path, PathBuf};

/// Rotate at this size. Normal traffic is single-digit MB per day, so this is
/// only reached by something pathological — which is precisely when the cap
/// has to hold.
pub(crate) const MAX_LOG_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Rotated files kept beside the current one. Enough history to cover several
/// days of ordinary logging, and a hard bound under a flood.
pub(crate) const KEPT_ROTATED_LOG_FILES: usize = 5;

/// The most disk one process's log can occupy: the file being written plus the
/// rotated files kept behind it.
pub(crate) fn max_retained_bytes() -> u64 {
    MAX_LOG_FILE_BYTES * (KEPT_ROTATED_LOG_FILES as u64 + 1)
}

pub(crate) fn rotation() -> (
    flexi_logger::Criterion,
    flexi_logger::Naming,
    flexi_logger::Cleanup,
) {
    (
        flexi_logger::Criterion::Size(MAX_LOG_FILE_BYTES),
        flexi_logger::Naming::Numbers,
        flexi_logger::Cleanup::KeepLogFiles(KEPT_ROTATED_LOG_FILES),
    )
}

pub(crate) fn file_spec(daemon_dir: &Path) -> flexi_logger::FileSpec {
    flexi_logger::FileSpec::default()
        .directory(daemon_dir)
        .discriminant(std::process::id().to_string())
}

/// Point `kanna-server.log` at this process's current log file.
///
/// The desktop used to append the sidecar's stderr to that name, which is how
/// an unrotated 22.8 GB file came to exist beside a rotated one holding the
/// same records. The stable name is now a pointer, not a second copy.
pub(crate) fn link_current_log(daemon_dir: &Path) -> std::io::Result<PathBuf> {
    let pid_marker = format!("_{}_", std::process::id());
    let mut matches = std::fs::read_dir(daemon_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("kanna-server_")
                        && name.contains(&pid_marker)
                        && name.ends_with(".log")
                })
        })
        .collect::<Vec<_>>();
    matches.sort();
    let target = matches.pop().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "per-process server log for pid {} was not found",
                std::process::id()
            ),
        )
    })?;
    let link = daemon_dir.join("kanna-server.log");
    match std::fs::symlink_metadata(&link) {
        // A build before this one let the desktop append the sidecar's stderr
        // here as a regular file, unrotated and never reset across restarts —
        // one reached 22.8 GB. Move it aside rather than delete it: it is
        // somebody's incident history until they have read it. This runs once;
        // afterwards the path is a symlink.
        Ok(metadata) if metadata.file_type().is_file() => {
            let archived = daemon_dir.join("kanna-server.log.previous");
            std::fs::rename(&link, &archived)?;
            eprintln!(
                "kanna-server: moved the pre-rotation {} aside to {}; delete it when you no longer need it",
                link.display(),
                archived.display()
            );
        }
        Ok(_) => std::fs::remove_file(&link)?,
        Err(_) => {}
    }
    std::os::unix::fs::symlink(&target, &link)?;
    Ok(target)
}

/// Start the file logger.
///
/// Records keep going to stderr as well. That stream is a consumed interface,
/// not just a convenience: the desktop's server-lifecycle tests attach a
/// socket to it and wait for specific records, so silencing it hangs them.
/// Bounding the *file* the desktop writes that stream into is the desktop's
/// job — see `server_stderr_log`.
pub(crate) fn init(daemon_dir: &Path) -> Option<flexi_logger::LoggerHandle> {
    let _ = std::fs::create_dir_all(daemon_dir);
    // RUST_LOG overrides the default filter. `kanna_daemon=warn` must stay in
    // the default: the KSP terminal path emits its `terminal_perf`
    // stall/backpressure records under the shared `kanna_daemon` target, and
    // a `kanna_server`-only filter silently discards the diagnostics that
    // pinpoint terminal freezes.
    let logger =
        match flexi_logger::Logger::try_with_env_or_str("kanna_server=info,kanna_daemon=warn") {
            Ok(logger) => logger,
            Err(error) => {
                eprintln!("kanna-server: failed to configure logging: {error}");
                return None;
            }
        };
    let (criterion, naming, cleanup) = rotation();
    match logger
        .log_to_file(file_spec(daemon_dir))
        .format(flexi_logger::detailed_format)
        .rotate(criterion, naming, cleanup)
        .duplicate_to_stderr(flexi_logger::Duplicate::Info)
        .start()
    {
        Ok(handle) => {
            // The file is created lazily, on the first record — so emit one
            // before looking for it, or the link always misses.
            log::info!(
                "logging to {} (rotating at {} MiB, keeping {} rotated files, at most {} MiB on disk)",
                daemon_dir.display(),
                MAX_LOG_FILE_BYTES / (1024 * 1024),
                KEPT_ROTATED_LOG_FILES,
                max_retained_bytes() / (1024 * 1024),
            );
            if let Err(error) = link_current_log(daemon_dir) {
                log::warn!("failed to link kanna-server.log to this run's log file: {error}");
            }
            Some(handle)
        }
        Err(error) => {
            // No file: stderr is the only record left.
            eprintln!("kanna-server: file logging unavailable ({error}); logging to stderr");
            flexi_logger::Logger::try_with_env_or_str("kanna_server=info,kanna_daemon=warn")
                .ok()
                .and_then(|logger| {
                    logger
                        .format(flexi_logger::detailed_format)
                        .log_to_stderr()
                        .start()
                        .ok()
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flexi_logger::writers::{FileLogWriter, LogWriter};

    fn dir_bytes(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.metadata().ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .sum()
    }

    /// The property that matters is not "rotation is configured" but "a
    /// process that logs without bound occupies bounded disk". Drive a writer
    /// with the production spec past several rotations and measure the
    /// directory.
    #[test]
    fn a_runaway_logger_stays_within_the_retained_size_cap() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The production constant is 32 MB; writing 6+ of those in a unit test
        // is pure I/O, so the same policy shape is exercised at 64 KiB. Both
        // bounds are `size * (keep + 1)`, which is the invariant under test.
        let cap = 64 * 1024;
        let writer = FileLogWriter::builder(
            flexi_logger::FileSpec::default()
                .directory(dir.path())
                .basename("kanna-server"),
        )
        .format(flexi_logger::detailed_format)
        .rotate(
            flexi_logger::Criterion::Size(cap),
            flexi_logger::Naming::Numbers,
            flexi_logger::Cleanup::KeepLogFiles(KEPT_ROTATED_LOG_FILES),
        )
        // Production cleans up on a background thread so logging never blocks
        // on unlinking files; that only delays convergence. Make it
        // synchronous here so the steady state is observable at a fixed point
        // instead of raced against.
        .cleanup_in_background_thread(false)
        .try_build()
        .expect("build writer");

        let line = "x".repeat(512);
        for _ in 0..4_000 {
            writer
                .write(
                    &mut flexi_logger::DeferredNow::new(),
                    &log::Record::builder()
                        .args(format_args!("{line}"))
                        .level(log::Level::Error)
                        .target("kanna_server::test")
                        .build(),
                )
                .expect("write record");
        }
        writer.flush().expect("flush");

        let written = 4_000 * 512;
        let retained = dir_bytes(dir.path());
        assert!(
            retained > cap,
            "the test must actually rotate; retained {retained} bytes"
        );
        assert!(
            retained < written / 2,
            "an unbounded log would have kept everything; retained {retained} of {written} bytes"
        );
        // `size * (keep + 1)`, plus one record's slack: rotation triggers
        // after the record that crosses the threshold is written.
        let bound = cap * (KEPT_ROTATED_LOG_FILES as u64 + 1) + 4_096;
        assert!(
            retained <= bound,
            "retained {retained} bytes exceeds the {bound} byte cap"
        );
    }

    #[test]
    fn every_logged_line_carries_a_timestamp() {
        let dir = tempfile::tempdir().expect("temp dir");
        let writer = FileLogWriter::builder(
            flexi_logger::FileSpec::default()
                .directory(dir.path())
                .basename("kanna-server"),
        )
        .format(flexi_logger::detailed_format)
        .try_build()
        .expect("build writer");
        writer
            .write(
                &mut flexi_logger::DeferredNow::new(),
                &log::Record::builder()
                    .args(format_args!("GET /v1/task-events -> 400 Bad Request"))
                    .level(log::Level::Error)
                    .target("kanna_server::http_api::routes")
                    .build(),
            )
            .expect("write record");
        writer.flush().expect("flush");

        let contents = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<String>();
        assert!(
            contents.contains("GET /v1/task-events"),
            "record missing: {contents}"
        );
        // `detailed_format` leads with `[YYYY-MM-DD HH:MM:SS...]`. A line that
        // cannot be placed in time is what made a 123-million-line log
        // impossible to correlate with anything.
        let leading_date = contents
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches('[')
            .to_string();
        assert_eq!(
            leading_date.len(),
            "2026-09-08".len(),
            "expected a leading date, got {contents}"
        );
        assert!(
            leading_date
                .chars()
                .all(|character| character.is_ascii_digit() || character == '-'),
            "expected a leading date, got {contents}"
        );
    }

    /// Every runbook, doc and memory note says `kanna-server.log`. Rotation
    /// renames the file behind it, so the stable name has to resolve to
    /// whichever file this process is currently writing.
    #[test]
    fn the_stable_log_name_points_at_this_process_current_rotating_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let pid = std::process::id();
        for name in [
            // Another process's files, and this process's already-rotated
            // ones: neither is what the stable name must resolve to.
            "kanna-server_99999_rCURRENT.log".to_string(),
            format!("kanna-server_{pid}_r00000.log"),
            format!("kanna-server_{pid}_r00001.log"),
            format!("kanna-server_{pid}_rCURRENT.log"),
        ] {
            std::fs::write(dir.path().join(&name), b"").expect("stage log file");
        }

        // A pre-rotation regular file at the stable name is somebody's
        // incident history; it must be moved aside, not deleted.
        std::fs::write(dir.path().join("kanna-server.log"), b"pre-rotation history")
            .expect("stage legacy log");

        let target = link_current_log(dir.path()).expect("link current log");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("kanna-server.log.previous"))
                .expect("archived legacy log"),
            "pre-rotation history"
        );
        assert_eq!(
            target.file_name().and_then(|name| name.to_str()),
            Some(format!("kanna-server_{pid}_rCURRENT.log").as_str())
        );
        let link = dir.path().join("kanna-server.log");
        assert_eq!(std::fs::read_link(&link).expect("read link"), target);

        // A stale link from a previous run must be replaced, not appended to.
        let target_again = link_current_log(dir.path()).expect("relink current log");
        assert_eq!(target_again, target);
    }

    #[test]
    fn the_retained_size_cap_is_the_documented_bound() {
        assert_eq!(
            max_retained_bytes(),
            MAX_LOG_FILE_BYTES * (KEPT_ROTATED_LOG_FILES as u64 + 1)
        );
        // A bound nobody would notice is not a bound. 192 MB per process is
        // small enough that a full disk is never this file's doing.
        assert!(max_retained_bytes() <= 256 * 1024 * 1024);
    }
}
