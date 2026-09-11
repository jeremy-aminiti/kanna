//! End-to-end coverage for provider quota-rejection notices.
//!
//! These drive a real daemon process over its unix socket with a real PTY, so
//! they cross the whole path the feature lives on: the `Spawn` command
//! carrying the provider executable, the daemon's own version probe, the
//! notice scan inside the classifier, the per-incarnation latch, and the
//! `ProviderNotice` broadcast kanna-server acts on. Unit tests can prove a
//! rule matches a frame; only this can prove the daemon selected it for the
//! CLI a live session is running and announced it exactly once.
//!
//! The provider is a scripted stand-in that prints the *measured* rejection
//! chrome (from `tests/cli-contract/fixtures/provider-quota-rejection.json`)
//! and then parks, which is what a refused CLI actually does. No real Claude
//! or Codex is driven and no quota is spent.
//!
//! Run single-threaded like the other daemon tests:
//! `cargo test --test provider_quota_notice -- --test-threads=1`

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

static TEST_INSTANCE_COUNTER: AtomicUsize = AtomicUsize::new(0);

const CAPTURES: &str =
    include_str!("../../../tests/cli-contract/fixtures/provider-quota-rejection.json");

const INCIDENT_QUOTA_ROW: &str = concat!(
    "(Fable): ⎿ You've reached your Fable limit. Run /usage-credits ",
    "to continue or switch\", \"resumedFromRunId\": null, ",
    "\"stage\": \"review\", \"status\": \"running\",",
);

/// Every wait below is for something that must eventually happen, never a
/// latency contract: a dev box runs several Rust lanes and their compilers at
/// once, so this ceiling exists only to contain a wedged fixture.
const EVENTUAL: Duration = Duration::from_secs(20);

/// How long a "this must not happen" assertion watches for. Long enough that
/// the daemon's status timer has ticked several times over, short enough that
/// the suite does not stall on a negative.
const QUIET: Duration = Duration::from_secs(6);

struct Capture {
    provider: String,
    cli_version: String,
    rule_id: String,
    scope: Option<String>,
    frame: Vec<String>,
}

fn capture(provider: &str) -> Capture {
    let parsed: Value = serde_json::from_str(CAPTURES).expect("capture fixture parses");
    let entry = parsed
        .as_array()
        .expect("capture fixture is a list")
        .iter()
        .find(|entry| {
            entry["provider"] == provider
                && entry.get("frame").is_some_and(|frame| !frame.is_null())
        })
        .unwrap_or_else(|| panic!("a measured {provider} capture"));
    Capture {
        provider: provider.to_string(),
        cli_version: entry["cliVersion"].as_str().expect("version").to_string(),
        rule_id: entry["ruleId"].as_str().expect("rule id").to_string(),
        scope: entry["scope"].as_str().map(str::to_string),
        frame: entry["frame"]
            .as_array()
            .expect("frame")
            .iter()
            .filter_map(|line| line.as_str().map(str::to_string))
            .collect(),
    }
}

struct DaemonHandle {
    child: Child,
    socket_path: PathBuf,
    dir: PathBuf,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl DaemonHandle {
    fn start(label: &str) -> Self {
        let instance = TEST_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kanna-quota-notice-{label}-{}-{instance}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let socket_path = kanna_runtime_defaults::socket_path(&dir);
        let _ = std::fs::remove_file(&socket_path);
        let pid_path = dir.join("daemon.pid");
        let _ = std::fs::remove_file(&pid_path);

        let child = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_kanna-daemon")))
            .env("KANNA_DAEMON_DIR", dir.to_str().unwrap())
            .spawn()
            .expect("failed to start daemon");

        let deadline = Instant::now() + EVENTUAL;
        while Instant::now() < deadline {
            let pid_matches = std::fs::read_to_string(&pid_path)
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok())
                == Some(child.id());
            if pid_matches && UnixStream::connect(&socket_path).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            UnixStream::connect(&socket_path).is_ok(),
            "daemon was not ready at {socket_path:?}"
        );

        Self {
            child,
            socket_path,
            dir,
        }
    }

    fn connect(&self) -> ClientConn {
        let stream = UnixStream::connect(&self.socket_path).expect("failed to connect to daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        ClientConn {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            pending_line: String::new(),
        }
    }

