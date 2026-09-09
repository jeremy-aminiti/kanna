//! Spawning the out-of-process sidecar for an integration test.
//!
//! Two things every sidecar test needs, and neither is optional:
//!
//! 1. **A database of its own.** `RuntimeConfig::from_env` falls back to the
//!    desktop's production database when nothing names one, and
//!    `kanna_runtime_defaults::database_access` refuses that path for a
//!    worktree or test process — so a spawn that names no database exits
//!    before it reads its first control request. The path belongs under the
//!    test's own temp root, which every caller already owns. Authorizing the
//!    desktop path instead (`KANNA_DESKTOP_DB_ACCESS=desktop`) would point a
//!    test at the operator's live data and is never the fix.
//!
//! 2. **A dead child must fail the test, not stall it.** The sidecar reports
//!    its startup failures on stderr and then exits, which is silent to any
//!    wait on its side effects: one of these tests waited forever for a peer
//!    connection that an already-exited sidecar was never going to open, so a
//!    misconfiguration surfaced as a suite that never returned. Every wait
//!    here is bounded and gives up the moment the child is gone, reporting its
//!    exit status and the stderr it left behind.

use kanna_task_transfer::protocol::{ControlRequest, ControlResponse};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a control response may take to come back from the out-of-process
/// sidecar. Every use is a liveness wait — the failure it guards is a response
/// that never arrives — so it is deliberately far above the milliseconds a
/// healthy round trip takes, leaving room for a box running several suites.
pub const CONTROL_RESPONSE_WAIT: Duration = Duration::from_secs(10);

/// How often a bounded wait rechecks whether the child is still alive. Small
/// enough that a sidecar which refused to start fails its test in a moment
/// rather than at the full deadline.
const LIVENESS_POLL: Duration = Duration::from_millis(50);

pub struct SidecarProcess {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<ControlResponse>,
    stderr: Arc<Mutex<String>>,
}

impl Drop for SidecarProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl SidecarProcess {
    /// Spawn the sidecar against `temp`, the directory the calling test owns.
    ///
    /// The transfer root, registry, identity, discovery mode, listen port and
    /// **database** are all resolved inside that directory; `configure` adds
    /// whatever the individual test needs on top.
    pub fn spawn(temp: &Path, configure: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanna-task-transfer"));
        command
            .env("KANNA_TRANSFER_ROOT", temp)
            .env("KANNA_TRANSFER_REGISTRY_DIR", registry_dir(temp))
            .env("KANNA_TRANSFER_PEER_ID", "peer-primary")
            .env("KANNA_TRANSFER_DISPLAY_NAME", "Primary")
            .env("KANNA_TRANSFER_DISCOVERY", "registry")
            .env("KANNA_TRANSFER_PORT", "0")
            .env("KANNA_DB_PATH", isolated_db_path(temp))
            .env_remove("KANNA_CLI_DB_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = command.spawn().expect("sidecar binary should spawn");

        let stdin = child.stdin.take().expect("piped sidecar stdin");
        let stdout = child.stdout.take().expect("piped sidecar stdout");
        let mut child_stderr = child.stderr.take().expect("piped sidecar stderr");
        let (response_tx, responses) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(response) = serde_json::from_str::<ControlResponse>(&line) {
                    if response_tx.send(response).is_err() {
                        break;
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = child_stderr.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                stderr_sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push_str(&String::from_utf8_lossy(&buffer[..read]));
            }
        });

        Self {
            child,
            stdin,
            responses,
            stderr,
        }
    }

    pub fn write_control(&mut self, request: &ControlRequest) {
        writeln!(
            self.stdin,
            "{}",
            serde_json::to_string(request).expect("control request should serialize")
        )
        .expect("control request should reach the sidecar");
        self.stdin.flush().expect("control request should flush");
    }

    /// The next control response, or a failure naming what was expected and
    /// what the sidecar did instead.
    pub fn next_response(&mut self, what: &str) -> ControlResponse {
        let deadline = Instant::now() + CONTROL_RESPONSE_WAIT;
        loop {
            match self.responses.recv_timeout(LIVENESS_POLL) {
                Ok(response) => return response,
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("{what}: sidecar stdout closed; {}", self.diagnostics())
                }
                Err(RecvTimeoutError::Timeout) => self.fail_if_settled(what, deadline),
            }
        }
    }

    /// Await `future` while the sidecar is expected to make it resolve.
    pub async fn expect_alive<T>(&mut self, what: &str, future: impl Future<Output = T>) -> T {
        let deadline = Instant::now() + CONTROL_RESPONSE_WAIT;
        tokio::pin!(future);
        loop {
            match tokio::time::timeout(LIVENESS_POLL, &mut future).await {
                Ok(value) => return value,
                Err(_) => self.fail_if_settled(what, deadline),
            }
        }
    }

    /// Panic if the child is gone, or if the wait has run out of time.
    fn fail_if_settled(&mut self, what: &str, deadline: Instant) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            panic!("{what}: {}", self.diagnostics());
        }
        if Instant::now() >= deadline {
            panic!(
                "{what}: no answer within {CONTROL_RESPONSE_WAIT:?}; {}",
                self.diagnostics()
            );
        }
    }

    fn diagnostics(&mut self) -> String {
        let state = match self.child.try_wait() {
            Ok(Some(status)) => format!("sidecar exited with {status}"),
            Ok(None) => "sidecar is still running".to_string(),
            Err(error) => format!("sidecar state is unknown: {error}"),
        };
        let stderr = self
            .stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let stderr = if stderr.trim().is_empty() {
            "<none>".to_string()
        } else {
            stderr
        };
        format!("{state}; sidecar stderr: {stderr}")
    }
}

/// The registry [`SidecarProcess::spawn`] points the sidecar at, so a test can
/// read and write the same one.
pub fn registry_dir(temp: &Path) -> std::path::PathBuf {
    temp.join("registry")
}

/// The sidecar's database for this test: under the test's own temp root, so
/// the desktop's protected production database is never named or opened.
fn isolated_db_path(temp: &Path) -> std::path::PathBuf {
    temp.join("kanna-sidecar-test.sqlite")
}
