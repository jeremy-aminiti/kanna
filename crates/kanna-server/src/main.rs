mod agent_inventory;
mod bonjour;
mod cloud_task_publisher;
mod cloud_transfer_proxy;
mod commands;
mod config;
mod daemon_client;
mod db;
mod git_refs;
mod http_api;
mod human_control;
mod internal_ports;
mod ksp;
mod logging;
mod login_shell;
mod mobile_api;
mod pairing;
mod register;
mod relay;
mod relay_client;
mod repo_browser;
mod repo_commands;
mod runtime;
mod session_replacements;
mod task_creator;
mod task_diff;
mod task_files;
mod task_input_attachments;
mod task_transfer_tunnel;
mod terminal_attachments;
mod terminal_watcher;
mod terminal_window;
#[cfg(test)]
mod test_paths;
mod transfer_artifact;
mod transfer_control;
mod transfer_engine;
mod transfer_sidecar;
mod transfer_targets;
mod visual_companion;
mod workspace_commands;
mod worktree_cleanup;

use config::Config;
use std::sync::Arc;

/// Serializes unit tests that stage fake sidecars beside the shared test
/// executable. Those paths are process-wide, so every creator must hold this
/// guard until it has finished using and cleaning up the fixture.
#[cfg(test)]
static TEST_SIDECAR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Held for as long as a test owns the staged sidecars.
///
/// The directory the fixtures are staged in is the test executable's own, and
/// nothing stops a second `cargo test` for this worktree from running that same
/// executable at the same time — a gate beside a manual run, say. The mutex
/// cannot see that process, so it used to watch one run delete the `codex` it
/// had just written while another was resolving it, and the resolver then
/// silently fell through to a real `codex` on `PATH`. The lock file makes the
/// guard's reach match the directory's: `flock` is released when the file
/// closes, including when a process dies holding it, so a panicking test leaves
/// nothing wedged behind it.
#[cfg(test)]
pub(crate) struct TestSidecarGuard {
    // Dropped in declaration order, so the narrower lock is released first and
    // no other process is admitted while a thread here still holds the mutex.
    _in_process: tokio::sync::MutexGuard<'static, ()>,
    _across_processes: std::fs::File,
}

#[cfg(test)]
fn lock_test_sidecar_directory() -> std::fs::File {
    use std::os::unix::io::AsRawFd;

    let path = std::env::current_exe()
        .expect("test executable path")
        .parent()
        .expect("test executable directory")
        .join(".kanna-test-sidecars.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("open sidecar lock {}: {error}", path.display()));
    // SAFETY: `file` owns the descriptor and outlives the call.
    while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let error = std::io::Error::last_os_error();
        assert!(
            error.kind() == std::io::ErrorKind::Interrupted,
            "lock sidecar directory {}: {error}",
            path.display()
        );
    }
    file
}

#[cfg(test)]
pub(crate) async fn test_sidecar_guard() -> TestSidecarGuard {
    // Taken blocking rather than through `block_in_place`, which panics on the
    // current-thread runtime `#[tokio::test]` builds by default. The in-process
    // mutex above is already held, so the only wait left is on another process,
    // and this test cannot proceed until that one is done regardless.
    let in_process = TEST_SIDECAR_LOCK.lock().await;
    TestSidecarGuard {
        _in_process: in_process,
        _across_processes: lock_test_sidecar_directory(),
    }
}

