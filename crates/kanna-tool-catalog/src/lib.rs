//! Shared catalog for Kanna MCP and CLI tools.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use url::Url;

pub const KANNA_STAGE_RUN_ID_ENV: &str = "KANNA_STAGE_RUN_ID";
pub const KANNA_COMPLETION_CONTEXT_ENV: &str = "KANNA_COMPLETION_CONTEXT";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionContext {
    /// Immutable identity of the run that received this context at spawn.
    /// Older files omit it; their run-scoped filename is the authoritative
    /// fallback during upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_run_id: Option<String>,
    /// True when the server compiled a context created by an adapter which
    /// predates coordinated context writes. That live process must be
    /// replaced rather than continued into a post.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_writer: bool,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_attempt_key: Option<String>,
    /// Run paired with the legacy single-attempt field. Older contexts omit
    /// this, in which case `completed_attempt_key` belongs to `run_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_run_id: Option<String>,
    /// Bounded replay history retained when a continued post rebinds this
    /// context to its successor run. A retry of the original verdict must
    /// replay against the original run, never complete the successor post.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_attempts: Vec<CompletionAttempt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionAttempt {
    pub run_id: String,
    pub attempt_key: String,
}

const MAX_COMPLETION_ATTEMPTS: usize = 8;

impl CompletionContext {
    pub fn new(run_id: impl Into<String>) -> Self {
        let run_id = run_id.into();
        Self {
            spawned_run_id: Some(run_id.clone()),
            legacy_writer: false,
            run_id,
            completed_attempt_key: None,
            completed_run_id: None,
            completed_attempts: Vec::new(),
        }
    }

    pub fn run_for_attempt(&self, attempt_key: &str) -> Option<&str> {
        self.completed_attempts
            .iter()
            .rev()
            .find(|attempt| attempt.attempt_key == attempt_key)
            .map(|attempt| attempt.run_id.as_str())
            .or_else(|| {
                (self.completed_attempt_key.as_deref() == Some(attempt_key)).then(|| {
                    self.completed_run_id
                        .as_deref()
                        .unwrap_or(self.run_id.as_str())
                })
            })
    }

    pub fn record_completed_attempt(&mut self, run_id: &str, attempt_key: &str) {
        self.completed_attempts
            .retain(|attempt| attempt.attempt_key != attempt_key);
        self.completed_attempts.push(CompletionAttempt {
            run_id: run_id.to_string(),
            attempt_key: attempt_key.to_string(),
        });
        if self.completed_attempts.len() > MAX_COMPLETION_ATTEMPTS {
            self.completed_attempts
                .drain(..self.completed_attempts.len() - MAX_COMPLETION_ATTEMPTS);
        }
        self.completed_attempt_key = Some(attempt_key.to_string());
        self.completed_run_id = Some(run_id.to_string());
    }
}

pub fn completion_attempt_key(body: &Value) -> Result<String, String> {
    let mut canonical = body.clone();
    let object = canonical
        .as_object_mut()
        .ok_or_else(|| "complete-stage request body must be an object".to_string())?;
    object.remove("runId");
    object.remove("completionAttemptKey");
    serde_json::to_string(&canonical)
        .map_err(|error| format!("failed to encode completion attempt: {error}"))
}

pub fn read_completion_context(path: &Path) -> Result<CompletionContext, String> {
    let body = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "failed to read completion context {}: {error}",
            path.display()
        )
    })?;
    serde_json::from_str(&body)
        .map_err(|error| format!("invalid completion context {}: {error}", path.display()))
}

pub fn write_completion_context(path: &Path, context: &CompletionContext) -> Result<(), String> {
    mutate_completion_context(path, |_| Ok(context.clone())).map(|_| ())
}