    fn handoff(&mut self) {
        let mut successor = ChildGuard(
            Command::new(PathBuf::from(env!("CARGO_BIN_EXE_kanna-daemon")))
                .env("KANNA_DAEMON_DIR", &self.dir)
                .spawn()
                .expect("start successor from the same trusted parent"),
        );
        let deadline = Instant::now() + EVENTUAL;
        loop {
            assert!(
                Instant::now() < deadline,
                "successor did not complete handoff"
            );
            assert!(
                successor.0.try_wait().unwrap().is_none(),
                "successor exited during handoff"
            );
            let published = std::fs::read_to_string(self.dir.join("daemon.pid"))
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok())
                == Some(successor.0.id());
            if published && UnixStream::connect(&self.socket_path).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "old daemon retained ownership");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::mem::swap(&mut self.child, &mut successor.0);
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct ClientConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    // A timeout can occur midway through a JSON event. Keep those bytes for
    // the next read, rather than silently losing a notice or Output chunk.
    pending_line: String,
}

impl ClientConn {
    fn send(&mut self, command: &Value) {
        let mut line = serde_json::to_string(command).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn recv_with_timeout(&mut self, timeout: Duration) -> Option<Value> {
        self.reader
            .get_mut()
            .set_read_timeout(Some(timeout))
            .unwrap();
        match self.reader.read_line(&mut self.pending_line) {
            Ok(0) => panic!("daemon event stream closed unexpectedly"),
            Ok(_) => {
                let line = std::mem::take(&mut self.pending_line);
                let event: Value = serde_json::from_str(line.trim()).expect("valid daemon event");
                assert_ne!(event["type"], "Error", "daemon rejected fixture: {event:?}");
                Some(event)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                None
            }
            Err(error) => panic!("failed reading daemon event: {error}"),
        }
    }

    /// Collect every event this session publishes until `within`
    /// elapses. Deliberately not "the next event": the daemon interleaves
    /// output with status and notices on one stream, and a test that asserts
    /// a notice is announced *once* has to see the whole window.
    fn drain(&mut self, session_id: &str, within: Duration) -> Vec<Value> {
        let deadline = Instant::now() + within;
        let mut collected = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return collected;
            }
            match self.recv_with_timeout(remaining.min(Duration::from_millis(200))) {
                Some(event) if event["session_id"] == session_id => collected.push(event),
                _ => continue,
            }
        }
    }

    /// Wait until the session publishes at least one notice, or the window
    /// closes. Returns everything collected either way, so a negative
    /// assertion reads the same events a positive one would.
    fn drain_until_notice(&mut self, session_id: &str, within: Duration) -> Vec<Value> {
        let deadline = Instant::now() + within;
        let mut collected = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return collected;
            }
            if let Some(event) = self.recv_with_timeout(remaining.min(Duration::from_millis(200))) {
                if event["session_id"] == session_id {
                    let is_notice = event["type"] == "ProviderNotice";
                    collected.push(event);
                    if is_notice {
                        return collected;
                    }
                }
            }
        }
    }

    /// Retain interleaved notices while awaiting a command acknowledgement.
    fn wait_for(&mut self, kind: &str, events: &mut Vec<Value>) -> Value {
        let deadline = Instant::now() + EVENTUAL;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "missing {kind}; events: {events:?}");
            if let Some(event) = self.recv_with_timeout(remaining) {
                let matched = event["type"] == kind;
                events.push(event.clone());
                if matched {
                    return event;
                }
            }
        }
    }
}

/// A stand-in for an installed provider CLI that answers `--version`. The
/// daemon probes the executable the server resolved, so a script is a complete
/// substitute for the real binary here — and, unlike the real binary, it does
/// not need an exhausted account to print a refusal.
fn fake_cli(dir: &Path, name: &str, version: &str) -> String {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; fi\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().to_string()
}

/// A shell script that paints the measured refusal frame and then parks —
/// what a refused CLI actually does, and the reason a refusal cannot be
/// classified from a process exit.
fn refusal_script(frame: &[String]) -> String {
    let painted = frame
        .iter()
        .map(|line| line.replace('\\', "\\\\").replace('\'', "'\\''"))
        .map(|line| format!("printf '%s\\r\\n' '{line}'; "))
        .collect::<String>();
    format!("printf '\\033[2J\\033[H'; {painted}sleep 120")
}

