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

// Each test binary that includes this module compiles its own copy, and none
// of them uses every helper.
#![allow(dead_code)]

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

/// How long a failure waits for the child's stderr to finish arriving. The
/// child is already gone by then, so this is scheduling latency and not a
/// round trip; it stays generous only so a loaded box cannot turn a reported
/// refusal back into `<none>`.
const STDERR_DRAIN_WAIT: Duration = Duration::from_secs(2);

pub struct SidecarProcess {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<ControlResponse>,
    stderr: Arc<Mutex<String>>,
    /// Closed by the reader thread once the child's stderr reaches EOF.
    /// Observing the exit status says nothing about that thread having been
    /// scheduled, so the buffer alone can still be empty when the refusal that
    /// explains the exit is already in the pipe.
    stderr_drained: Receiver<()>,
    stderr_is_drained: bool,
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
        let (drained_tx, stderr_drained) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            // Dropped on the way out, whether by EOF or by a read error, which
            // is what `wait_for_stderr` waits for.
            let _drained = drained_tx;
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
            stderr_drained,
            stderr_is_drained: false,
        }
    }

    /// Write one control request, or fail with the reason the sidecar is not
    /// reading it. A child that refused to start closes this pipe, and the
    /// resulting `BrokenPipe` says nothing on its own — the refusal it exited
    /// over is on stderr, so report that here rather than the errno.
    pub fn write_control(&mut self, request: &ControlRequest) {
        let line = serde_json::to_string(request).expect("control request should serialize");
        let written = writeln!(self.stdin, "{line}").and_then(|()| self.stdin.flush());
        if let Err(error) = written {
            panic!(
                "control request did not reach the sidecar ({error}); {}",
                self.diagnostics()
            );
        }
    }

    /// Whether the child is gone within `within`. Bounded, and deliberately
    /// leaves stderr alone: draining it is [`Self::diagnostics`]'s job, and a
    /// caller checking liveness must not paper over that step.
    pub fn wait_for_exit(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(LIVENESS_POLL);
        }
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
        let exited = self.child.try_wait();
        let state = match exited {
            Ok(Some(status)) => format!("sidecar exited with {status}"),
            Ok(None) => "sidecar is still running".to_string(),
            Err(ref error) => format!("sidecar state is unknown: {error}"),
        };
        if matches!(exited, Ok(Some(_))) {
            self.wait_for_stderr();
        }
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

    /// Let the reader thread finish before the buffer is read.
    ///
    /// Only ever called once the child is gone: its exit closes the write end,
    /// so EOF is already on its way and this returns in the time it takes the
    /// thread to be scheduled. The bound is there for the one case that cannot
    /// happen here but would otherwise hang forever — a surviving process
    /// holding the inherited descriptor open.
    fn wait_for_stderr(&mut self) {
        if self.stderr_is_drained {
            return;
        }
        // Either arm means the thread is finished; only a timeout does not.
        self.stderr_is_drained = !matches!(
            self.stderr_drained.recv_timeout(STDERR_DRAIN_WAIT),
            Err(RecvTimeoutError::Timeout)
        );
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
