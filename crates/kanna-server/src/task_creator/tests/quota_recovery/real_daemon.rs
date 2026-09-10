//! Producer-to-consumer regression: no fabricated ProviderNotice enters this
//! test. An isolated real daemon owns the PTY, seed, classifier and latch; the
//! real server watcher owns the resulting rejection/park record. The provider
//! is a FIFO-gated shell, never an installed provider or account.

use super::*;
use kanna_daemon::protocol::{Command as DaemonCommand, Event};
use serde_json::{json, Value};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::{timeout, Instant};

const EVENTUAL: Duration = Duration::from_secs(20);
const QUIET: Duration = Duration::from_secs(6);

struct RealDaemon {
    child: Child,
    dir: PathBuf,
    binary: PathBuf,
}

struct Successor(Child);

impl Drop for Successor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl RealDaemon {
    async fn start() -> Self {
        // Cross-crate tests do not receive CARGO_BIN_EXE_kanna-daemon. Match
        // the existing server real-daemon fixture's split build-dir layout.
        // Missing binaries fail; this test never builds or silently skips.
        let binary = if let Some(path) = std::env::var_os("KANNA_DAEMON_TEST_BIN") {
            PathBuf::from(path)
        } else {
            let exe = std::env::current_exe().unwrap();
            let profile_dir = exe.parent().and_then(Path::parent).unwrap();
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .unwrap();
            [
                root.join(".build")
                    .join(profile_dir.file_name().unwrap())
                    .join("kanna-daemon"),
                profile_dir.join("kanna-daemon"),
            ]
            .into_iter()
            .find(|path| path.is_file())
            .expect("build kanna-daemon first or set KANNA_DAEMON_TEST_BIN")
        };
        assert!(binary.is_file(), "missing daemon binary: {binary:?}");
        let dir = crate::test_paths::unique_test_dir("quota-provenance-daemon");
        std::fs::create_dir_all(&dir).unwrap();
        // Own the child before any fallible readiness assertion.
        let mut daemon = Self {
            child: Command::new(&binary)
                .env("KANNA_DAEMON_DIR", &dir)
                .spawn()
                .expect("start real daemon"),
            dir,
            binary,
        };
        timeout(EVENTUAL, async {
            loop {
                assert!(daemon.child.try_wait().unwrap().is_none(), "daemon exited");
                let pid_matches = std::fs::read_to_string(daemon.dir.join("daemon.pid"))
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    == Some(daemon.child.id());
                if pid_matches && UnixStream::connect(daemon.socket()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon readiness");
        daemon
    }

    fn socket(&self) -> PathBuf {
        kanna_runtime_defaults::socket_path(&self.dir)
    }

    async fn connect(&self) -> DaemonClient {
        DaemonClient::connect(self.dir.to_str().unwrap())
            .await
            .unwrap()
    }

    async fn handoff(&mut self) {
        let mut successor = Successor(
            Command::new(&self.binary)
                .env("KANNA_DAEMON_DIR", &self.dir)
                .spawn()
                .unwrap(),
        );
        timeout(EVENTUAL, async {
            loop {
                assert!(
                    successor.0.try_wait().unwrap().is_none(),
                    "successor exited"
                );
                let published = std::fs::read_to_string(self.dir.join("daemon.pid"))
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    == Some(successor.0.id());
                if published
                    && UnixStream::connect(self.socket()).await.is_ok()
                    && self.child.try_wait().unwrap().is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("actual same-PTY handoff");
        std::mem::swap(&mut self.child, &mut successor.0);
    }
}

impl Drop for RealDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Relay(tokio::task::JoinHandle<()>);

impl Drop for Relay {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Forward actual commands and events byte-for-byte. The only observation is
/// the real Subscribe acknowledgement, so the seed cannot race ahead of the
/// server's subscription. No production readiness hook or protocol change.
async fn relay(dir: &Path, upstream: PathBuf) -> (Relay, tokio::sync::mpsc::UnboundedReceiver<()>) {
    std::fs::create_dir_all(dir).unwrap();
    let listener = UnixListener::bind(kanna_runtime_defaults::socket_path(dir)).unwrap();
    let (ready, ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        // Dropping the relay also cancels every accepted connection.
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted.unwrap();
                    let upstream = upstream.clone();
                    let ready = ready.clone();
                    connections.spawn(async move {
                        let mut client = BufReader::new(stream);
                        let mut daemon = BufReader::new(UnixStream::connect(upstream).await.unwrap());
                        let mut line = String::new();
                        assert_ne!(client.read_line(&mut line).await.unwrap(), 0);
                        let subscribing = matches!(
                            serde_json::from_str::<DaemonCommand>(&line).unwrap(),
                            DaemonCommand::Subscribe
                        );
                        daemon.write_all(line.as_bytes()).await.unwrap();
                        line.clear();
                        assert_ne!(daemon.read_line(&mut line).await.unwrap(), 0);
                        if subscribing {
                            assert!(matches!(serde_json::from_str::<Event>(&line).unwrap(), Event::Ok));
                        }
                        client.write_all(line.as_bytes()).await.unwrap();
                        if subscribing {
                            ready.send(()).unwrap();
                        }
                        if let Err(error) = tokio::io::copy_bidirectional(&mut client, &mut daemon).await {
                            // The real handoff closes both endpoints after
                            // ShuttingDown. macOS can report ENOTCONN while
                            // copy_bidirectional shuts down the closed half.
                            assert!(matches!(error.kind(),
                                std::io::ErrorKind::NotConnected |
                                std::io::ErrorKind::BrokenPipe |
                                std::io::ErrorKind::ConnectionReset), "relay I/O: {error}");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    completed.unwrap().unwrap();
                }
            }
        }
    });
    (Relay(task), ready_rx)
}

async fn command(client: &mut DaemonClient, value: Value) -> Event {
    client
        .send_command(&serde_json::from_value(value).unwrap())
        .await
        .unwrap()
}

fn assert_unrejected(db: &Db, phase: &str) {
    assert!(
        db.provider_rejections_for_task(TASK_ID).unwrap().is_empty(),
        "{phase}: history or quoted output reached the real quota consumer"
    );
    assert!(events_of(db, "task.provider_quota_rejected").is_empty());
    assert!(events_of(db, "task.provider_quota_parked").is_empty());
    let runs = db.list_stage_runs_for_task(TASK_ID).unwrap();
    assert_eq!(runs.len(), 1, "{phase}: no replacement run");
    assert_eq!(runs[0].status, "running");
}

// Complete each JSON read before handing off the event. Cancelling a timeout
// around DaemonClient::read_event itself could discard a partially read line.
fn events_from(
    mut client: DaemonClient,
    readers: &mut tokio::task::JoinSet<()>,
) -> tokio::sync::mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    readers.spawn(async move {
        loop {
            let event = client.read_event().await.expect("real daemon event read");
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    rx
}

async fn collect_for(
    observer: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    duration: Duration,
) -> Vec<Event> {
    let deadline = Instant::now() + duration;
    let mut events = Vec::new();
    while let Ok(event) = tokio::time::timeout_at(deadline, observer.recv()).await {
        let event = event.expect("observer closed");
        assert!(
            !matches!(event, Event::Exit { .. } | Event::Error { .. }),
            "{event:?}"
        );
        events.push(event);
    }
    events
}

fn release(gate: &mut File, phase: &str) {
    writeln!(gate, "{phase}").unwrap();
    gate.flush().unwrap();
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[tokio::test]
async fn real_seed_and_quoted_output_do_not_park_but_current_refusal_parks_once() {
    let mut daemon = RealDaemon::start().await;
    let mut config = test_config("quota-real-provenance");
    config.daemon_dir = daemon.dir.join("relay").to_string_lossy().into_owned();
    let (repo_root, db) = init_quota_fixture_without_candidates("quota-real-provenance", &config);
    insert_running_review_run(&db, &repo_root, "run-review", "claude", Some("opus"), None);
    let state = crate::http_api::AppState::new(config.clone());
    let replacements = state.session_replacements();
    let (mut relay, mut ready) = relay(Path::new(&config.daemon_dir), daemon.socket()).await;
    let (handoff_done, resume_watcher) = tokio::sync::oneshot::channel();

    let scenario = async {
        ready
            .recv()
            .await
            .expect("watcher subscribed to the real daemon");
        let captures: Value = serde_json::from_str(include_str!(
            "../../../../../../tests/cli-contract/fixtures/provider-quota-rejection.json"
        ))
        .unwrap();
        let capture = &captures[0];
        assert_eq!(capture["provider"], "claude");
        let frame = capture["frame"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\r\n");
        let refusal = capture["frame"][1].as_str().unwrap().trim();
        let quoted = format!("{{\"text\":\"{refusal}\"}}");
        let executable = daemon.dir.join("measured-claude-version");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] || exit 2\nprintf '%s\\n' {}\n",
                shell_quote(&format!(
                    "{} (Claude Code)",
                    capture["cliVersion"].as_str().unwrap()
                ))
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fifo = daemon.dir.join("provider-phases");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a live NUL-terminated fixture path.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut gate = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let script = format!(
            "exec 3<\"$1\"; while IFS= read -r phase <&3; do case \"$phase\" in \
             startup) printf '\\r\\nfresh-startup-marker\\r\\n' ;; \
             quoted) printf '%s\\r\\n' {} ;; \
             busy) printf '\\033[2J\\033[H✻ Thinking… (12s · ↓ 50 tokens)\\r\\n' ;; \
             refusal) printf '\\r\\n%s\\r\\n' {} ;; *) exit 2 ;; esac; done",
            shell_quote(&quoted),
            shell_quote(&frame),
        );
        let mut control = daemon.connect().await;
        assert!(matches!(
            command(
                &mut control,
                json!({
                    "type": "SeedSnapshot", "session_id": TASK_ID,
                    "snapshot": {"version": 1, "cols": 167, "rows": 65,
                        "cursor_row": 16, "cursor_col": 0, "cursor_visible": true,
                        "vt": format!("\x1b[2J\x1b[H{frame}")}
                })
            )
            .await,
            Event::Ok
        ));
        assert!(matches!(
            command(
                &mut control,
                json!({
                    "type": "Spawn", "session_id": TASK_ID, "executable": "/bin/sh",
                    "args": ["-c", script, "quota-fixture", fifo], "cwd": repo_root,
                    "env": {}, "cols": 80, "rows": 24, "agent_provider": "claude",
                    "agent_executable": executable
                })
            )
            .await,
            Event::SessionCreated { .. }
        ));
        let mut observer = daemon.connect().await;
        assert!(matches!(
            observer
                .send_command(&DaemonCommand::Subscribe)
                .await
                .unwrap(),
            Event::Ok
        ));
        // Snapshot registration is on a separate, unsubscribed connection;
        // interleaved notices cannot be mistaken for its acknowledgement.
        let mut output = daemon.connect().await;
        let snapshot = output
            .send_command(&DaemonCommand::ObserveSnapshot {
                session_id: TASK_ID.to_string(),
            })
            .await
            .unwrap();
        assert!(
            matches!(snapshot, Event::Snapshot { snapshot, .. } if snapshot.vt.contains("Fable limit"))
        );
        let mut readers = tokio::task::JoinSet::new();
        let mut observer = events_from(observer, &mut readers);
        let mut output = events_from(output, &mut readers);

        // The gated shell cannot produce any bytes in this first interval.
        let events = collect_for(&mut observer, QUIET).await;
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::ProviderNotice { .. })));
        loop {
            match output.try_recv() {
                Ok(event) => assert!(
                    !matches!(
                        event,
                        Event::Output { .. } | Event::Exit { .. } | Event::Error { .. }
                    ),
                    "gated provider must have produced no PTY output: {event:?}"
                ),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(error) => panic!("output observer disconnected: {error}"),
            }
        }
        assert_unrejected(&db, "no new PTY output");
        for (phase, marker) in [("startup", "fresh-startup-marker"), ("quoted", "{\"text\"")] {
            release(&mut gate, phase);
            timeout(EVENTUAL, async {
                let mut bytes = Vec::new();
                loop {
                    match output.recv().await.expect("output observer closed") {
                        Event::Output { data, .. } => bytes.extend(data),
                        event @ (Event::Exit { .. } | Event::Error { .. }) => panic!("{event:?}"),
                        // Observers also receive daemon-derived snapshots and
                        // metadata. Only Output proves a new PTY read.
                        _ => {}
                    }
                    if String::from_utf8_lossy(&bytes).contains(marker) {
                        break;
                    }
                }
            })
            .await
            .expect("actual fresh PTY output");
            let events = collect_for(&mut observer, QUIET).await;
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, Event::ProviderNotice { .. })),
                "{phase}: {events:?}"
            );
            assert_unrejected(&db, phase);
        }

        release(&mut gate, "refusal");
        let mut notices = Vec::new();
        timeout(EVENTUAL, async {
            loop {
                let event = observer.recv().await.expect("observer closed");
                assert!(!matches!(event, Event::Exit { .. } | Event::Error { .. }));
                if matches!(event, Event::ProviderNotice { .. }) {
                    notices.push(event);
                    break;
                }
            }
            while events_of(&db, "task.provider_quota_parked").is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("genuine daemon notice reaches the real park consumer");
        // Repainting a genuine refusal in the same attempt must not create
        // another producer announcement or another durable park.
        release(&mut gate, "refusal");
        notices.extend(
            collect_for(&mut observer, QUIET)
                .await
                .into_iter()
                .filter(|event| matches!(event, Event::ProviderNotice { .. })),
        );
        assert_eq!(notices.len(), 1, "per-attempt daemon latch");
        let rows = db.provider_rejections_for_task(TASK_ID).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stage_run_id, "run-review");
        assert_eq!(rows[0].model.as_deref(), Some("opus"));
        assert_eq!(rows[0].scope.as_deref(), Some("Fable"));
        assert_eq!(rows[0].source, "pty");
        assert_eq!(rows[0].rule_id, capture["ruleId"].as_str().unwrap());
        assert_eq!(
            rows[0].cli_version.as_deref(),
            capture["cliVersion"].as_str()
        );
        assert_eq!(events_of(&db, "task.provider_quota_rejected").len(), 1);
        let parked = events_of(&db, "task.provider_quota_parked");
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0]["reason"], "parked-no-candidate-list");
        assert_eq!(db.list_stage_runs_for_task(TASK_ID).unwrap().len(), 1);
        readers.abort_all();
        while readers.join_next().await.is_some() {}

        // Transfer the actual PTY, then reconnect the actual watcher. A
        // startup reannouncement may precede subscription, so additionally
        // demand a new, observed producer announcement after a real busy
        // frame. It still belongs to the same durable run/provider/scope.
        daemon.handoff().await;
        handoff_done.send(()).unwrap();
        ready.recv().await.expect("watcher subscribed to successor");
        let mut observer = daemon.connect().await;
        assert!(matches!(
            observer
                .send_command(&DaemonCommand::Subscribe)
                .await
                .unwrap(),
            Event::Ok
        ));
        let mut observer = events_from(observer, &mut readers);
        release(&mut gate, "busy");
        timeout(EVENTUAL, async {
            loop {
                let event = observer.recv().await.expect("successor observer closed");
                assert!(!matches!(event, Event::Exit { .. } | Event::Error { .. }));
                if matches!(
                    event,
                    Event::StatusChanged {
                        status: kanna_daemon::protocol::SessionStatus::Busy,
                        ..
                    }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("real busy frame resets the daemon attempt latch");
        release(&mut gate, "refusal");
        timeout(EVENTUAL, async {
            loop {
                let event = observer.recv().await.expect("successor observer closed");
                assert!(!matches!(event, Event::Exit { .. } | Event::Error { .. }));
                if matches!(event, Event::ProviderNotice { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("successor produces a real duplicate refusal for the same run");
        let additional = collect_for(&mut observer, QUIET).await;
        assert!(!additional
            .iter()
            .any(|event| matches!(event, Event::ProviderNotice { .. })));
        assert_eq!(db.provider_rejections_for_task(TASK_ID).unwrap().len(), 1);
        assert_eq!(events_of(&db, "task.provider_quota_rejected").len(), 1);
        assert_eq!(
            events_of(&db, "task.provider_quota_parked").len(),
            1,
            "handoff/reannouncement must not cause a second recovery"
        );
        assert_eq!(db.list_stage_runs_for_task(TASK_ID).unwrap().len(), 1);
        let mut control = daemon.connect().await;
        assert!(matches!(
            control
                .send_command(&DaemonCommand::Kill {
                    session_id: TASK_ID.to_string(),
                })
                .await
                .unwrap(),
            Event::Ok
        ));
        readers.abort_all();
        while readers.join_next().await.is_some() {}
    };
    let watcher = async {
        crate::terminal_watcher::terminal_state_watcher_once(&state, &replacements)
            .await
            .expect("watcher follows old daemon until ShuttingDown");
        resume_watcher.await.expect("successor published");
        crate::terminal_watcher::terminal_state_watcher_once(&state, &replacements).await
    };
    timeout(Duration::from_secs(120), async {
        tokio::select! {
            result = watcher => {
                panic!("real watcher stopped before scenario finished: {result:?}");
            }
            result = &mut relay.0 => panic!("relay stopped early: {result:?}"),
            () = scenario => {}
        }
    })
    .await
    .expect("bounded real producer/consumer regression");
    relay.0.abort();
    let _ = (&mut relay.0).await;
}