fn spawn_refused_session(
    conn: &mut ClientConn,
    session_id: &str,
    capture: &Capture,
    agent_executable: Option<String>,
) {
    let mut command = json!({
        "type": "Spawn",
        "session_id": session_id,
        "executable": "/bin/sh",
        "args": ["-c", refusal_script(&capture.frame)],
        "cwd": "/tmp",
        "env": {},
        "cols": 120,
        "rows": 40,
        "agent_provider": capture.provider,
    });
    if let Some(executable) = agent_executable {
        command["agent_executable"] = Value::String(executable);
    }
    conn.send(&command);
}

fn notices(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "ProviderNotice")
        .collect()
}

/// Writes stdout only after a command arrives on a private FIFO. The PTY's
/// input and composer never participate in fixture synchronization, so even
/// input echo cannot manufacture "fresh output" during the historical phase.
struct GatedNoticeSession {
    control: ClientConn,
    subscriber: ClientConn,
    gate: File,
    session_id: String,
    events: Vec<Value>,
    // Drop connections and the FIFO before the daemon's RAII shutdown.
    _daemon: DaemonHandle,
}

fn paint_lines(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("printf '%s\\r\\n' '{}'; ", line.replace('\'', "'\\''")))
        .collect()
}

impl GatedNoticeSession {
    fn start(
        label: &str,
        capture: &Capture,
        cols: u16,
        seed: &[String],
        quoted: &[String],
    ) -> Self {
        Self::start_with_geometry(label, capture, (cols, 40), (cols, 40), seed, quoted)
    }

    fn start_with_geometry(
        label: &str,
        capture: &Capture,
        spawn_geometry: (u16, u16),
        seed_geometry: (u16, u16),
        seed: &[String],
        quoted: &[String],
    ) -> Self {
        let daemon = DaemonHandle::start(label);
        let fifo = daemon.dir.join("provider-phases");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_c is a live, NUL-terminated path in this fixture's directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        // Opening both ends keeps startup independent of shell scheduling.
        let gate = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let ready = daemon.dir.join("provider-ready");
        let version = if capture.provider == "claude" {
            format!("{} (Claude Code)", capture.cli_version)
        } else {
            format!("codex-cli {}", capture.cli_version)
        };
        let executable = fake_cli(&daemon.dir, "measured-provider", &version);
        let mut subscriber = daemon.connect();
        subscriber.send(&json!({ "type": "Subscribe" }));
        subscriber.wait_for("Ok", &mut Vec::new());
        let mut control = daemon.connect();
        // The seed is ordinary display history, including a complete measured
        // refusal. A matcher-only correction must not make this negative pass.
        let seed_vt = format!("\x1b[2J\x1b[H{}", seed.join("\r\n"));
        control.send(&json!({
            "type": "SeedSnapshot",
            "session_id": label,
            "snapshot": {
                "version": 1, "cols": seed_geometry.0, "rows": seed_geometry.1,
                "cursor_row": 16, "cursor_col": 0, "cursor_visible": true,
                "vt": seed_vt,
            },
        }));
        control.wait_for("Ok", &mut Vec::new());
        let script = format!(
            "exec 3<\"$1\"; : >\"$2\"; \
             while IFS= read -r phase <&3; do \
             case \"$phase\" in \
             startup) printf '\\r\\nfresh-startup-marker\\r\\n' ;; \
             quoted) {} ;; \
             partial) printf '%s' '{}' ;; \
             finish) printf '%s\\r\\n' '{}' ;; \
             refusal) printf '\\r\\n'; {} ;; \
             *) exit 2 ;; esac; done",
            paint_lines(quoted),
            capture
                .frame
                .join("\r\n")
                .split_once("limit")
                .unwrap()
                .0
                .replace('\'', "'\\''"),
            format!(
                "limit{}",
                capture.frame.join("\r\n").split_once("limit").unwrap().1
            )
            .replace('\'', "'\\''"),
            paint_lines(&capture.frame),
        );
        control.send(&json!({
            "type": "Spawn", "session_id": label,
            "executable": "/bin/sh", "args": ["-c", script, "notice-fixture", fifo, ready],
            "cwd": daemon.dir, "env": {}, "cols": spawn_geometry.0, "rows": spawn_geometry.1,
            "agent_provider": capture.provider, "agent_executable": executable,
        }));
        control.wait_for("SessionCreated", &mut Vec::new());
        let deadline = Instant::now() + EVENTUAL;
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "script never reached its private gate"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // No provider output can occur before the gate is released. Register
        // an atomic snapshot/live-output observer before releasing anything.
        subscriber.send(&json!({ "type": "ObserveSnapshot", "session_id": label }));
        let mut events = Vec::new();
        let snapshot = subscriber.wait_for("Snapshot", &mut events);
        assert_eq!(snapshot["session_id"], label);
        assert_eq!(snapshot["snapshot"]["cols"], seed_geometry.0);
        assert_eq!(snapshot["snapshot"]["rows"], seed_geometry.1);
        Self {
            control,
            subscriber,
            gate,
            session_id: label.to_string(),
            events,
            _daemon: daemon,
        }
    }