/// Synchronous fixtures share the same lock, outside any Tokio runtime.
#[cfg(test)]
pub(crate) fn test_sidecar_guard_blocking() -> TestSidecarGuard {
    let in_process = TEST_SIDECAR_LOCK.blocking_lock();
    TestSidecarGuard {
        _in_process: in_process,
        _across_processes: lock_test_sidecar_directory(),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match worktree_cleanup::run_cleanup_cli(&args[1..]) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Worktree cleanup failed: {error}");
            std::process::exit(1);
        }
    }
    if args.get(1).map(|s| s.as_str()) == Some("register") {
        let relay_url = args
            .get(2)
            .cloned()
            .or_else(|| std::env::var("KANNA_RELAY_URL").ok())
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty());
        let Some(relay_url) = relay_url else {
            eprintln!("Registration requires a relay URL argument or KANNA_RELAY_URL.");
            std::process::exit(1);
        };
        if let Err(e) = register::register(&relay_url).await {
            eprintln!("Registration failed: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    // Log to a rotated, timestamped file in the instance's daemon data dir
    // (like the daemon does). The desktop app spawns this sidecar with null
    // stdio, so stderr-only logging would discard everything — the file is
    // the durable record of stage transitions and revision decisions. See
    // `logging` for the size cap and the stable `kanna-server.log` symlink.
    let _logger_handle = logging::init(std::path::Path::new(&config.daemon_dir));
    kanna_daemon::terminal_perf::start_global_watchdog();

    let relay_url = config.relay_url.trim().to_string();
    log::info!(
        "kanna-server starting, relay: {}",
        if relay_url.is_empty() {
            "(disabled)"
        } else {
            &relay_url
        }
    );

    if let Some((legacy_path, canonical_path)) =
        config::legacy_database_relocation_paths(&config.db_path)
    {
        match db::relocate_legacy_database_if_needed(&legacy_path, &canonical_path) {
            Ok(true) => log::info!(
                "Relocated legacy database: {} -> {}",
                legacy_path.display(),
                canonical_path.display()
            ),
            Ok(false) => {}
            Err(error) => {
                eprintln!("Failed to relocate legacy database: {error}");
                std::process::exit(1);
            }
        }
    }

    let heartbeat_config = config.clone();
    tokio::spawn(async move {
        loop {
            log::info!("desktop heartbeat tick for {}", heartbeat_config.desktop_id);
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    let db = match db::Db::open_migrated(&config.db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to open database at {}: {}", config.db_path, e);
            std::process::exit(1);
        }
    };

    log::info!("Database opened: {}", config.db_path);

    log::info!(
        "starting mobile Bonjour advertisement for {} ({}, {}, port {})",
        config.desktop_name,
        config.desktop_id,
        config.environment,
        config.lan_port
    );
    let _mobile_bonjour = bonjour::MobileBonjourAdvertisement::start(
        &config.desktop_name,
        &config.desktop_id,
        &config.environment,
        config.lan_port,
    )
    .map_err(|error| {
        log::warn!("mobile Bonjour advertisement unavailable: {}", error);
        error
    })
    .ok();

    // Capture the login-shell PATH before the first stage action needs it —
    // loading zshrc costs seconds and must never sit on a request path.
    tokio::task::spawn_blocking(task_creator::warm_login_shell_path);
    let reconciliation_db_path = config.db_path.clone();
    tokio::task::spawn_blocking(move || match db::Db::open(&reconciliation_db_path) {
        Ok(db) => {
            if let Err(error) = worktree_cleanup::reconcile_leftover_worktrees(&db) {
                log::warn!("startup worktree cleanup reconciliation failed: {error}");
            }
        }
        Err(error) => {
            log::warn!("startup worktree cleanup could not open database: {error}");
        }
    });

    let http_state = Arc::new(http_api::AppState::new(config.clone()));
    let session_replacements = http_state.session_replacements();
    let detached_terminals = http_state
        .terminal_attachments()
        .take_detach_receiver()
        .expect("terminal detach reconciliation receiver should only be taken at startup");
    let terminal_detach_state = Arc::clone(&http_state);
    tokio::spawn(async move {
        terminal_watcher::terminal_detach_reconciliation_loop(
            terminal_detach_state,
            detached_terminals,
        )
        .await;
    });
    let terminal_state = Arc::clone(&http_state);
    tokio::spawn(async move {
        terminal_watcher::terminal_state_watcher_loop(terminal_state, session_replacements).await;
    });
    let activity_debounce_state = Arc::clone(&http_state);
    tokio::spawn(async move {
        terminal_watcher::activity_event_debounce_loop(activity_debounce_state).await;
    });
    runtime::run_server_services(config, db, http_state).await;
}
