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

use std::io::{BufRead, BufReader, Write};
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
        }
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
}

impl ClientConn {
    fn send(&mut self, command: &Value) {
        let mut line = serde_json::to_string(command).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn recv_with_timeout(&mut self, timeout: Duration) -> Option<Value> {
        self.reader.get_mut().set_read_timeout(Some(timeout)).ok()?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(line.trim()).ok(),
        }
    }

    /// Collect the notices and statuses this session publishes until `within`
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
                Some(event)
                    if event["session_id"] == session_id
                        && matches!(
                            event["type"].as_str(),
                            Some("ProviderNotice") | Some("StatusChanged")
                        ) =>
                {
                    collected.push(event)
                }
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
                if event["session_id"] == session_id
                    && matches!(
                        event["type"].as_str(),
                        Some("ProviderNotice") | Some("StatusChanged")
                    )
                {
                    let is_notice = event["type"] == "ProviderNotice";
                    collected.push(event);
                    if is_notice {
                        return collected;
                    }
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