    fn release(&mut self, phase: &str) {
        writeln!(self.gate, "{phase}").unwrap();
        self.gate.flush().unwrap();
    }

    fn output(&self) -> Vec<u8> {
        self.events
            .iter()
            .filter(|event| event["type"] == "Output")
            .flat_map(|event| serde_json::from_value::<Vec<u8>>(event["data"].clone()).unwrap())
            .collect()
    }

    fn await_output(&mut self, marker: &str) {
        let deadline = Instant::now() + EVENTUAL;
        loop {
            if String::from_utf8_lossy(&self.output()).contains(marker) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "missing PTY output {marker:?}: {:?}",
                self.events
            );
            if let Some(event) = self.subscriber.recv_with_timeout(remaining) {
                self.events.push(event);
            }
        }
    }

    fn settle(&mut self) {
        self.events
            .extend(self.subscriber.drain(&self.session_id, QUIET));
    }

    fn assert_no_notice(&self, phase: &str) {
        assert!(
            notices(&self.events).is_empty(),
            "{phase} is not a current refusal: {:?}",
            self.events
        );
        assert!(
            !self.events.iter().any(|event| event["type"] == "Exit"),
            "the fixture must remain alive through {phase}: {:?}",
            self.events
        );
    }

    fn assert_snapshot_contains(&mut self, text: &str) {
        self.control
            .send(&json!({ "type": "Snapshot", "session_id": self.session_id }));
        let snapshot = self.control.wait_for("Snapshot", &mut Vec::new());
        assert!(
            snapshot["snapshot"]["vt"].as_str().unwrap().contains(text),
            "display history must retain {text:?}: {snapshot:?}"
        );
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.control.send(&json!({
            "type": "Resize", "session_id": self.session_id, "cols": cols, "rows": rows,
        }));
        self.control.wait_for("Ok", &mut Vec::new());
        self.control
            .send(&json!({ "type": "Snapshot", "session_id": self.session_id }));
        let snapshot = self.control.wait_for("Snapshot", &mut Vec::new());
        assert_eq!(snapshot["snapshot"]["cols"], cols);
        assert_eq!(snapshot["snapshot"]["rows"], rows);
    }

    fn handoff(&mut self) {
        self._daemon.handoff();
        self.control = self._daemon.connect();
        self.subscriber = self._daemon.connect();
        self.subscriber.send(&json!({ "type": "Subscribe" }));
        self.subscriber.wait_for("Ok", &mut self.events);
        self.subscriber.send(&json!({
            "type": "ObserveSnapshot", "session_id": self.session_id,
        }));
        self.subscriber.wait_for("Snapshot", &mut self.events);
    }

    fn assert_current_refusal(&mut self, capture: &Capture) {
        self.release("refusal");
        self.assert_refusal_announced(capture);
    }

    fn assert_refusal_announced(&mut self, capture: &Capture) {
        self.await_output(capture.frame.last().unwrap());
        if notices(&self.events).is_empty() {
            self.events.extend(
                self.subscriber
                    .drain_until_notice(&self.session_id, EVENTUAL),
            );
        }
        self.settle();
        let announced = notices(&self.events);
        assert_eq!(
            announced.len(),
            1,
            "one current refusal, one notice: {:?}",
            self.events
        );
        assert_eq!(announced[0]["session_id"], self.session_id);
        assert_eq!(announced[0]["kind"], "quota-rejection");
        assert_eq!(announced[0]["agent_provider"], capture.provider);
        assert_eq!(announced[0]["cli_version"], capture.cli_version);
        assert_eq!(announced[0]["rule_id"], capture.rule_id);
        assert_eq!(announced[0]["scope"].as_str(), capture.scope.as_deref());
        assert_eq!(announced[0]["session_kind"], "pty");
        assert!(announced[0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("limit")));
        assert!(
            !self.events.iter().any(|event| event["type"] == "Exit"),
            "a refusal must not end the live session: {:?}",
            self.events
        );
        self.control.send(&json!({ "type": "List" }));
        let listed = self.control.wait_for("SessionList", &mut Vec::new());
        let session = listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|session| session["session_id"] == self.session_id)
            .expect("the refused session remains listable");
        assert_eq!(
            session["status"], "idle",
            "refusal keeps runtime idle: {session:?}"
        );
    }
}