/// Atomically read-modify-write a completion context across the server, CLI,
/// and MCP processes. The adjacent lock file is stable across the atomic
/// rename used to publish the JSON payload.
pub fn mutate_completion_context(
    path: &Path,
    mutate: impl FnOnce(Option<CompletionContext>) -> Result<CompletionContext, String>,
) -> Result<CompletionContext, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create completion context directory: {error}"))?;
    }
    let lock_path = path.with_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("failed to open completion context lock: {error}"))?;
    lock_completion_file(&lock)?;
    let current =
        match std::fs::read_to_string(path) {
            Ok(body) => Some(serde_json::from_str(&body).map_err(|error| {
                format!("invalid completion context {}: {error}", path.display())
            })?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "failed to read completion context {}: {error}",
                    path.display()
                ))
            }
        };
    let context = mutate(current)?;
    let body = serde_json::to_vec(&context)
        .map_err(|error| format!("failed to encode completion context: {error}"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    std::fs::write(&temp, body)
        .map_err(|error| format!("failed to write completion context: {error}"))?;
    std::fs::rename(&temp, path)
        .map_err(|error| format!("failed to publish completion context: {error}"))?;
    Ok(context)
}

#[cfg(unix)]
fn lock_completion_file(file: &std::fs::File) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "failed to lock completion context: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
fn lock_completion_file(_file: &std::fs::File) -> Result<(), String> {
    Ok(())
}

const BUNDLED_CATALOG: &str = include_str!("catalog.json");

/// MCP clients abort a `tools/call` on their own timer — Codex and Claude Code
/// both cut at 300s — and when they do the calling agent loses the result
/// entirely, including the tool's own "still running" answer.
/// Header every first-party Kanna HTTP client sets so the server can name the
/// caller in an error log.
///
/// Every request on the local listener arrives from `127.0.0.1`, so the peer
/// address separates nothing: the CLI, this adapter, the desktop, and a
/// sidecar are indistinguishable. A runaway client once wrote a million
/// identical 400s into `kanna-server.log` and the line named neither the
/// process that sent it nor the query it failed on. This is diagnostic only —
/// it is caller-declared, unverified, and grants no authority whatsoever.
pub const CLIENT_IDENTITY_HEADER: &str = "x-kanna-client";

/// `<name>/<version> pid=<pid>[ task=<task id>]` — enough to find the process
/// while it is still running, and to name the task session it belongs to after
/// it is gone.
pub fn client_identity_header_value(name: &str, version: &str) -> String {
    let mut identity = format!("{name}/{version} pid={}", std::process::id());
    if let Some(task_id) = std::env::var("KANNA_TASK_ID")
        .ok()
        .map(|task_id| task_id.trim().to_string())
        .filter(|task_id| {
            !task_id.is_empty()
                && task_id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
    {
        identity.push_str(&format!(" task={task_id}"));
    }
    identity
}

pub const CLIENT_TOOL_CALL_BUDGET_SECS: u64 = 300;

/// Hard ceiling on a single `kanna_wait_task` window, enforced here rather than
/// only in `catalog.json` so an override catalog cannot reintroduce a wait the
/// client is guaranteed to kill. The gap to `CLIENT_TOOL_CALL_BUDGET_SECS`
/// leaves room for the final poll and the response render.
pub const MAX_WAIT_TIMEOUT_SECS: u64 = 240;

/// Waits are designed to be called in a loop, so the default is the full
/// (bounded) window: a wait that hands back the task's current state at 240s is
/// strictly better than one the client kills at 300s.
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = MAX_WAIT_TIMEOUT_SECS;

/// Seconds between polls when the caller does not choose.
pub const DEFAULT_WAIT_POLL_SECS: u64 = 3;

const _: () = assert!(
    MAX_WAIT_TIMEOUT_SECS + 60 <= CLIENT_TOOL_CALL_BUDGET_SECS,
    "a wait window must leave the client room to receive the answer, or the \
     call is killed and the agent loses the result"
);
const _: () = assert!(DEFAULT_WAIT_TIMEOUT_SECS <= MAX_WAIT_TIMEOUT_SECS);

pub fn clamp_wait_timeout_secs(timeout_secs: u64) -> u64 {
    timeout_secs.min(MAX_WAIT_TIMEOUT_SECS)
}

/// Rows in one `/v1/task-events` response when the caller does not choose, and
/// the ceiling it may raise that to. Declared here rather than only in
/// `catalog.json` for the same reason as the wait window: the server, the
/// MCP fan-in and the CLI must agree on the page size, and an override catalog
/// must not be able to move it.
pub const DEFAULT_TASK_EVENT_LIMIT: i64 = 100;
pub const MAX_TASK_EVENT_LIMIT: i64 = 500;

/// Ceiling on `debounceMs` and `minIntervalMs`. The remaining wait window
/// already caps both; this only stops a caller asking to hold a response
/// longer than any batch it could plausibly be waiting for.
pub const MAX_TASK_EVENT_HOLD_MS: u64 = 60_000;

pub fn clamp_task_event_limit(limit: Option<i64>) -> i64 {
    limit
        .unwrap_or(DEFAULT_TASK_EVENT_LIMIT)
        .clamp(1, MAX_TASK_EVENT_LIMIT)
}

/// `minEvents` is capped by the page size: a caller that asks to wait for more
/// events than one response can carry would otherwise always run to timeout.
pub fn clamp_task_event_min_events(min_events: Option<i64>, limit: i64) -> usize {
    min_events.unwrap_or(1).clamp(1, limit) as usize
}

pub fn clamp_task_event_hold_ms(hold_ms: Option<u64>) -> u64 {
    hold_ms.unwrap_or(0).min(MAX_TASK_EVENT_HOLD_MS)
}

/// Whether a batched task-event wait may return now — the one rule shared by
/// the server's single-machine wait, its cross-machine fan-out, and the MCP
/// client fan-in, so `minEvents` counts the same events on every path.
///
/// `hasMore` and a full page both mean waiting longer cannot add anything to
/// *this* response, so they release it whatever the caller asked to hold for.
/// Otherwise the batch is ready once it holds `min_events` and every hold
/// window (`debounceMs`, `minIntervalMs`) has closed. Callers pass
/// `hold_elapsed` because each owns its own clock — the shared part is the
/// rule, not the timekeeping.
pub fn task_event_batch_is_complete(
    collected: usize,
    has_more: bool,
    limit: i64,
    min_events: usize,
    hold_elapsed: bool,
) -> bool {
    if has_more || collected >= limit.max(0) as usize {
        return true;
    }
    collected >= min_events && hold_elapsed
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalog {
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub guides: Vec<GuideDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GuideDef {
    pub topic: String,
    pub title: String,
    pub summary: String,
    pub sections: Vec<GuideSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GuideSection {
    pub title: String,
    pub body: String,
    /// JSON pointers whose schema `description` is generated from this body.
    #[serde(default)]
    pub schema_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub method: Method,
    pub path: String,
    #[serde(rename = "response")]
    pub response_kind: ResponseKind,
    #[serde(default)]
    pub params: Vec<ParamDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParamDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub param_type: ParamType,
    pub required: bool,
    pub location: ParamLoc,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub min: Option<u64>,
    #[serde(default)]
    pub max: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Patch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseKind {
    Json,
    Text,
    Wait,
    RuntimeInfo,
    Guide,
}

/// Identity owned by the client-side adapter executing a catalog tool. The
/// connected HTTP server is intentionally represented separately in the
/// runtime-info result because the two binaries can have different versions
/// and lifecycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeAdapterIdentity<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub mcp_protocol_version: Option<&'a str>,
    pub task_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SafeServerStatus {
    state: String,
    desktop_id: String,
    desktop_name: String,
    version: String,
    environment: String,
    lan_host: String,
    lan_port: u16,
    #[serde(default)]
    ksp_stream_version: Option<u8>,
    #[serde(default)]
    agent_api_tools: Option<Vec<String>>,
    #[serde(default)]
    write_path_health: Option<SafeWritePathHealth>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SafeWritePathHealth {
    healthy: bool,
    status: String,
    active_workspace_commands: usize,
    max_workspace_commands: usize,
    long_running_workspace_commands: usize,
    oldest_workspace_command_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    String,
    Integer,
    Boolean,
    StringArray,
    Object,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParamLoc {
    Path,
    Query,
    Body,
    Routing,
    /// Consumed by the calling adapter's policy layer (see
    /// [`args_with_self_exclusion`]) and never serialized into the request.
    /// Declared in the catalog so every client advertises and validates it.
    Client,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitUntil {
    Reconcile,
    Finished,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitSpec {
    pub task_id: String,
    pub timeout_secs: u64,
    pub poll_secs: u64,
    pub until: WaitUntil,
}

/// `stage_run.status` values that mean the run is over as a matter of record.
///
/// `cancelled` is deliberately absent: it is the transient state a rerun,
/// resume, or close passes through on the way to starting a replacement run,
/// so treating it as finished would resolve a wait on a task Kanna is about to
/// restart.
pub fn run_status_is_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed")
}

/// The three facts a wait predicate needs out of a task detail.
///
/// It exists so every client surface that answers "has this task finished?" —
/// `kanna-mcp`, the typed `kanna-cli` wait, and the catalog-driven `kanna-cli`
/// wait — reads the same fields the same way. The three used to carry their own
/// copy of the predicate, which is how they drifted.
///
/// `activity` is deliberately absent. It is a display value blending the
/// runtime and read dimensions, so `unread` means "a human has not read the
/// latest output" — which a *working* task satisfies. Waits read
/// `runtimeState`, the runtime dimension, instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WaitTaskState<'a> {
    pub closed: bool,
    pub runtime_state: Option<&'a str>,
    pub runtime_settled: bool,
    pub latest_run_status: Option<&'a str>,
}

/// Whether a task has reached the state a wait was asked to block for.
///
/// `Finished` means the work stopped: the task closed, its latest `stage_run`
/// recorded a terminal verdict, or its agent session ended without a
/// replacement (`runtimeState == "exited"`). All three are durable records of
/// a termination, written where the termination happens.
///
/// It used to also resolve on `activity == "unread"`, which is a read-state
/// value, not a termination: an actively working task whose last output nobody
/// has read carries `unread` too, so a wait could report a busy agent as
/// finished. An agent whose *process* ends without recording a verdict is
/// covered positively by `exited`.
///
/// `idle` deliberately does not resolve: the daemon reports `idle` for a task
/// parked at its composer between turns and for one that never started, and
/// neither has finished anything. Termination, not quiet, is the signal.
///
/// The default `Reconcile` also accepts `runtimeSettled`: the server's
/// observation of non-busy runtime after the existing debounce. It surfaces
/// parked work without turning that observation into a completion verdict.
/// An older server without that field retains termination-only behavior.
pub fn task_state_matches_wait_until(state: WaitTaskState<'_>, until: WaitUntil) -> bool {
    match until {
        WaitUntil::Reconcile => {
            (state.runtime_settled
                && matches!(state.runtime_state, Some("idle" | "waiting" | "exited")))
                || task_state_matches_wait_until(state, WaitUntil::Finished)
        }
        WaitUntil::Closed => state.closed,
        WaitUntil::Finished => {
            state.closed
                || state.latest_run_status.is_some_and(run_status_is_terminal)
                || state.runtime_state == Some("exited")
        }
    }
}

/// Read a `WaitTaskState` out of a raw task-detail JSON body.
pub fn wait_task_state(task: &Value) -> WaitTaskState<'_> {
    WaitTaskState {
        closed: task.get("closedAt").is_some_and(|value| !value.is_null()),
        runtime_state: task.get("runtimeState").and_then(Value::as_str),
        runtime_settled: task
            .get("runtimeSettled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        latest_run_status: task
            .get("latestRun")
            .and_then(|run| run.get("status"))
            .and_then(Value::as_str),
    }
}

/// `task_state_matches_wait_until` for a raw task-detail JSON body.
pub fn task_value_matches_wait_until(task: &Value, until: WaitUntil) -> bool {
    task_state_matches_wait_until(wait_task_state(task), until)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedRequest {
    pub kind: ResponseKind,
    pub method: Method,
    pub path: String,
    pub body: Value,
    pub wait: Option<WaitSpec>,
    /// Adapter-only routing metadata. It is declared alongside ordinary tool
    /// parameters, but is never serialized into the target server request.
    pub machine_id: Option<String>,
    /// Adapter-owned result for tools that do not make an HTTP request.
    pub local_response: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLoad {
    pub catalog: Catalog,
    pub watch_source: Option<PathBuf>,
    pub warning: Option<String>,
}

pub fn bundled_catalog() -> Catalog {
    ensure_required_adapter_content(parsed_bundled_catalog())
}

fn parsed_bundled_catalog() -> Catalog {
    serde_json::from_str(BUNDLED_CATALOG)
        .unwrap_or_else(|error| panic!("bundled kanna tool catalog is invalid: {error}"))
}

/// Runtime identity, account-scoped machine discovery, and bundled guidance
/// are adapter boundaries, not repo-customizable transport shortcuts. Catalog
/// overrides may add or replace ordinary HTTP tools and guide page contents,
/// but the bundled declarations always own these adapter-local tools.
fn ensure_required_adapter_content(mut catalog: Catalog) -> Catalog {
    let bundled = parsed_bundled_catalog();
    let mut required = bundled
        .tools
        .into_iter()
        .filter(|tool| {
            matches!(
                tool.name.as_str(),
                "kanna_info" | "kanna_list_machines" | "kanna_guide"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        required.len(),
        3,
        "bundled catalog must declare adapter-owned tools"
    );
    catalog.tools.retain(|tool| {
        !matches!(
            tool.name.as_str(),
            "kanna_info" | "kanna_list_machines" | "kanna_guide"
        )
    });
    required.append(&mut catalog.tools);
    catalog.tools = required;
    if catalog.guides.is_empty() {
        catalog.guides = bundled.guides;
    }
    catalog
}

impl Catalog {
    pub fn guide_topics(&self) -> Vec<&str> {
        self.guides
            .iter()
            .map(|guide| guide.topic.as_str())
            .collect()
    }

    pub fn guide(&self, topic: &str) -> Result<&GuideDef, String> {
        self.guides
            .iter()
            .find(|guide| guide.topic == topic)
            .ok_or_else(|| {
                format!(
                    "unknown guide topic: {topic} (available topics: {})",
                    self.guide_topics().join(", ")
                )
            })
    }

    pub fn render_guide(&self, topic: &str) -> Result<String, String> {
        let guide = self.guide(topic)?;
        let mut output = format!("# {}\n\n{}", guide.title, guide.summary);
        for section in &guide.sections {
            output.push_str(&format!("\n\n## {}\n\n{}", section.title, section.body));
        }
        Ok(output)
    }

    pub fn config_schema_descriptions(&self) -> BTreeMap<&str, &str> {
        self.guides
            .iter()
            .flat_map(|guide| &guide.sections)
            .flat_map(|section| {
                section
                    .schema_paths
                    .iter()
                    .map(move |path| (path.as_str(), section.body.as_str()))
            })
            .collect()
    }
}

/// Build the shared `kanna_info` result from client-owned connection metadata
/// and the server's raw status response. Deserializing into an explicit type is
/// the security boundary: fields such as `pairingCode`, compatibility aliases,
/// and any future status additions are ignored unless deliberately allow-listed
/// here.
pub fn runtime_info_snapshot(
    effective_base_url: &str,
    adapter: RuntimeAdapterIdentity<'_>,
    status_result: Result<Value, String>,
    client_tool_names: &[String],
) -> Value {
    let task_context = adapter
        .task_id
        .filter(|task_id| !task_id.trim().is_empty())
        .map(|task_id| serde_json::json!({ "taskId": task_id }));
    let parsed_url = Url::parse(effective_base_url).ok();
    let connection = serde_json::json!({
        "effectiveBaseUrl": effective_base_url,
        "host": parsed_url.as_ref().and_then(Url::host_str),
        "port": parsed_url.as_ref().and_then(Url::port_or_known_default),
    });
    let client_adapter = serde_json::json!({
        "name": adapter.name,
        "version": adapter.version,
        "mcpProtocolVersion": adapter.mcp_protocol_version,
    });

    let (server_status, lan_advertised_endpoint, agent_api) = match status_result {
        Ok(raw_status) => match serde_json::from_value::<SafeServerStatus>(raw_status) {
            Ok(status) => {
                let lan_endpoint = serde_json::json!({
                    "host": status.lan_host,
                    "port": status.lan_port,
                });
                let agent_api =
                    agent_api_skew(client_tool_names, status.agent_api_tools.as_deref());
                let server_status = serde_json::json!({
                    "available": true,
                    "state": status.state,
                    "environment": status.environment,
                    "version": status.version,
                    "desktop": {
                        "id": status.desktop_id,
                        "name": status.desktop_name,
                    },
                    "capabilityVersions": {
                        "kspStream": status.ksp_stream_version,
                    },
                    "writePathHealth": status.write_path_health,
                });
                (server_status, lan_endpoint, agent_api)
            }
            Err(error) => (
                serde_json::json!({
                    "available": false,
                    "error": format!("GET /v1/status returned an invalid identity payload: {error}"),
                }),
                Value::Null,
                agent_api_unreadable(client_tool_names),
            ),
        },
        Err(error) => (
            serde_json::json!({
                "available": false,
                "error": error,
            }),
            Value::Null,
            agent_api_unreadable(client_tool_names),
        ),
    };

    serde_json::json!({
        "clientAdapter": client_adapter,
        "connection": connection,
        "serverStatus": server_status,
        "lanAdvertisedEndpoint": lan_advertised_endpoint,
        "agentApi": agent_api,
        "taskContext": task_context,
    })
}

/// Hint text shared by every skew verdict that cannot confirm a tool is
/// routable, so an agent reads the same instruction whichever way the check
/// came up short.
const AGENT_API_UNVERIFIED_HINT: &str =
    "Treat any tool your instructions mandate as unverified: a \
     404 from it means this server does not serve the route, which is not the same as the route \
     answering \"none\". Do not record an empty result as fact — say the surface was unavailable.";

/// Compare the tools this client advertises against the tools the connected
/// server says it can serve.
///
/// The two are separate binaries with separate lifecycles, and a released app
/// can lag a working-tree client by hundreds of commits. Without this an agent
/// whose instructions mandate a tool discovers its absence only when the call
/// 404s, and a 404 cannot be told apart from a legitimate empty answer — so a
/// fan-out orchestrator can silently record "no children" for a server that
/// simply cannot be asked.
fn agent_api_skew(client_tool_names: &[String], server_tools: Option<&[String]>) -> Value {
    let Some(server_tools) = server_tools else {
        return serde_json::json!({
            "status": "unknown",
            "serverAdvertisesCapabilities": false,
            "clientToolCount": client_tool_names.len(),
            "note": format!(
                "This server predates agent-API capability advertisement, so which of the {} tools \
                 this client exposes are actually routable cannot be determined. It is therefore \
                 older than this client, and tools added since its build will 404. {}",
                client_tool_names.len(),
                AGENT_API_UNVERIFIED_HINT
            ),
        });
    };
    let unavailable = client_tool_names
        .iter()
        .filter(|name| !server_tools.iter().any(|served| served == *name))
        .cloned()
        .collect::<Vec<_>>();
    if unavailable.is_empty() {
        return serde_json::json!({
            "status": "current",
            "serverAdvertisesCapabilities": true,
            "clientToolCount": client_tool_names.len(),
            "unavailableTools": Vec::<String>::new(),
        });
    }
    serde_json::json!({
        "status": "server_behind",
        "serverAdvertisesCapabilities": true,
        "clientToolCount": client_tool_names.len(),
        "unavailableTools": unavailable,
        "note": format!(
            "The connected server does not serve {} of the tools this client exposes, so it is \
             older than this client. {}",
            unavailable.len(),
            AGENT_API_UNVERIFIED_HINT
        ),
    })
}

/// The status read failed or was unparseable, so nothing is known about the
/// server's surface. Reported as `unknown` rather than omitted: a missing block
/// would read as "no skew".
fn agent_api_unreadable(client_tool_names: &[String]) -> Value {
    serde_json::json!({
        "status": "unknown",
        "serverAdvertisesCapabilities": false,
        "clientToolCount": client_tool_names.len(),
        "note": format!(
            "The server's status could not be read, so its agent-API surface is unknown. {}",
            AGENT_API_UNVERIFIED_HINT
        ),
    })
}

pub fn load_catalog(cwd: &Path) -> CatalogLoad {
    let env_path = std::env::var_os("KANNA_MCP_CATALOG").map(PathBuf::from);
    let file_path = env_path.or_else(|| {
        let local = cwd.join(".kanna/mcp-tools.json");
        local.exists().then_some(local)
    });

    let Some(path) = file_path else {
        return CatalogLoad {
            catalog: bundled_catalog(),
            watch_source: None,
            warning: None,
        };
    };

    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<Catalog>(&contents) {
            Ok(catalog) => CatalogLoad {
                catalog: ensure_required_adapter_content(catalog),
                watch_source: Some(path),
                warning: None,
            },
            Err(e) => CatalogLoad {
                catalog: bundled_catalog(),
                watch_source: Some(path.clone()),
                warning: Some(format!(
                    "failed to parse catalog override {}: {e}",
                    path.display()
                )),
            },
        },
        Err(e) => CatalogLoad {
            catalog: bundled_catalog(),
            watch_source: Some(path.clone()),
            warning: Some(format!(
                "failed to read catalog override {}: {e}",
                path.display()
            )),
        },
    }
}

impl Catalog {
    pub fn tools_list_value(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|tool| {
                    let mut entry = serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": input_schema(tool),
                    });
                    if tool.method == Method::Get {
                        entry["annotations"] = serde_json::json!({ "readOnlyHint": true });
                    }
                    entry
                })
                .collect(),
        )
    }

    fn find_tool(&self, name: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    /// The declaration for one parameter, so a surface that receives arguments
    /// as untyped text can coerce them by declaration instead of guessing.
    pub fn find_param(&self, tool_name: &str, param_name: &str) -> Option<&ParamDef> {
        self.find_tool(tool_name)?
            .params
            .iter()
            .find(|param| param.name == param_name)
    }
}

impl ParamDef {
    /// Parse the `value` half of a `key=value` command-line argument into the
    /// JSON value this parameter declares.
    ///
    /// The declaration decides the type; the shape of the text never does.
    /// Guessing from the text (JSON-parsing the value and falling back to a
    /// string) silently retyped every all-digit string: task ids are hex, so
    /// roughly one in 16^8 became a number and the catalog then rejected it
    /// with `task_id must be a string` — an error that reads like a bad id
    /// rather than the CLI bug it was.
    pub fn parse_cli_value(&self, raw: &str) -> Result<Value, String> {
        match self.param_type {
            ParamType::String => Ok(Value::String(raw.to_string())),
            ParamType::Integer => raw
                .trim()
                .parse::<u64>()
                .map(|number| Value::Number(number.into()))
                .map_err(|_| format!("{} must be an unsigned integer, got {raw}", self.name)),
            ParamType::Boolean => raw
                .trim()
                .parse::<bool>()
                .map(Value::Bool)
                .map_err(|_| format!("{} must be true or false, got {raw}", self.name)),
            ParamType::StringArray => parse_cli_string_array(&self.name, raw),
            ParamType::Object => {
                let parsed = serde_json::from_str::<Value>(raw)
                    .map_err(|e| format!("{} must be a JSON object: {e}", self.name))?;
                if !parsed.is_object() {
                    return Err(format!("{} must be a JSON object", self.name));
                }
                Ok(parsed)
            }
        }
    }
}

/// A JSON array when the value is spelled as one, otherwise the plain-CLI
/// comma-separated list — the same spelling `query_value` emits on the way out.
fn parse_cli_string_array(name: &str, raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    let values = if trimmed.starts_with('[') {
        let parsed = serde_json::from_str::<Value>(trimmed)
            .map_err(|e| format!("{name} must be an array of strings: {e}"))?;
        string_array_value(&parsed, name)?
    } else {
        trimmed
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()
    };
    Ok(Value::Array(
        values.into_iter().map(Value::String).collect(),
    ))
}

fn input_schema(tool: &ToolDef) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    for param in &tool.params {
        let mut property = match param.param_type {
            ParamType::String => serde_json::json!({ "type": "string" }),
            ParamType::Integer => serde_json::json!({ "type": "integer" }),
            ParamType::Boolean => serde_json::json!({ "type": "boolean" }),
            ParamType::StringArray => {
                serde_json::json!({ "type": "array", "items": { "type": "string" } })
            }
            ParamType::Object => serde_json::json!({ "type": "object" }),
        };

        if let Some(description) = &param.description {
            property["description"] = Value::String(description.clone());
        }

        if let Some(enum_values) = &param.enum_values {
            let enum_values = Value::Array(
                enum_values
                    .iter()
                    .map(|value| Value::String(value.clone()))
                    .collect(),
            );
            // On a list the vocabulary belongs to the items. Setting it on the
            // array would say the array itself must equal one of the strings,
            // which no client can satisfy.
            if param.param_type == ParamType::StringArray {
                property["items"]["enum"] = enum_values;
            } else {
                property["enum"] = enum_values;
            }
        }

        if let Some(default) = &param.default {
            property["default"] = default.clone();
        }
        if param.param_type == ParamType::Integer {
            if let Some(min) = param.min {
                property["minimum"] = Value::Number(min.into());
            }
            if let Some(max) = param.max {
                property["maximum"] = Value::Number(max.into());
            }
        }

        properties.insert(param.name.clone(), property);
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }

    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(schema)
}

pub fn resolve_request(
    catalog: &Catalog,
    tool_name: &str,
    args: &Value,
) -> Result<ResolvedRequest, String> {
    let tool = catalog.find_tool(tool_name).ok_or_else(|| {
        let available = catalog
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown tool: {tool_name} (available tools: {available})")
    })?;
    reject_unknown_args(tool, args)?;
    let mut path = tool.path.clone();
    let mut body = Map::new();
    let mut query = Vec::new();
    let mut machine_id = None;

    for param in &tool.params {
        let Some(value) = value_for_param(tool, param, args)? else {
            continue;
        };
        match param.location {
            ParamLoc::Path => {
                let value = string_value(&value, &param.name)?;
                path = path.replace(&format!("{{{}}}", param.name), &encode_path_segment(&value));
            }
            ParamLoc::Query => {
                let key = param.key.as_deref().unwrap_or(&param.name);
                let rendered = query_value(&value, &param.name)?;
                query.push(format!(
                    "{}={}",
                    encode_path_segment(key),
                    encode_path_segment(&rendered)
                ));
            }
            ParamLoc::Body => {
                if tool.response_kind == ResponseKind::Wait {
                    continue;
                }
                let key = param.key.as_deref().unwrap_or(&param.name);
                body.insert(key.to_string(), value);
            }
            ParamLoc::Client => {}
            ParamLoc::Routing => {
                if param.name != "machine_id" {
                    return Err(format!(
                        "unsupported routing argument on {}: {}",
                        tool.name, param.name
                    ));
                }
                let value = string_value(&value, &param.name)?;
                if value.trim().is_empty() {
                    return Err("machine_id must not be empty".to_string());
                }
                machine_id = Some(value);
            }
        }
    }

    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }

    let wait = if tool.response_kind == ResponseKind::Wait {
        Some(wait_spec(tool, args)?)
    } else {
        None
    };

    let local_response = if tool.response_kind == ResponseKind::Guide {
        let topic = body
            .get("topic")
            .and_then(Value::as_str)
            .ok_or_else(|| "guide request missing topic".to_string())?;
        let guide = catalog.guide(topic)?;
        Some(serde_json::json!({
            "topic": guide.topic,
            "title": guide.title,
            "content": catalog.render_guide(topic)?,
        }))
    } else {
        None
    };

    Ok(ResolvedRequest {
        kind: tool.response_kind,
        method: tool.method,
        path,
        body: Value::Object(body),
        wait,
        machine_id,
        local_response,
    })
}

/// Return the current task id whose repository must be resolved before this
/// request can be serialized.
///
/// Repository defaulting is tool policy, so it lives beside request
/// resolution rather than in either transport adapter. The adapters only own
/// the HTTP read needed to turn the durable task id into its machine-local
/// repository id.
pub fn repo_context_task_id(
    tool_name: &str,
    args: &Value,
    task_id: Option<&str>,
    remote_machine_id: Option<&str>,
) -> Result<Option<String>, String> {
    let create_task = tool_name == "kanna_create_task";
    let task_listing = matches!(tool_name, "kanna_list_recent_tasks" | "kanna_search_tasks");
    let task_watch = tool_name == "kanna_wait_events";
    if !create_task && !task_listing && !task_watch {
        return Ok(None);
    }

    let has_repo_id = args.get("repo_id").is_some();
    let all_repos = args.get("all_repos").and_then(Value::as_bool) == Some(true);
    let all_machines = args.get("all_machines").and_then(Value::as_bool) == Some(true);
    if task_listing && has_repo_id && all_machines {
        return Err(
            "repo_id and all_machines cannot be used together; repository IDs are machine-local, so omit repo_id for an account-wide all_machines listing"
                .to_string(),
        );
    }
    if task_listing && has_repo_id && all_repos {
        return Err("repo_id and all_repos cannot be used together".to_string());
    }

    let watch_has_scope = args.get("task_ids").is_some()
        || args.get("parent_task_id").is_some()
        || args.get("repo_id").is_some()
        || args.get("repo_remote_url_hash").is_some();
    if has_repo_id
        || (task_listing && (all_repos || all_machines))
        || (task_watch && watch_has_scope)
    {
        return Ok(None);
    }

    let task_id = match task_id.filter(|value| !value.trim().is_empty()) {
        Some(task_id) => task_id,
        None if create_task => {
            return Err("repo_id is required when KANNA_TASK_ID is not available".to_string())
        }
        None => return Ok(None),
    };

    if let Some(machine_id) = remote_machine_id {
        let operation = if create_task {
            "creating a task"
        } else if task_listing {
            "listing tasks"
        } else {
            "watching tasks"
        };
        return Err(format!(
            "repo_id is required when {operation} on machine {machine_id} from a task session; repository IDs are machine-local, so call kanna_list_repos with the same machine_id and pass the remote repository explicitly"
        ));
    }

    Ok(Some(task_id.to_string()))
}

/// The task whose events a repository-scoped wait drops so it does not wake
/// itself, or `None` when no exclusion applies.
///
/// Self-exclusion is tool policy shared by every catalog client, so it lives
/// here beside repository defaulting. It applies only when the wait is
/// repository-scoped — an explicit `repo_id` / `repo_remote_url_hash`, or the
/// task-session repository default — because an explicit `task_ids` list is
/// already literal and a `parent_task_id` scope excludes the parent
/// structurally. `include_self` turns the default off; explicit
/// `exclude_task_ids` entries are never touched by either.
pub fn task_event_self_exclusion(
    explicit_task_scope: bool,
    include_self: bool,
    current_task_id: Option<&str>,
) -> Option<String> {
    if explicit_task_scope || include_self {
        return None;
    }
    current_task_id
        .map(str::trim)
        .filter(|task_id| !task_id.is_empty())
        .map(str::to_string)
}

/// Apply [`task_event_self_exclusion`] to `kanna_wait_events` arguments,
/// appending the caller task to `exclude_task_ids` and consuming the
/// client-only `include_self` flag. Every other tool passes through unchanged.
pub fn args_with_self_exclusion(
    tool_name: &str,
    args: &Value,
    current_task_id: Option<&str>,
) -> Result<Value, String> {
    if tool_name != "kanna_wait_events" {
        return Ok(args.clone());
    }
    let mut resolved_args = args
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;
    let include_self = match resolved_args.remove("include_self") {
        Some(Value::Bool(include_self)) => include_self,
        Some(Value::Null) | None => false,
        Some(_) => return Err("include_self must be a boolean".to_string()),
    };
    // Match the server's scope resolution: empty task-id arrays and blank
    // parent ids fall through to repository scope, so they must not disable
    // the repository watch's default self-exclusion.
    let explicit_task_ids = match resolved_args.get("task_ids") {
        Some(Value::Null) | None => false,
        Some(value) => string_array_value(value, "task_ids")?
            .iter()
            .any(|task_id| !task_id.trim().is_empty()),
    };
    let explicit_parent_scope = resolved_args
        .get("parent_task_id")
        .and_then(Value::as_str)
        .is_some_and(|parent_task_id| !parent_task_id.trim().is_empty());
    let explicit_task_scope = explicit_task_ids || explicit_parent_scope;
    // Echo suppression is not scope-dependent the way self-exclusion is: the
    // loop it exists to break — send input to a child, wait, wake on the
    // delivery announcement — happens under an explicit `task_ids` scope. A
    // caller in a task session gets it by default on every scope, and an
    // explicit value always wins.
    if current_task_id
        .map(str::trim)
        .is_some_and(|task_id| !task_id.is_empty())
        && !matches!(resolved_args.get("exclude_own"), Some(value) if !value.is_null())
    {
        resolved_args.insert("exclude_own".to_string(), Value::Bool(true));
    }
    let Some(self_task_id) =
        task_event_self_exclusion(explicit_task_scope, include_self, current_task_id)
    else {
        return Ok(Value::Object(resolved_args));
    };
    let mut exclude_task_ids = match resolved_args.get("exclude_task_ids") {
        Some(Value::Null) | None => Vec::new(),
        Some(value) => string_array_value(value, "exclude_task_ids")?,
    };
    if !exclude_task_ids.contains(&self_task_id) {
        exclude_task_ids.push(self_task_id);
    }
    resolved_args.insert(
        "exclude_task_ids".to_string(),
        Value::Array(exclude_task_ids.into_iter().map(Value::String).collect()),
    );
    Ok(Value::Object(resolved_args))
}

/// Resolve a request after applying the caller task's repository context.
/// `current_task` is the ordinary task-detail response fetched by the thin
/// adapter named by [`repo_context_task_id`].
pub fn resolve_request_with_repo_context(
    catalog: &Catalog,
    tool_name: &str,
    args: &Value,
    current_task: Option<&Value>,
) -> Result<ResolvedRequest, String> {
    let resolved_args = args_with_repo_context(args, current_task)?;
    resolve_request(catalog, tool_name, &resolved_args)
}

pub fn args_with_repo_context(args: &Value, current_task: Option<&Value>) -> Result<Value, String> {
    let mut resolved_args = args
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be a JSON object".to_string())?;
    if let Some(current_task) = current_task.filter(|_| !resolved_args.contains_key("repo_id")) {
        let repo_id = current_task
            .get("repoId")
            .or_else(|| current_task.get("repo_id"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "current task detail does not contain repo_id".to_string())?;
        resolved_args.insert("repo_id".to_string(), Value::String(repo_id.to_string()));
    }
    Ok(Value::Object(resolved_args))
}

fn value_for_param(
    tool: &ToolDef,
    param: &ParamDef,
    args: &Value,
) -> Result<Option<Value>, String> {
    let value = args
        .get(&param.name)
        .cloned()
        .or_else(|| param.default.clone());
    let Some(value) = value else {
        if param.required {
            return Err(format!("missing required argument: {}", param.name));
        }
        return Ok(None);
    };

    if let Some(enum_values) = &param.enum_values {
        // A closed vocabulary on a list constrains each element, not the list.
        // Validated here rather than only in the schema because the CLI reaches
        // `resolve_request` without a JSON-Schema validator in front of it.
        if param.param_type == ParamType::StringArray {
            for entry in string_array_value(&value, &param.name)? {
                if !enum_values.iter().any(|allowed| allowed == &entry) {
                    return Err(format!(
                        "{} entry {entry} must be one of {}",
                        param.name,
                        enum_values.join(", ")
                    ));
                }
            }
            return Ok(Some(Value::Array(
                string_array_value(&value, &param.name)?
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            )));
        }
        let rendered = string_value(&value, &param.name)?;
        if !enum_values.iter().any(|allowed| allowed == &rendered) {
            if tool.name == "kanna_complete_stage" && param.name == "status" {
                return Err("status must be success or failure".to_string());
            }
            if tool.response_kind == ResponseKind::Wait && param.name == "until" {
                return Err(format!(
                    "until must be reconcile, finished or closed, got {rendered}"
                ));
            }
            return Err(format!(
                "{} must be one of {}",
                param.name,
                enum_values.join(", ")
            ));
        }
    }

    let value = match param.param_type {
        ParamType::String => Value::String(string_value(&value, &param.name)?),
        ParamType::Integer => {
            Value::Number(integer_value(&value, &param.name, param.min, param.max)?.into())
        }
        ParamType::Boolean => Value::Bool(
            value
                .as_bool()
                .ok_or_else(|| format!("{} must be a boolean", param.name))?,
        ),
        ParamType::StringArray => Value::Array(
            string_array_value(&value, &param.name)?
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
        ParamType::Object => value,
    };
    Ok(Some(value))
}

fn reject_unknown_args(tool: &ToolDef, args: &Value) -> Result<(), String> {
    let Some(args_object) = args.as_object() else {
        return Ok(());
    };
    for key in args_object.keys() {
        if !tool.params.iter().any(|param| param.name == *key) {
            let accepted = tool
                .params
                .iter()
                .map(|param| param.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if accepted.is_empty() {
                return Err(format!(
                    "unknown argument: {key} ({} accepts no arguments)",
                    tool.name
                ));
            }
            return Err(format!(
                "unknown argument: {key} ({} accepts: {accepted})",
                tool.name
            ));
        }
    }
    Ok(())
}

fn string_value(value: &Value, name: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{name} must be a string"))
}

fn integer_value(
    value: &Value,
    name: &str,
    min: Option<u64>,
    max: Option<u64>,
) -> Result<u64, String> {
    let mut number = value
        .as_u64()
        .ok_or_else(|| format!("{name} must be an unsigned integer"))?;
    if let Some(min) = min {
        number = number.max(min);
    }
    if let Some(max) = max {
        number = number.min(max);
    }
    Ok(number)
}

fn string_array_value(value: &Value, name: &str) -> Result<Vec<String>, String> {
    let Some(values) = value.as_array() else {
        return Err(format!("{name} must be an array of strings"));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{name} must be an array of strings"))
        })
        .collect()
}

fn query_value(value: &Value, name: &str) -> Result<String, String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        // A list in a query string is comma-joined, so an agent passes the
        // array its schema declares rather than pre-joining ids itself.
        Value::Array(_) => Ok(string_array_value(value, name)?.join(",")),
        _ => Err(format!("{name} must be a scalar or string array")),
    }
}

fn wait_spec(tool: &ToolDef, args: &Value) -> Result<WaitSpec, String> {
    let mut task_id = None;
    let mut timeout_secs = DEFAULT_WAIT_TIMEOUT_SECS;
    let mut poll_secs = DEFAULT_WAIT_POLL_SECS;
    let mut until = WaitUntil::Reconcile;

    for param in &tool.params {
        let Some(value) = value_for_param(tool, param, args)? else {
            continue;
        };
        match param.name.as_str() {
            "task_id" => task_id = Some(string_value(&value, &param.name)?),
            "timeout_secs" => timeout_secs = integer_value(&value, &param.name, None, None)?,
            "poll_secs" => poll_secs = integer_value(&value, &param.name, None, None)?,
            "until" => {
                until = match string_value(&value, &param.name)?.as_str() {
                    "reconcile" => WaitUntil::Reconcile,
                    "finished" => WaitUntil::Finished,
                    "closed" => WaitUntil::Closed,
                    other => {
                        return Err(format!(
                            "until must be reconcile, finished or closed, got {other}"
                        ))
                    }
                };
            }
            _ => {}
        }
    }

    Ok(WaitSpec {
        task_id: task_id.ok_or_else(|| "missing required argument: task_id".to_string())?,
        timeout_secs: clamp_wait_timeout_secs(timeout_secs),
        poll_secs,
        until,
    })
}

/// A wait that reaches the requested state. Callers get the task detail they
/// already read, plus the discriminator that tells them not to loop again.
pub fn wait_resolved_result(task: Value) -> Value {
    let mut object = wait_result_object(task);
    object.insert(
        "waitOutcome".to_string(),
        Value::String("resolved".to_string()),
    );
    Value::Object(object)
}

/// A wait that runs out its window. This is a normal result, not an error: the
/// caller keeps the task's latest detail and the instruction to call again, and
/// both kanna-mcp and kanna-cli render it here so agents see one shape whichever
/// surface they use.
pub fn wait_timeout_result(task: Value, task_id: &str, timeout_secs: u64) -> Value {
    let mut object = wait_result_object(task);
    object.insert(
        "waitOutcome".to_string(),
        Value::String("timeout".to_string()),
    );
    object.insert(
        "waitTimeoutSecs".to_string(),
        Value::Number(timeout_secs.into()),
    );
    object.insert(
        "waitHint".to_string(),
        Value::String(format!(
            "task {task_id} has not reached the requested state within {timeout_secs}s. \
             This is not an error and the task is untouched — call kanna_wait_task again \
             with the same arguments to keep waiting."
        )),
    );
    Value::Object(object)
}

fn wait_result_object(task: Value) -> Map<String, Value> {
    match task {
        Value::Object(object) => object,
        other => {
            let mut wrapper = Map::new();
            wrapper.insert("task".to_string(), other);
            wrapper
        }
    }
}

pub fn encode_path_segment(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn run_finished_has_running_successor(event: &Value) -> bool {
    let payload = &event["payload"];
    let finished_run_id = payload.get("runId").and_then(Value::as_str);
    let latest_run = &payload["currentTask"]["latestRun"];
    latest_run.get("status").and_then(Value::as_str) == Some("running")
        && match (
            finished_run_id,
            latest_run.get("id").and_then(Value::as_str),
        ) {
            (Some(finished), Some(latest)) => finished != latest,
            // A running latest run is necessarily a successor even when an
            // older server omitted one of the ids from its enrichment.
            _ => true,
        }
}

pub fn is_actionable_task_event(event: &Value) -> bool {
    match event.get("type").and_then(Value::as_str) {
        Some("run.started" | "stage.changed" | "task.created" | "task.input_delivered") => false,
        // The read/unread display dimension. A person opening a task in the
        // desktop moves it, which is information for that person and never a
        // reason to wake the watcher; `task.runtime_changed` carries the
        // runtime edge underneath it.
        Some("task.activity_changed") => false,
        // Deprecated alias of the busy-to-non-busy subset of
        // `task.runtime_changed`, appended in the same transaction — so it is
        // always redundant with an event already in this batch.
        Some("task.runtime_settled") => false,
        Some("task.runtime_changed") => event["payload"]["runtimeState"] != "busy",
        Some("run.finished") => !run_finished_has_running_successor(event),
        _ => true,
    }
}

/// Default subscription relevance, before either local or aggregate batching.
/// Raw waits and the legacy CLI watch retain their general-purpose behavior.
/// Unknown facts remain visible, including on independently upgraded peers.
pub fn is_relevant_subscription_event(event: &Value) -> bool {
    let payload = &event["payload"];
    let current = &payload["currentTask"];
    let context = &payload["notificationContext"];
    let latest = context.get("latestRun").unwrap_or(&current["latestRun"]);
    match event.get("type").and_then(Value::as_str) {
        Some("run.finished") => {
            // A later stage never erases a failure. Only successful completion
            // is engine progress; missing verdict/policy stays observable.
            if payload["status"] == "failed" {
                return true;
            }
            if payload["status"] == "succeeded" {
                if context["completionTransition"] == "auto"
                    || payload["kind"] == "post"
                    || (payload.get("stage").is_some()
                        && current.get("stage").is_some()
                        && (payload["stage"] != current["stage"]
                            || (context["completionTransition"].is_null()
                                && current["stageTransition"] == "auto")))
                {
                    return false;
                }
                let successor = payload
                    .get("runId")
                    .and_then(Value::as_str)
                    .zip(current["latestRun"].get("id").and_then(Value::as_str))
                    .is_some_and(|(finished, latest)| finished != latest);
                return !successor && !run_finished_has_running_successor(event);
            }
            // Closing cancels its remaining runs; task.closed is the one
            // coordination fact, not a second completion per cancelled run.
            !(payload["status"] == "cancelled"
                && (context["closed"] == true || run_finished_has_running_successor(event)))
        }
        Some("task.runtime_changed") => {
            if payload["runtimeState"] == "busy" {
                return false;
            }
            if context["closed"] == true {
                return false;
            }
            if payload["currentState"] == true && context["providerParked"] == true {
                return true;
            }
            // Runtime edges are not questions; preserve the daemon's explicit
            // awaiting_input event. The initial settled scan must still find
            // a question whose edge predates registration.
            if payload["runtimeState"] == "waiting" {
                return payload["currentState"] == true;
            }
            if context["lifecyclePending"] == true {
                return false;
            }
            if context
                .get("runtimeState")
                .is_some_and(|state| state != &payload["runtimeState"])
            {
                return false;
            }
            if payload.get("stage").is_some()
                && current.get("stage").is_some()
                && payload["stage"] != current["stage"]
            {
                return false;
            }
            let transition = latest
                .get("completionTransition")
                .filter(|value| !value.is_null())
                .unwrap_or(&current["stageTransition"]);
            if payload["currentState"] == true
                && (latest["status"] == "failed"
                    || (latest["status"] == "succeeded"
                        && transition != "auto"
                        && latest["kind"] != "post"))
            {
                return true;
            }
            if payload["runtimeState"] == "idle" {
                // A manual main agent may park without recording a verdict.
                // An automatic main agent between turns is not manager work.
                return (latest["status"] == "running" || latest["status"].is_null())
                    && latest["kind"] != "post"
                    && transition != "auto";
            }
            // Termination with no recorded verdict is incomplete lifecycle.
            // A terminal run already supplied the durable completion signal.
            payload["runtimeState"] == "exited" && latest["status"] == "running"
        }
        Some("task.revision_requested") => {
            payload["exhausted"] != false || latest["status"] != "running"
        }
        Some("task.provider_quota_rejected") => {
            !payload["recovery"].as_str().is_some_and(|recovery| {
                recovery == "fallback-started" || recovery.starts_with("parked-")
            })
        }
        Some("task.transfer_finalizing") => payload["phase"] == "degraded",
        Some("task.raw_input_delivered") => false,
        // Dependency edges are meaningful even when their cause was automatic
        // PR/close progress. Closure and PR readiness also reconcile fan-out
        // and merge ownership; do not globally exclude those event types.
        Some(
            "task.blocked"
            | "task.unblocked"
            | "task.closed"
            | "task.pr_created"
            | "task.merge_signaled"
            | "task.merge_handoff_missing"
            | "task.awaiting_input"
            | "task.awaiting_advance"
            | "task.provider_quota_parked"
            | "task.teardown_failed"
            | "task.lifecycle_operation_retired",
        ) => true,
        _ => is_actionable_task_event(event),
    }
}

#[cfg(test)]
mod completion_context_tests {
    use super::{
        mutate_completion_context, read_completion_context, write_completion_context,
        CompletionContext,
    };
    use std::sync::{Arc, Barrier};

    #[test]
    fn old_context_without_spawn_identity_remains_readable_for_server_upgrade() {
        let context: CompletionContext = serde_json::from_str(r#"{"runId":"run-post"}"#).unwrap();
        assert_eq!(context.run_id, "run-post");
        assert_eq!(context.spawned_run_id, None);
        assert!(!context.legacy_writer);
        assert!(context.completed_attempts.is_empty());
        assert_eq!(context.completed_run_id, None);
    }

    #[test]
    fn concurrent_completion_record_and_post_rebind_cannot_overwrite_each_other() {
        let root = std::env::temp_dir().join(format!(
            "kanna-completion-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("task-1.json");
        write_completion_context(&path, &super::CompletionContext::new("run-main")).unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let record_path = path.clone();
        let record_barrier = Arc::clone(&barrier);
        let record = std::thread::spawn(move || {
            record_barrier.wait();
            mutate_completion_context(&record_path, |current| {
                let mut context = current.unwrap();
                context.record_completed_attempt("run-main", "attempt-main");
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok(context)
            })
            .unwrap();
        });
        let rebind_path = path.clone();
        let rebind = std::thread::spawn(move || {
            barrier.wait();
            mutate_completion_context(&rebind_path, |current| {
                let mut context = current.unwrap();
                context.run_id = "run-post".to_string();
                Ok(context)
            })
            .unwrap();
        });
        record.join().unwrap();
        rebind.join().unwrap();

        let context = read_completion_context(&path).unwrap();
        assert_eq!(context.run_id, "run-post");
        assert_eq!(context.run_for_attempt("attempt-main"), Some("run-main"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod subscription_relevance_tests {
    use super::*;

    #[test]
    fn relevance_is_structured_and_failure_survives_a_successor() {
        for kind in [
            "run.started",
            "stage.changed",
            "task.created",
            "task.activity_changed",
            "task.runtime_settled",
            "task.input_delivered",
            "task.raw_input_delivered",
        ] {
            assert!(
                !is_relevant_subscription_event(&serde_json::json!({"type":kind,"payload":{}})),
                "{kind}"
            );
        }
        for kind in [
            "task.awaiting_input",
            "task.awaiting_advance",
            "task.blocked",
            "task.unblocked",
            "task.closed",
            "task.pr_created",
            "task.merge_signaled",
            "task.merge_handoff_missing",
            "task.provider_quota_parked",
            "task.lifecycle_failed",
            "task.lifecycle_operation_retired",
            "task.teardown_failed",
            "future.observation_fault",
        ] {
            assert!(
                is_relevant_subscription_event(&serde_json::json!({"type":kind,"payload":{}})),
                "{kind}"
            );
        }
        let mut event = serde_json::json!({"type":"run.finished", "payload": {
            "runId":"old", "stage":"review", "kind":"main", "status":"failed",
            "currentTask":{"stage":"pr", "latestRun":{"id":"new", "status":"running"}},
            "notificationContext":{"completionTransition":"auto"}
        }});
        assert!(
            is_relevant_subscription_event(&event),
            "failure is a historical fact even after advance"
        );
        event["payload"]["status"] = serde_json::json!("succeeded");
        assert!(
            !is_relevant_subscription_event(&event),
            "automatic successful review is serviced by the engine"
        );
        event["payload"]["notificationContext"]["completionTransition"] =
            serde_json::json!("manual");
        event["payload"]["currentTask"] =
            serde_json::json!({"stage":"review", "latestRun":{"id":"old", "status":"succeeded"}});
        assert!(
            is_relevant_subscription_event(&event),
            "manual success needs advance"
        );
        event["payload"]["kind"] = serde_json::json!("post");
        assert!(
            !is_relevant_subscription_event(&event),
            "successful post transitions automatically"
        );
    }

    #[test]
    fn settled_runtime_distinguishes_manual_attention_from_automatic_progress() {
        let mut event = serde_json::json!({"type":"task.runtime_changed", "payload":{
            "stage":"review", "runtimeState":"idle", "currentState":true,
            "currentTask":{"stage":"review", "stageTransition":"auto"},
            "notificationContext":{"runtimeState":"idle", "latestRun":{
                "kind":"main", "status":"running", "completionTransition":"auto"
            }}
        }});
        assert!(
            !is_relevant_subscription_event(&event),
            "automatic idle alone is not work"
        );
        event["payload"]["notificationContext"]["latestRun"]["completionTransition"] =
            serde_json::json!("manual");
        assert!(
            is_relevant_subscription_event(&event),
            "manual agent without a verdict stays observable"
        );
        event["payload"]["notificationContext"]["latestRun"]["status"] =
            serde_json::json!("succeeded");
        assert!(
            is_relevant_subscription_event(&event),
            "initial scan finds a completed manual gate"
        );
        event["payload"]["currentState"] = serde_json::json!(false);
        assert!(
            !is_relevant_subscription_event(&event),
            "runtime echo adds nothing to recorded completion"
        );
        event["payload"]["currentState"] = serde_json::json!(true);
        event["payload"]["notificationContext"]["latestRun"]["status"] =
            serde_json::json!("running");
        event["payload"]["notificationContext"]["latestRun"]["completionTransition"] =
            serde_json::json!("auto");
        event["payload"]["notificationContext"]["providerParked"] = serde_json::json!(true);
        assert!(
            is_relevant_subscription_event(&event),
            "parked provider is attention even at an automatic stage"
        );
        event["payload"]["notificationContext"]["providerParked"] = serde_json::json!(false);
        event["payload"]["runtimeState"] = serde_json::json!("waiting");
        assert!(
            is_relevant_subscription_event(&event),
            "initial question scan survives"
        );
        event["payload"]["currentState"] = serde_json::json!(false);
        assert!(
            !is_relevant_subscription_event(&event),
            "durable waiting duplicates awaiting_input"
        );
    }

    #[test]
    fn revisions_and_provider_recovery_select_attention_not_routine_recovery() {
        let mut event = serde_json::json!({"type":"task.revision_requested", "payload":{
            "exhausted":false, "notificationContext":{"latestRun":{"status":"running"}}
        }});
        assert!(!is_relevant_subscription_event(&event));
        event["payload"]["exhausted"] = serde_json::json!(true);
        assert!(is_relevant_subscription_event(&event));
        event["payload"]["exhausted"] = serde_json::json!(false);
        event["payload"]["notificationContext"]["latestRun"]["status"] =
            serde_json::json!("failed");
        assert!(
            is_relevant_subscription_event(&event),
            "unresolved revision needs coordination"
        );
        assert!(!is_relevant_subscription_event(
            &serde_json::json!({"type":"task.provider_quota_rejected", "payload":{"recovery":"fallback-started"}})
        ));
        assert!(is_relevant_subscription_event(
            &serde_json::json!({"type":"task.provider_quota_parked", "payload":{"reason":"parked-no-candidates"}})
        ));
    }
}