/// The optional snapshot must carry a clean projection through the actual
/// authenticated daemon-to-daemon transfer, even when the primary holds a seed.
#[test]
fn same_pty_handoff_keeps_seed_separate_and_preserves_partial_current_output() {
    let capture = capture("claude");
    for phase in ["seed", "startup", "partial"] {
        let mut fixture = GatedNoticeSession::start(
            &format!("notice-handoff-{phase}"),
            &capture,
            120,
            &capture.frame,
            &[],
        );
        if phase == "startup" {
            fixture.release("startup");
            fixture.await_output("fresh-startup-marker");
        } else if phase == "partial" {
            fixture.release("partial");
            fixture.await_output("You've reached your Fable ");
        }
        fixture.settle();
        fixture.assert_no_notice("seed/current partial before handoff");
        fixture.handoff();
        fixture.settle();
        fixture.assert_no_notice("same-PTY projection after handoff");
        fixture.assert_snapshot_contains("reached your Fable limit");
        if phase == "partial" {
            // Continue the same logical row after adoption: an empty new
            // projection would lose the prefix and fail the positive assertion.
            fixture.release("finish");
            fixture.assert_refusal_announced(&capture);
        } else {
            fixture.assert_current_refusal(&capture);
        }
    }
}

#[test]
fn handoff_after_a_published_refusal_allows_at_most_one_reannouncement() {
    let capture = capture("claude");
    let mut fixture = GatedNoticeSession::start(
        "notice-handoff-published",
        &capture,
        120,
        &["preserved-history-marker".to_string()],
        &[],
    );
    fixture.assert_current_refusal(&capture);
    fixture.handoff();
    fixture.settle();
    let first_window = notices(&fixture.events).len();
    assert!(
        (1..=2).contains(&first_window),
        "one per daemon incarnation: {:?}",
        fixture.events
    );
    fixture.settle();
    assert_eq!(
        notices(&fixture.events).len(),
        first_window,
        "no repeated settled-frame notice"
    );
    fixture.assert_snapshot_contains("preserved-history-marker");
    // Recovery idempotence belongs to the real server consumer, not this
    // daemon subscriber. Its separate control must count durable recovery rows.
}

impl Drop for GatedNoticeSession {
    fn drop(&mut self) {
        // Retire the gated PTY before stopping its daemon, including on an
        // assertion failure, while the FIFO is still open in this fixture.
        let command = json!({ "type": "Kill", "session_id": self.session_id });
        if let Ok(mut line) = serde_json::to_string(&command) {
            line.push('\n');
            let _ = self.control.writer.write_all(line.as_bytes());
            let _ = self.control.writer.flush();
            let _ = self
                .control
                .reader
                .get_mut()
                .set_read_timeout(Some(EVENTUAL));
            let mut response = String::new();
            let _ = self.control.reader.read_line(&mut response);
        }
    }
}

/// A seeded *valid* refusal is still history. This must fail on the uncorrected
/// producer at the no-output assertion, independent of quote-matcher changes.
#[test]
fn seeded_refusal_is_not_current_before_or_after_unrelated_pty_output() {
    for provider in ["claude", "codex"] {
        let capture = capture(provider);
        let mut fixture = GatedNoticeSession::start(
            &format!("{provider}-seed-provenance"),
            &capture,
            120,
            &capture.frame,
            &[],
        );
        fixture.settle();
        assert!(
            fixture.output().is_empty(),
            "provider gate has emitted no PTY bytes"
        );
        fixture.assert_no_notice("seed before any new output");
        let refusal = capture
            .frame
            .iter()
            .find(|line| line.contains("limit"))
            .unwrap();
        fixture.assert_snapshot_contains(refusal.trim());

        fixture.release("startup");
        fixture.await_output("fresh-startup-marker");
        fixture.settle();
        fixture.assert_no_notice("seed plus unrelated new startup bytes");
        fixture.assert_snapshot_contains(refusal.trim());
        fixture.assert_snapshot_contains("fresh-startup-marker");

        // The same sentence printed freshly must still trigger: a text hash
        // blacklist of everything in the seed would fail this control.
        fixture.assert_current_refusal(&capture);
        fixture.assert_snapshot_contains("fresh-startup-marker");
    }
}

/// Both anchors occur in a prefixed logical VT row. A soft wrap that moves
/// the glyph to column zero does not make it the start of that logical row.
/// This does not cover an identical quotation deliberately printed after a
/// hard line break with the glyph leading the new logical row: that still
/// has the measured refusal shape and cannot be distinguished here.
#[test]
fn quoted_tool_json_refusal_is_not_current_but_real_wrapped_refusal_is() {
    for cols in [120, 48] {
        for provider in ["claude", "codex"] {
            let capture = capture(provider);
            let refusal = capture
                .frame
                .iter()
                .find(|line| line.contains("limit"))
                .unwrap();
            // Make the quoted provider glyph begin a physical wrapped row.
            // A visual row-start anchor alone must not match this continuation.
            let prefix = "{\"matchedText\":\"";
            let quoted_refusal = format!(
                "{}{}",
                " ".repeat(usize::from(cols) - prefix.len()),
                refusal.trim_start(),
            );
            let mut quoted = vec![
                "Tool result: historical task detail".to_string(),
                format!("Earlier provider result: {}", refusal.trim_start()),
                format!("  │ tool_result: {}", refusal.trim_start()),
                serde_json::to_string(&json!({"matchedText": quoted_refusal, "status": "running"}))
                    .unwrap(),
                "quoted-output-complete".to_string(),
            ];
            if provider == "claude" {
                // The recorded incident row contains real chrome, but its
                // logical row begins with a prior result's scope prefix.
                quoted.insert(1, INCIDENT_QUOTA_ROW.to_string());
            }
            let mut fixture = GatedNoticeSession::start(
                &format!("{provider}-quoted-{cols}"),
                &capture,
                cols,
                &["preserved-history-marker".to_string()],
                &quoted,
            );
            fixture.release("quoted");
            fixture.await_output("quoted-output-complete");
            fixture.settle();
            fixture.assert_no_notice("current quoted tool JSON");
            fixture.assert_snapshot_contains("preserved-history-marker");
            fixture.assert_current_refusal(&capture);
            fixture.assert_snapshot_contains("preserved-history-marker");
        }
    }
}

#[test]
fn incident_json_seed_stays_display_only_before_and_after_startup_output() {
    let capture = capture("claude");
    let mut fixture = GatedNoticeSession::start_with_geometry(
        "incident-json-seed",
        &capture,
        (80, 24),
        (167, 65),
        &[INCIDENT_QUOTA_ROW.to_string()],
        &[],
    );
    fixture.settle();
    assert!(fixture.output().is_empty());
    fixture.assert_no_notice("incident JSON before any PTY output");
    fixture.assert_snapshot_contains(INCIDENT_QUOTA_ROW);
    fixture.release("startup");
    fixture.await_output("fresh-startup-marker");
    fixture.settle();
    fixture.assert_no_notice("incident JSON plus unrelated startup bytes");
    fixture.assert_snapshot_contains(INCIDENT_QUOTA_ROW);
    fixture.assert_current_refusal(&capture);
}

/// The incident's seed is 167x65 but the fresh PTY starts at 80x24. Growing a
/// terminal can reflow old history into view; it must not become notice evidence.
#[test]
fn seeded_refusal_stays_historical_across_resize_and_current_refusal_still_reports() {
    let capture = capture("claude");
    let mut fixture = GatedNoticeSession::start_with_geometry(
        "claude-seed-resize",
        &capture,
        (80, 24),
        (167, 65),
        &capture.frame,
        &[],
    );
    fixture.settle();
    assert!(
        fixture.output().is_empty(),
        "provider has not emitted any bytes"
    );
    fixture.assert_no_notice("wide seed before fresh narrow PTY output");
    fixture.release("startup");
    fixture.await_output("fresh-startup-marker");
    fixture.settle();
    fixture.assert_no_notice("wide seed plus unrelated narrow PTY output");
    let refusal = capture
        .frame
        .iter()
        .find(|line| line.contains("limit"))
        .unwrap();
    // A request equal to the kernel spawn geometry is an existing no-op in
    // SessionSizeState even when the display seed has different dimensions.
    // Request an actual size change so this control really exercises reflow.
    for (cols, rows) in [(79, 24), (80, 24), (167, 65)] {
        fixture.resize(cols, rows);
        fixture.settle();
        fixture.assert_no_notice("seed after acknowledged resize/reflow");
        fixture.assert_snapshot_contains(refusal.trim());
        fixture.assert_snapshot_contains("fresh-startup-marker");
    }
    fixture.assert_current_refusal(&capture);
    fixture.assert_snapshot_contains("fresh-startup-marker");
}

/// The incident, end to end on a real PTY: the CLI prints its refusal, parks,
/// and the daemon announces it as a quota rejection naming the scope the
/// provider itself stated.
#[test]
fn a_refused_claude_session_announces_a_quota_rejection() {
    let capture = capture("claude");
    let daemon = DaemonHandle::start("claude-refusal");
    let executable = fake_cli(
        &daemon.dir,
        "claude-refused",
        &format!("{} (Claude Code)", capture.cli_version),
    );

    let mut subscriber = daemon.connect();
    subscriber.send(&json!({ "type": "Subscribe" }));

    let mut control = daemon.connect();
    spawn_refused_session(&mut control, "claude-refusal", &capture, Some(executable));

    let events = subscriber.drain_until_notice("claude-refusal", EVENTUAL);
    let announced = notices(&events);
    assert_eq!(
        announced.len(),
        1,
        "the refusal must be announced, and announced once: {events:?}"
    );
    let notice = announced[0];
    assert_eq!(notice["kind"], "quota-rejection");
    assert_eq!(notice["agent_provider"], "claude");
    assert_eq!(notice["rule_id"], capture.rule_id);
    assert_eq!(notice["session_kind"], "pty");
    assert_eq!(
        notice["scope"].as_str().map(str::to_string),
        capture.scope,
        "the announced scope is exactly what the provider named"
    );
    assert_eq!(
        notice["cli_version"], capture.cli_version,
        "the announcement names the version its rule was selected for"
    );
    assert!(
        notice["text"]
            .as_str()
            .is_some_and(|text| text.contains("reached your Fable limit")),
        "the announcement carries the sentence it matched: {notice:?}"
    );
}

/// Codex states no model in its refusal. The absence has to survive to the
/// announcement: "the CLI did not say" is a different claim from "every model
/// is unavailable", and only one of them is true.
#[test]
fn a_refused_codex_session_announces_no_scope_it_was_not_given() {
    let capture = capture("codex");
    let daemon = DaemonHandle::start("codex-refusal");
    let executable = fake_cli(
        &daemon.dir,
        "codex-refused",
        &format!("codex-cli {}", capture.cli_version),
    );

    let mut subscriber = daemon.connect();
    subscriber.send(&json!({ "type": "Subscribe" }));

    let mut control = daemon.connect();
    spawn_refused_session(&mut control, "codex-refusal", &capture, Some(executable));

    let events = subscriber.drain_until_notice("codex-refusal", EVENTUAL);
    let announced = notices(&events);
    assert_eq!(announced.len(), 1, "one refusal, one notice: {events:?}");
    assert_eq!(announced[0]["kind"], "quota-rejection");
    assert_eq!(announced[0]["agent_provider"], "codex");
    assert!(
        announced[0]["scope"].is_null(),
        "codex names no scope, so none is claimed: {:?}",
        announced[0]
    );
}

/// The refusal stays painted for as long as the session is parked in front of
/// it, and the daemon keeps classifying frames the whole time. One refusal
/// must still be one observation — otherwise every recovery decision downstream
/// would have to de-duplicate a stream.
#[test]
fn a_refusal_that_stays_on_screen_is_announced_once() {
    let capture = capture("claude");
    let daemon = DaemonHandle::start("claude-latched");
    let executable = fake_cli(
        &daemon.dir,
        "claude-latched",
        &format!("{} (Claude Code)", capture.cli_version),
    );

    let mut subscriber = daemon.connect();
    subscriber.send(&json!({ "type": "Subscribe" }));

    let mut control = daemon.connect();
    spawn_refused_session(&mut control, "claude-latched", &capture, Some(executable));

    let events = subscriber.drain("claude-latched", QUIET);
    assert_eq!(
        notices(&events).len(),
        1,
        "a screen the session is parked in front of is one observation: {events:?}"
    );
}

/// A refusal is not a status. The session that printed one is still a live,
/// parked agent, and the daemon must keep saying so — reporting it as dead is
/// what made the original incident look like an ordinary crash.
#[test]
fn a_refused_session_still_reports_itself_idle_and_alive() {
    let capture = capture("claude");
    let daemon = DaemonHandle::start("claude-still-idle");
    let executable = fake_cli(
        &daemon.dir,
        "claude-still-idle",
        &format!("{} (Claude Code)", capture.cli_version),
    );

    let mut subscriber = daemon.connect();
    subscriber.send(&json!({ "type": "Subscribe" }));

    let mut control = daemon.connect();
    spawn_refused_session(
        &mut control,
        "claude-still-idle",
        &capture,
        Some(executable),
    );

    let events = subscriber.drain("claude-still-idle", QUIET);
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "StatusChanged" && event["status"] == "idle"),
        "the refused session parks idle: {events:?}"
    );
    assert!(
        !events.iter().any(|event| event["type"] == "Exit"),
        "the session is alive; nothing here may report it as ended: {events:?}"
    );

    // And it is still listable as an active session.
    control.send(&json!({ "type": "List" }));
    let deadline = Instant::now() + EVENTUAL;
    let listed = loop {
        assert!(Instant::now() < deadline, "the daemon never answered List");
        if let Some(event) = control.recv_with_timeout(Duration::from_millis(500)) {
            if event["type"] == "SessionList" {
                break event;
            }
        }
    };
    assert!(
        listed["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .any(|session| session["session_id"] == "claude-still-idle"),
        "a refused session is still a session: {listed:?}"
    );
}

/// A version-bounded notice is refused for a CLI nobody measured.
///
/// `VersionRange::admits(None)` is permissive for status rules on purpose — a
/// verdict from an unmeasured CLI beats none. A rejection is not a verdict
/// about the screen but a claim that drives automatic provider recovery, so it
/// gets the opposite default.
#[test]
fn an_unprobed_session_announces_no_refusal() {
    let capture = capture("claude");
    let daemon = DaemonHandle::start("claude-unprobed");

    let mut subscriber = daemon.connect();
    subscriber.send(&json!({ "type": "Subscribe" }));

    let mut control = daemon.connect();
    spawn_refused_session(&mut control, "claude-unprobed", &capture, None);

    let events = subscriber.drain("claude-unprobed", QUIET);
    assert!(
        notices(&events).is_empty(),
        "with no measured version, no refusal may be claimed: {events:?}"
    );
    // The session is still classified — only the claim is withheld.
    assert!(
        events.iter().any(|event| event["type"] == "StatusChanged"),
        "status detection is unaffected by the withheld claim: {events:?}"
    );
}

/// An ordinary transcript that talks about limits is not a refusal. This is
/// the failure mode that would be worst: a false positive re-points a healthy
/// task onto another provider for no reason.
#[test]
fn ordinary_output_about_limits_announces_nothing() {
    let daemon = DaemonHandle::start("claude-prose");
    let executable = fake_cli(&daemon.dir, "claude-prose", "2.1.266 (Claude Code)");

    let mut subscriber = daemon.connect();
    subscriber.send(&json!({ "type": "Subscribe" }));

    let prose = Capture {
        provider: "claude".to_string(),
        cli_version: "2.1.266".to_string(),
        rule_id: String::new(),
        scope: None,
        frame: vec![
            "  ⎿ Read docs/specs/accounts-and-billing.md (412 lines)".to_string(),
            "The docs say you've reached your limit when the five-hour window is spent."
                .to_string(),
            "You have 3 usage limit resets available. Run /usage to use one.".to_string(),
            "✻ Churned for 0s · done 1:14 PM".to_string(),
        ],
    };
    let mut control = daemon.connect();
    spawn_refused_session(&mut control, "claude-prose", &prose, Some(executable));

    let events = subscriber.drain("claude-prose", QUIET);
    assert!(
        notices(&events).is_empty(),
        "prose about quota is not a provider refusing a turn: {events:?}"
    );
}
