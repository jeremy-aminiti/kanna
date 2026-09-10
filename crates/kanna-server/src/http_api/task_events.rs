//! `GET /v1/task-events` — the multi-task, cursor-based wait.
//!
//! The wait happens here, not in the client: `kanna-mcp` and `kanna-cli` both
//! issue one plain GET and block on the response, so the cursor contract has a
//! single implementation and neither client grows a polling loop of its own.
//!
//! Contract:
//! - `cursor` omitted → the caller is starting fresh and gets the scope's
//!   retained history from the beginning, so events that fired before the first
//!   call are not lost.
//! - Events available → return them immediately. Fixed scopes return a numeric
//!   sequence; a parent scope returns the same global sequence bound to the
//!   parent id in a constant-size opaque cursor. `hasMore` says another batch
//!   is already waiting, so the caller loops without waiting.
//! - Nothing available → advance to the global head read before the query and
//!   block until an event arrives or the window elapses. Each recheck advances
//!   again, so unrelated retained history is never rescanned by a drained poll.
//! - Parent membership is evaluated at each read checkpoint. Reparenting never
//!   rewinds the global sequence: events at or below an acknowledged checkpoint
//!   do not become eligible later, while events after it are included whenever
//!   the child is in the parent scope at the next read.
//! - `excludeTaskIds` is a filter over whichever scope was chosen, never a
//!   scope: it drops those tasks' durable and synthetic rows but is not part
//!   of any cursor, so a watcher may change it between calls. It exists so a
//!   repo-scoped manager running inside a task session can keep its own
//!   runtime edges out of the feed that wakes it.

use super::lan_trust::AccountWideTaskEventAccess;
use super::state::{AppState, TunneledHttpInvoke};
use super::task_blockers::resolve_existing_task_id;
use crate::db::{Db, TaskEventFilters, TaskEventScope};
use axum::extract::{Extension, Query, State};
use axum::Json;
use base64::Engine;
use kanna_tool_catalog::encode_path_segment;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Upper bound on one response. A caller that asks for a whole repo's history
/// gets it in batches with `hasMore` set rather than one unbounded page.
const DEFAULT_EVENT_LIMIT: i64 = kanna_tool_catalog::DEFAULT_TASK_EVENT_LIMIT;
const MAX_EVENT_LIMIT: i64 = kanna_tool_catalog::MAX_TASK_EVENT_LIMIT;
/// Keeps invalid public GET requests and legacy cursor decoding bounded. New
/// parent cursors are normally under 100 bytes. A deployed p1 cursor may be up
/// to 32 KiB; its bounded upgrade continuation adds one checkpoint field, so
/// leave explicit headroom without allowing an unbounded public request.
const MAX_CURSOR_LEN: usize = 64 * 1024;
const MAX_DEPLOYED_PARENT_CURSOR_LEN: usize = 32 * 1024;
const MAX_LEGACY_PARENT_WATERMARKS: usize = 500;
const LEGACY_CURSOR_SCAN_LIMIT: i64 = 500;

/// Backstop re-check while blocked. In-process appends wake the waiter
/// immediately (`db::task_events::appended`); this bounds latency for the rare
/// writer that is not this process — the debug-only E2E SQL route, a test
/// harness writing the database directly.
const WAIT_RECHECK_SECS: u64 = 5;
/// The type a level-triggered `includeCurrentActivity` row is reported as. It
/// states the current runtime state, so it uses the same vocabulary as the
/// durable runtime event a manager keys on rather than a snapshot-only name.
const CURRENT_RUNTIME_SNAPSHOT_TYPE: &str = "task.runtime_changed";
const AGGREGATE_CURSOR_PREFIX: &str = "ks1.";
const CURRENT_ACTIVITY_CURSOR_PREFIX: &str = "kc1.";
const MACHINE_CURSOR_PREFIX: &str = "ke1.";
const SHORT_CURSOR_PREFIX: &str = "kh1.";
const AGGREGATE_WAIT_SESSION_TTL: Duration = Duration::from_secs(10 * 60);
const ZERO_TIMEOUT_DRAIN_BUDGET: Duration = Duration::from_millis(100);
const MAX_AGGREGATE_WAIT_SESSIONS: usize = 256;
const MAX_AGGREGATE_MACHINES: usize = 128;
const MAX_SHORT_CURSOR_HANDLES: usize = 4_096;
static SHORT_CURSOR_NONCE: AtomicU64 = AtomicU64::new(0);

/// Remote E2E deliberately ages the process-local tier quickly. The durable
/// SQLite checkpoint remains the production source of truth; this only proves
/// a live `kh1` resumes after the cache lifetime that used to lose it.
fn aggregate_wait_session_ttl() -> Duration {
    if cfg!(debug_assertions) && std::env::var("KANNA_E2E_SHORT_CURSOR_TTL_SECS").is_ok() {
        return Duration::from_secs(1);
    }
    AGGREGATE_WAIT_SESSION_TTL
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskEventsQuery {
    cursor: Option<String>,
    /// On a cursorless request, establish the durable checkpoint at the
    /// current event-log tail instead of replaying retained history.
    from: Option<String>,
    /// Comma-separated task ids or branch names. Omit to watch a whole repo.
    task_ids: Option<String>,
    /// Watch this task's direct children instead of naming their ids — the
    /// scope a fan-out can still express after losing the ids it created.
    parent_task_id: Option<String>,
    repo_id: Option<String>,
    /// Machine-independent repository identity. The public catalog exposes it
    /// for callers that already have the hash; aggregate repo-id waits resolve
    /// their local row to this value before contacting peers.
    repo_remote_url_hash: Option<String>,
    /// Comma-separated task ids or branch names whose events are dropped from
    /// the chosen scope. A watcher inside a task session passes its own id so
    /// a repo-wide watch does not wake it with its own settled-runtime edges.
    /// Unknown ids exclude nothing; they are never an error.
    exclude_task_ids: Option<String>,
    /// Comma-separated event type names dropped from the chosen scope. A
    /// manager passes `task.activity_changed` so a human reading a task's
    /// output does not wake it. Unknown names drop nothing.
    exclude_event_types: Option<String>,
    /// Comma-separated event type names the caller wants, to the exclusion of
    /// every other type — the complement of `exclude_event_types` for a
    /// manager that knows the short list it acts on. A filter, never part of
    /// the cursor, exactly like the two exclusion lists.
    event_types: Option<String>,
    timeout_secs: Option<u64>,
    limit: Option<i64>,
    /// Do not return before this many events (after filters) have accumulated,
    /// or the timeout elapses. Fewer, larger responses: the point of the
    /// parameter is that a manager polling in a loop pays one response for a
    /// stretch of feed rather than one per runtime flicker.
    min_events: Option<i64>,
    /// After the first matching event, keep collecting for this long — capped
    /// at the remaining timeout — so a burst (a run finishing, its post
    /// starting, a stage change, a PR created) comes back as one response
    /// instead of four.
    debounce_ms: Option<u64>,
    /// Floor on how long one call occupies before it returns events, measured
    /// from the start of the call. A caller polling in a loop therefore wakes
    /// at most once per interval however fast events arrive.
    min_interval_ms: Option<u64>,
    /// Drop the announcements of this caller's own deliveries —
    /// `task.input_delivered` and `task.raw_input_delivered` rows a manager
    /// declared itself the author of — so sending input and then waiting does
    /// not wake on the echo of the send.
    #[serde(default)]
    exclude_own: bool,
    /// Used by kanna-mcp's existing km1 fan-in so its native local sub-wait
    /// does not recursively start the server-side fan-in too.
    #[serde(default)]
    local_only: bool,
    /// Level-triggered manager wait: include synthetic current-state rows for
    /// tasks already stopped, so a restart cannot miss an earlier edge.
    #[serde(default = "include_current_state_by_default")]
    include_current_activity: bool,
    /// Agent-facing callers ask the server to replace the full native or
    /// aggregate checkpoint with a short, process-local handle. Direct HTTP
    /// clients that omit this keep the deployed stateless wire format.
    #[serde(default)]
    short_cursor: bool,
    /// Subscription notification selection. Optional on the wire so older
    /// peers can ignore it; the collecting server also selects their rows.
    #[serde(default)]
    orchestration_notifications: bool,
    /// Set only by the owning subscription call, never from peer wire input.
    #[serde(skip)]
    subscription_timing: bool,
}

fn include_current_state_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum AggregateScope {
    Tasks { task_ids: Vec<String> },
    Children { parent_task_id: String },
    Repo { remote_url_hash: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AggregateCursor {
    local_machine_id: String,
    scope: AggregateScope,
    machine_ids: Vec<String>,
    cursors_by_machine: BTreeMap<String, String>,
    #[serde(default)]
    machines_with_more: BTreeSet<String>,
}

struct AggregateWaitCompletion {
    machine_id: String,
    result: Result<Value, AggregateMachineWaitError>,
}

enum AggregateMachineWaitError {
    CursorRejected(String),
    Unavailable(String),
}

struct AggregateWaitSession {
    /// Forwarded verbatim to every machine leg; each leg resolves the task ids
    /// against its own database. Not part of the cursor, so a resumed session
    /// whose filters changed simply restarts its pending legs — one value, so
    /// that restart decision is one comparison.
    filters: TaskEventFilters,
    cursor: AggregateCursor,
    pending: tokio::task::JoinSet<AggregateWaitCompletion>,
    pending_machines: HashSet<String>,
    last_touched: tokio::time::Instant,
    include_current_activity: bool,
    from_now: bool,
    orchestration_notifications: bool,
}

struct ShortCursorEntry {
    cursor: String,
    last_touched: tokio::time::Instant,
}

fn starts_at_current_tail(
    query: &TaskEventsQuery,
) -> Result<bool, (axum::http::StatusCode, String)> {
    match query.from.as_deref() {
        None => Ok(false),
        Some("now") => Ok(true),
        Some(value) => Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!("from must be now, got {value}"),
        )),
    }
}

#[derive(Default)]
pub(super) struct AggregateWaitRegistry {
    sessions: HashMap<String, AggregateWaitSession>,
    short_cursors: HashMap<String, ShortCursorEntry>,
    reaper_started: bool,
}

impl AggregateWaitRegistry {
    fn abort_session(mut session: AggregateWaitSession) {
        session.pending.abort_all();
    }

    fn evict_expired(&mut self, now: tokio::time::Instant) {
        let expired = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                now.duration_since(session.last_touched) >= aggregate_wait_session_ttl()
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in expired {
            if let Some(session) = self.sessions.remove(&key) {
                Self::abort_session(session);
            }
        }
        self.short_cursors.retain(|_, entry| {
            now.duration_since(entry.last_touched) < aggregate_wait_session_ttl()
        });
    }

    fn resolve_short_cursor(&mut self, handle: &str) -> Option<String> {
        let now = tokio::time::Instant::now();
        self.evict_expired(now);
        let entry = self.short_cursors.get_mut(handle)?;
        entry.last_touched = now;
        Some(entry.cursor.clone())
    }

    fn issue_short_cursor(
        &mut self,
        issuer: &str,
        cursor: String,
        reuse_handle: Option<&str>,
    ) -> String {
        let now = tokio::time::Instant::now();
        self.evict_expired(now);
        // A cursor is a consumer checkpoint, not a response id. Updating the
        // handle supplied by the caller keeps one registry entry per watcher;
        // the old implementation minted an entry on every poll and allowed a
        // busy stream to evict its own still-active checkpoint at the cap.
        if let Some(handle) = reuse_handle {
            if let Some(entry) = self.short_cursors.get_mut(handle) {
                entry.cursor = cursor;
                entry.last_touched = now;
                return handle.to_string();
            }
            self.short_cursors.insert(
                handle.to_string(),
                ShortCursorEntry {
                    cursor,
                    last_touched: now,
                },
            );
            return handle.to_string();
        }
        while self.short_cursors.len() >= MAX_SHORT_CURSOR_HANDLES {
            let Some(oldest) = self
                .short_cursors
                .iter()
                .min_by_key(|(_, entry)| entry.last_touched)
                .map(|(handle, _)| handle.clone())
            else {
                break;
            };
            self.short_cursors.remove(&oldest);
        }
        loop {
            let handle = new_short_cursor_handle(issuer);
            if !self.short_cursors.contains_key(&handle) {
                self.short_cursors.insert(
                    handle.clone(),
                    ShortCursorEntry {
                        cursor,
                        last_touched: now,
                    },
                );
                return handle;
            }
        }
    }

    fn insert_bounded(&mut self, key: String, session: AggregateWaitSession) {
        if let Some(replaced) = self.sessions.remove(&key) {
            Self::abort_session(replaced);
        }
        while self.sessions.len() >= MAX_AGGREGATE_WAIT_SESSIONS {
            let Some(oldest_key) = self
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.last_touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = self.sessions.remove(&oldest_key) {
                Self::abort_session(evicted);
            }
        }
        self.sessions.insert(key, session);
    }
}

/// A short handle names the server that issued it. A handle is only
/// resolvable on that server — its process cache and its `task_event_cursor_handle`
/// rows are local — so a handle replayed against another machine can never
/// succeed, and reporting that as "expired" sent an operator hunting for a
/// retention bug instead of a misrouted wait. Derived from the desktop id with
/// SHA-256 rather than `DefaultHasher` so a handle minted by an older build
/// still identifies its issuer after a toolchain upgrade.
fn issuer_token(desktop_id: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(desktop_id.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

const SHORT_CURSOR_ISSUER_LEN: usize = 8;
const SHORT_CURSOR_NONCE_LEN: usize = 8;

/// `kh1.<issuer>.<nonce>`. Handles minted before the issuer field carry only
/// the nonce; they are attributed to this server, which is where they were
/// resolvable before the field existed.
fn short_cursor_issuer(handle: &str) -> Option<&str> {
    let body = handle.strip_prefix(SHORT_CURSOR_PREFIX)?;
    let (issuer, nonce) = body.split_once('.')?;
    (issuer.len() == SHORT_CURSOR_ISSUER_LEN
        && nonce.len() == SHORT_CURSOR_NONCE_LEN
        && issuer.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(issuer)
}

fn new_short_cursor_handle(issuer: &str) -> String {
    let nonce = SHORT_CURSOR_NONCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    issuer.hash(&mut hasher);
    nonce.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    format!(
        "{SHORT_CURSOR_PREFIX}{issuer}.{:08x}",
        hasher.finish() as u32
    )
}

fn foreign_short_cursor(
    handle: &str,
    issuer: &str,
    state: &Arc<AppState>,
) -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        format!(
            "task-event cursor handle {handle} was issued by another machine ({issuer}); \
             this server is {} ({}). A short cursor handle is only resolvable on the \
             server that issued it, so retrying it here can never succeed: route the wait \
             to the issuing machine, or retry without a cursor to start a wait this \
             server owns.",
            state.config().desktop_id,
            issuer_token(&state.config().desktop_id),
        ),
    )
}

/// What a supplied cursor resolved to.
enum ShortCursorResolution {
    /// Not a short handle, or a handle this server could resolve. Carries the
    /// native cursor to wait on.
    Cursor(Option<String>),
    /// This server issued the handle but no longer holds its checkpoint. The
    /// wait restarts from retained history — the recovery the old 400 asked
    /// the caller to perform — so a caller that re-arms mechanically makes
    /// progress instead of spinning on a request that can only ever fail.
    Reset(String),
}

fn resolve_short_cursor(
    state: &Arc<AppState>,
    cursor: Option<&str>,
) -> Result<ShortCursorResolution, (axum::http::StatusCode, String)> {
    let Some(handle) = cursor.filter(|cursor| cursor.starts_with(SHORT_CURSOR_PREFIX)) else {
        return Ok(ShortCursorResolution::Cursor(cursor.map(str::to_string)));
    };
    if let Some(cursor) = state
        .aggregate_task_event_waits
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .resolve_short_cursor(handle)
    {
        return Ok(ShortCursorResolution::Cursor(Some(cursor)));
    }
    if let Some(cursor) = Db::open(&state.config().db_path)
        .and_then(|db| db.resolve_task_event_cursor_handle(handle))
        .map_err(db_error)?
    {
        return Ok(ShortCursorResolution::Cursor(Some(cursor)));
    }
    let local_issuer = issuer_token(&state.config().desktop_id);
    if let Some(issuer) = short_cursor_issuer(handle) {
        if issuer != local_issuer {
            return Err(foreign_short_cursor(handle, issuer, state));
        }
    }
    Ok(ShortCursorResolution::Reset(format!(
        "task-event cursor handle {handle} is no longer held by this server, so this wait \
         restarted from retained history and returns a new handle"
    )))
}

fn shorten_response_cursor(
    state: &Arc<AppState>,
    reuse_handle: Option<&str>,
    Json(mut response): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let cursor = response
        .get("cursor")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "task-event response did not contain a cursor".to_string(),
            )
        })?
        .to_string();
    let issuer = issuer_token(&state.config().desktop_id);
    // The durable row is the checkpoint of record.  In particular, do not
    // advance the cache first: a SQLite failure after forming this response
    // must leave a retry of the same handle at its previous position.
    let handle = reuse_handle
        .map(str::to_owned)
        .unwrap_or_else(|| new_short_cursor_handle(&issuer));
    Db::open(&state.config().db_path)
        .and_then(|db| db.store_task_event_cursor_handle(&handle, &cursor))
        .map_err(db_error)?;
    state
        .aggregate_task_event_waits
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .issue_short_cursor(&issuer, cursor.clone(), Some(&handle));
    response["cursor"] = Value::String(handle);
    Ok(Json(response))
}

#[cfg(test)]
pub(super) fn issue_expired_short_cursor_for_test(state: &Arc<AppState>) -> String {
    let issuer = issuer_token(&state.config().desktop_id);
    let mut registry = state
        .aggregate_task_event_waits
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let handle = registry.issue_short_cursor(&issuer, "0".to_string(), None);
    if let Some(entry) = registry.short_cursors.get_mut(&handle) {
        entry.last_touched = tokio::time::Instant::now() - AGGREGATE_WAIT_SESSION_TTL;
    }
    // The production reaper removes durable rows only with the event
    // retention window. Tests need a handle that is absent from both tiers.
    let _ = Db::open(&state.config().db_path)
        .and_then(|db| db.delete_task_event_cursor_handle_for_test(&handle));
    handle
}

fn ensure_aggregate_wait_reaper(state: &Arc<AppState>) {
    start_aggregate_wait_reaper(Arc::clone(&state.aggregate_task_event_waits));
}

fn start_aggregate_wait_reaper(registry: Arc<std::sync::Mutex<AggregateWaitRegistry>>) {
    {
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if registry.reaper_started {
            return;
        }
        registry.reaper_started = true;
    }

    let registry = Arc::downgrade(&registry);
    tokio::spawn(async move {
        loop {
            let Some(registry) = registry.upgrade() else {
                break;
            };
            let sleep_for = {
                let mut registry = registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let now = tokio::time::Instant::now();
                registry.evict_expired(now);
                registry
                    .sessions
                    .values()
                    .map(|session| {
                        AGGREGATE_WAIT_SESSION_TTL
                            .saturating_sub(now.duration_since(session.last_touched))
                    })
                    .min()
                    .unwrap_or(AGGREGATE_WAIT_SESSION_TTL)
            };
            drop(registry);
            tokio::time::sleep(sleep_for).await;
        }
    });
}

#[derive(Debug, Clone)]
enum TaskEventsCursor {
    Sequence(i64),
    ParentV1(ParentTaskEventsCursorV1),
    ParentV3(ParentTaskEventsCursorV3),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CurrentActivityCursor {
    durable_cursor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settled_after_task_id: Option<String>,
    #[serde(default)]
    settled_complete: bool,
}

#[derive(Debug, Clone, Default)]
struct CurrentActivityProgress {
    settled_after_task_id: Option<String>,
    settled_complete: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ParentTaskEventsCursorV1 {
    parent_task_id: String,
    watermarks: BTreeMap<String, i64>,
    /// Global progress made while draining this legacy map. Deployed p1
    /// cursors omit it. Once present, entries at or below this boundary can be
    /// discarded safely, including stale children that later reattach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event_seq: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ParentTaskEventsCursorV3 {
    parent_task_id: String,
    event_seq: i64,
}

const PARENT_CURSOR_V1_PREFIX: &str = "p1.";
const PARENT_CURSOR_V3_PREFIX: &str = "p3.";

fn invalid_cursor() -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        "cursor is not a valid cursor returned by this endpoint; restart without a cursor to safely replay retained history"
            .to_string(),
    )
}

fn parse_cursor(
    cursor: Option<&str>,
) -> Result<Option<TaskEventsCursor>, (axum::http::StatusCode, String)> {
    let Some(cursor) = cursor.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if cursor.len() > MAX_CURSOR_LEN {
        return Err(invalid_cursor());
    }
    if let Some(encoded) = cursor.strip_prefix(PARENT_CURSOR_V3_PREFIX) {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid_cursor())?;
        let parsed: ParentTaskEventsCursorV3 =
            serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
        if parsed.event_seq < 0 {
            return Err(invalid_cursor());
        }
        return Ok(Some(TaskEventsCursor::ParentV3(parsed)));
    }
    if let Some(encoded) = cursor.strip_prefix(PARENT_CURSOR_V1_PREFIX) {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| invalid_cursor())?;
        let parsed: ParentTaskEventsCursorV1 =
            serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
        if (parsed.event_seq.is_none() && cursor.len() > MAX_DEPLOYED_PARENT_CURSOR_LEN)
            || parsed.watermarks.len() > MAX_LEGACY_PARENT_WATERMARKS
            || parsed.event_seq.is_some_and(|seq| seq < 0)
            || parsed.watermarks.values().any(|seq| *seq < 0)
        {
            return Err(invalid_cursor());
        }
        return Ok(Some(TaskEventsCursor::ParentV1(parsed)));
    }
    let sequence = cursor.parse::<i64>().map_err(|_| invalid_cursor())?;
    if sequence < 0 {
        return Err(invalid_cursor());
    }
    Ok(Some(TaskEventsCursor::Sequence(sequence)))
}

fn decode_current_activity_cursor(
    cursor: &str,
) -> Result<Option<CurrentActivityCursor>, (axum::http::StatusCode, String)> {
    if cursor.len() > MAX_CURSOR_LEN {
        return Err(invalid_cursor());
    }
    let Some(encoded) = cursor.strip_prefix(CURRENT_ACTIVITY_CURSOR_PREFIX) else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_cursor())?;
    let parsed: CurrentActivityCursor =
        serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    if parsed.durable_cursor.is_empty()
        || parsed
            .durable_cursor
            .starts_with(CURRENT_ACTIVITY_CURSOR_PREFIX)
        || parse_cursor(Some(&parsed.durable_cursor))?.is_none()
    {
        return Err(invalid_cursor());
    }
    Ok(Some(parsed))
}

fn encode_current_activity_cursor(
    cursor: &CurrentActivityCursor,
) -> Result<String, (axum::http::StatusCode, String)> {
    let bytes = serde_json::to_vec(cursor).map_err(|error| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to encode current-activity cursor: {error}"),
        )
    })?;
    let encoded = format!(
        "{CURRENT_ACTIVITY_CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    if encoded.len() > MAX_CURSOR_LEN {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode a bounded current-activity cursor".to_string(),
        ));
    }
    Ok(encoded)
}

fn encode_parent_cursor<T: Serialize>(
    prefix: &str,
    cursor: &T,
) -> Result<String, (axum::http::StatusCode, String)> {
    let bytes = serde_json::to_vec(cursor).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to encode task event cursor: {e}"),
        )
    })?;
    let encoded = format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    if encoded.len() > MAX_CURSOR_LEN {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode a bounded task event cursor".to_string(),
        ));
    }
    Ok(encoded)
}

fn resolve_scope(
    db: &Db,
    query: &TaskEventsQuery,
    tolerate_missing_tasks: bool,
) -> Result<TaskEventScope, (axum::http::StatusCode, String)> {
    let task_ids: Vec<String> = query
        .task_ids
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if !task_ids.is_empty() {
        // Branch names are accepted everywhere else a task id is; a watcher
        // must not silently observe nothing because it passed one here.
        let resolved = if tolerate_missing_tasks {
            let mut resolved = Vec::new();
            for task_id in &task_ids {
                if let Some(task_id) = db.resolve_pipeline_item_id(task_id).map_err(db_error)? {
                    resolved.push(task_id);
                }
            }
            resolved
        } else {
            task_ids
                .iter()
                .map(|task_id| resolve_existing_task_id(db, task_id))
                .collect::<Result<Vec<_>, _>>()?
        };
        return Ok(TaskEventScope::Tasks(resolved));
    }

    // Narrower than a repo and, unlike an id list, still nameable by a parent
    // that no longer remembers what it created. Resolved to an id here and
    // matched by `parent_task_id` at query time, so children created after this
    // call started are picked up by the very next read.
    if let Some(parent_task_id) = query
        .parent_task_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let resolved = if tolerate_missing_tasks {
            db.resolve_pipeline_item_id(parent_task_id)
                .map_err(db_error)?
                .unwrap_or_else(|| parent_task_id.to_string())
        } else {
            resolve_existing_task_id(db, parent_task_id)?
        };
        return Ok(TaskEventScope::Children(resolved));
    }

    if let Some(repo_id) = query
        .repo_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(TaskEventScope::Repo(repo_id.to_string()));
    }

    if let Some(remote_url_hash) = query
        .repo_remote_url_hash
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(TaskEventScope::RepoRemoteUrlHash(
            remote_url_hash.to_string(),
        ));
    }

    Err((
        axum::http::StatusCode::BAD_REQUEST,
        "task_ids, parent_task_id, repo_id or repo_remote_url_hash is required: an unscoped event feed \
         would hand an orchestrator every other task's events too"
            .to_string(),
    ))
}

/// Resolve `excludeTaskIds` to task ids and pair them with `excludeEventTypes`
/// and the `eventTypes` allow-list. Branch names are accepted like every other
/// task reference; a value that resolves to nothing is kept verbatim so
/// excluding a task that no longer exists is a harmless no-op rather than a
/// failed watch. Event type names are taken literally for the same reason — an
/// unrecognized exclusion drops nothing, and an unrecognized allow-list entry
/// simply matches nothing.
fn resolve_exclusions(
    db: &Db,
    query: &TaskEventsQuery,
) -> Result<TaskEventFilters, (axum::http::StatusCode, String)> {
    let mut exclude_task_ids = Vec::new();
    for raw in normalized_values(query.exclude_task_ids.as_deref()) {
        let task_id = db
            .resolve_pipeline_item_id(&raw)
            .map_err(db_error)?
            .unwrap_or(raw);
        if !exclude_task_ids.contains(&task_id) {
            exclude_task_ids.push(task_id);
        }
    }
    Ok(TaskEventFilters {
        exclude_task_ids,
        exclude_event_types: normalized_values(query.exclude_event_types.as_deref()),
        include_event_types: normalized_values(query.event_types.as_deref()),
        exclude_own_deliveries: query.exclude_own,
    })
}

struct EventBatch {
    events: Vec<Value>,
    cursor: String,
    has_more: bool,
}

fn db_error(error: rusqlite::Error) -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        format!("db error: {error}"),
    )
}

fn encode_v3_cursor(
    parent_task_id: &str,
    event_seq: i64,
) -> Result<String, (axum::http::StatusCode, String)> {
    encode_parent_cursor(
        PARENT_CURSOR_V3_PREFIX,
        &ParentTaskEventsCursorV3 {
            parent_task_id: parent_task_id.to_string(),
            event_seq,
        },
    )
}

fn cursor_after_truncated_parent_batch(
    parent_task_id: &str,
    previous_cursor: Option<&str>,
    event_seq: i64,
) -> Result<String, (axum::http::StatusCode, String)> {
    let Some(TaskEventsCursor::ParentV1(legacy)) = parse_cursor(previous_cursor)? else {
        return encode_v3_cursor(parent_task_id, event_seq);
    };
    if legacy.parent_task_id != parent_task_id {
        return Err((
            axum::http::StatusCode::BAD_GATEWAY,
            "peer returned events for a parent cursor with a different scope".to_string(),
        ));
    }

    // The aggregate response emitted only a prefix of the peer's batch, so it
    // cannot adopt the peer's returned cursor. Preserve acknowledgements above
    // that prefix from a deployed p1 cursor; collapsing directly to p3 at the
    // last emitted sequence would replay those already-acknowledged events.
    let mut watermarks = legacy.watermarks;
    watermarks.retain(|_, watermark| *watermark > event_seq);
    if watermarks.is_empty() {
        encode_v3_cursor(parent_task_id, event_seq)
    } else {
        encode_parent_cursor(
            PARENT_CURSOR_V1_PREFIX,
            &ParentTaskEventsCursorV1 {
                parent_task_id: parent_task_id.to_string(),
                watermarks,
                event_seq: Some(event_seq),
            },
        )
    }
}

fn durable_cursor_from_wire(cursor: &str) -> Result<String, (axum::http::StatusCode, String)> {
    Ok(decode_current_activity_cursor(cursor)?
        .map(|current| current.durable_cursor)
        .unwrap_or_else(|| cursor.to_string()))
}

/// Drain one bounded slice of a deployed p1 cursor. This path exists only for
/// forward compatibility: stale map entries cannot widen the current-parent
/// SQL scope, scans use the parent and per-task indexes, and the cursor
/// upgrades to constant-size p3 as soon as every legacy acknowledgement is
/// behind its global checkpoint.
fn read_legacy_parent_batch_with_hook(
    db: &Db,
    parent_task_id: &str,
    filters: &TaskEventFilters,
    legacy: &ParentTaskEventsCursorV1,
    limit: i64,
    after_membership_read: impl FnOnce(),
) -> Result<EventBatch, (axum::http::StatusCode, String)> {
    if legacy.parent_task_id != parent_task_id {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "cursor belongs to a different parent_task_id scope".to_string(),
        ));
    }

    let (head_seq, members, mut candidates, cursor_is_valid) = db
        .with_read_transaction(|db| {
            // The head read establishes the WAL snapshot before membership is
            // observed. Validate cursor state against that same snapshot before
            // doing membership or candidate work; a forged future cursor must
            // not trigger a potentially expensive compatibility scan.
            let head_seq = db.latest_task_event_seq()?;
            if legacy
                .watermarks
                .values()
                .any(|watermark| *watermark > head_seq)
                || legacy.event_seq.is_some_and(|seq| seq > head_seq)
            {
                return Ok((head_seq, Vec::new(), Vec::new(), false));
            }
            let mut members = db.list_child_task_ids(parent_task_id)?;
            members.retain(|member| !filters.exclude_task_ids.contains(member));
            after_membership_read();
            let after_seq = legacy.event_seq.unwrap_or_else(|| {
                members
                    .iter()
                    .map(|task_id| legacy.watermarks.get(task_id).copied().unwrap_or(0))
                    .min()
                    .unwrap_or(0)
            });
            let candidates = db.list_task_event_candidates_bounded(
                &members,
                filters,
                after_seq,
                head_seq,
                LEGACY_CURSOR_SCAN_LIMIT + 1,
            )?;
            Ok((head_seq, members, candidates, true))
        })
        .map_err(db_error)?;
    if !cursor_is_valid {
        return Err(invalid_cursor());
    }

    // A p1 map captured membership at issuance. Entries for children since
    // reparented away are legitimate but irrelevant to the current scope;
    // conversely, a current child absent from the map was adopted later and
    // retains p1's zero-watermark delivery semantics for the first upgrade
    // snapshot. Continuations carry a global checkpoint and never grow the
    // deployed map, so a valid 500-entry p1 always returns a valid cursor.
    let after_seq = legacy.event_seq.unwrap_or_else(|| {
        members
            .iter()
            .map(|task_id| legacy.watermarks.get(task_id).copied().unwrap_or(0))
            .min()
            .unwrap_or(0)
    });
    // Include stale entries in the safe compaction ceiling. Dropping a higher
    // acknowledgement merely because that child is currently away would make
    // p3 rewind and replay it if the child later reattaches.
    let ceiling = legacy
        .watermarks
        .values()
        .copied()
        .max()
        .unwrap_or(after_seq)
        .max(after_seq);
    let scan_has_more = candidates.len() as i64 > LEGACY_CURSOR_SCAN_LIMIT;
    candidates.truncate(LEGACY_CURSOR_SCAN_LIMIT as usize);

    let mut delivered = Vec::new();
    let mut processed_seq = after_seq;
    let mut stopped_at_page_limit = false;
    for event in candidates {
        let watermark = legacy
            .watermarks
            .get(&event.task_id)
            .copied()
            .unwrap_or(0)
            .max(after_seq);
        if event.seq > watermark {
            if delivered.len() as i64 == limit {
                stopped_at_page_limit = true;
                break;
            }
            delivered.push(event.clone());
        }
        processed_seq = event.seq;
    }

    let fully_scanned = !scan_has_more && !stopped_at_page_limit;
    if fully_scanned {
        processed_seq = head_seq;
    }
    let can_upgrade = processed_seq >= ceiling;
    let has_more = !fully_scanned;
    let cursor = if can_upgrade {
        encode_v3_cursor(parent_task_id, processed_seq)?
    } else {
        let mut watermarks = legacy.watermarks.clone();
        // `event_seq` now protects every acknowledgement through this global
        // boundary. Removing covered entries keeps the legacy state no larger
        // than the accepted input, while entries above it preserve the safe
        // boundary for temporarily reparented-away children.
        watermarks.retain(|_, watermark| *watermark > processed_seq);
        encode_parent_cursor(
            PARENT_CURSOR_V1_PREFIX,
            &ParentTaskEventsCursorV1 {
                parent_task_id: parent_task_id.to_string(),
                watermarks,
                event_seq: Some(processed_seq),
            },
        )?
    };

    Ok(EventBatch {
        events: delivered.iter().map(|event| event.to_json()).collect(),
        cursor,
        has_more,
    })
}

fn read_legacy_parent_batch(
    db: &Db,
    parent_task_id: &str,
    filters: &TaskEventFilters,
    legacy: &ParentTaskEventsCursorV1,
    limit: i64,
) -> Result<EventBatch, (axum::http::StatusCode, String)> {
    read_legacy_parent_batch_with_hook(db, parent_task_id, filters, legacy, limit, || {})
}

#[cfg(test)]
pub(super) fn read_legacy_parent_batch_for_test(
    db: &Db,
    parent_task_id: &str,
    cursor: &str,
    limit: i64,
    after_membership_read: impl FnOnce(),
) -> Result<Value, String> {
    let parsed = parse_cursor(Some(cursor)).map_err(|(_, error)| error)?;
    let TaskEventsCursor::ParentV1(legacy) = parsed.ok_or_else(|| "missing cursor".to_string())?
    else {
        return Err("expected p1 cursor".to_string());
    };
    let batch = read_legacy_parent_batch_with_hook(
        db,
        parent_task_id,
        &TaskEventFilters::default(),
        &legacy,
        limit,
        after_membership_read,
    )
    .map_err(|(_, error)| error)?;
    Ok(json!({
        "cursor": batch.cursor,
        "events": batch.events,
        "hasMore": batch.has_more,
    }))
}

fn read_sequence_batch(
    db: &Db,
    scope: &TaskEventScope,
    filters: &TaskEventFilters,
    after_seq: i64,
    limit: i64,
    parent_task_id: Option<&str>,
) -> Result<EventBatch, (axum::http::StatusCode, String)> {
    // Read the head first: the returned cursor must never be ahead of the rows
    // the same call looked at.
    let (head_seq, mut events) = db
        .with_read_transaction(|db| {
            let head_seq = db.latest_task_event_seq()?;
            let events =
                db.list_task_events_excluding(scope, filters, after_seq, head_seq, limit + 1)?;
            Ok((head_seq, events))
        })
        .map_err(db_error)?;
    if after_seq > head_seq {
        return Err(invalid_cursor());
    }
    // The lower bound stays a plain range constraint on task_event's INTEGER
    // PRIMARY KEY. Scope filtering happens after that indexable cut, so a
    // drained long poll performs work proportional to new rows, not history.
    let has_more = events.len() as i64 > limit;
    events.truncate(limit as usize);
    let cursor_seq = if has_more {
        events.last().map(|event| event.seq).unwrap_or(after_seq)
    } else {
        // The query proved there are no more matching rows through this head;
        // skipping unrelated rows prevents every recheck from scanning them.
        head_seq
    };
    let cursor = if let Some(parent_task_id) = parent_task_id {
        encode_v3_cursor(parent_task_id, cursor_seq)?
    } else {
        cursor_seq.to_string()
    };
    Ok(EventBatch {
        events: events.iter().map(|event| event.to_json()).collect(),
        cursor,
        has_more,
    })
}

fn read_batch(
    db_path: &str,
    scope: &TaskEventScope,
    filters: &TaskEventFilters,
    cursor: Option<&TaskEventsCursor>,
    limit: i64,
) -> Result<EventBatch, (axum::http::StatusCode, String)> {
    let db = Db::open(db_path).map_err(db_error)?;
    if let TaskEventScope::Children(parent_task_id) = scope {
        return match cursor {
            Some(TaskEventsCursor::ParentV1(legacy)) => {
                read_legacy_parent_batch(&db, parent_task_id, filters, legacy, limit)
            }
            Some(TaskEventsCursor::ParentV3(parent_cursor)) => {
                if parent_cursor.parent_task_id != *parent_task_id {
                    return Err((
                        axum::http::StatusCode::BAD_REQUEST,
                        "cursor belongs to a different parent_task_id scope".to_string(),
                    ));
                }
                read_sequence_batch(
                    &db,
                    scope,
                    filters,
                    parent_cursor.event_seq,
                    limit,
                    Some(parent_task_id),
                )
            }
            // Numeric parent cursors were returned by the first deployed
            // implementation. Their global watermark maps exactly to p3.
            Some(TaskEventsCursor::Sequence(seq)) => {
                read_sequence_batch(&db, scope, filters, *seq, limit, Some(parent_task_id))
            }
            None => read_sequence_batch(&db, scope, filters, 0, limit, Some(parent_task_id)),
        };
    }

    let after_seq = match cursor {
        Some(TaskEventsCursor::Sequence(seq)) => *seq,
        Some(TaskEventsCursor::ParentV1(_) | TaskEventsCursor::ParentV3(_)) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "parent-scoped cursor cannot be used with a fixed task or repo scope".to_string(),
            ));
        }
        None => 0,
    };
    read_sequence_batch(&db, scope, filters, after_seq, limit, None)
}

const EVENT_SUMMARY_SNIPPET_CHARS: usize = 280;

fn summary_snippet(summary: &str) -> String {
    let mut chars = summary.chars();
    let snippet = chars
        .by_ref()
        .take(EVENT_SUMMARY_SNIPPET_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}

/// Preserve the lifecycle fact and add delivery-time context under a distinct
/// key. Event-time fields must never be overwritten by today's task state: a
/// retained `run.finished` from `in progress` is still that run after the task
/// reaches `pr`.
fn enrich_event_batch(
    config: &crate::config::Config,
    batch: &mut EventBatch,
    orchestration_notifications: bool,
) -> Result<(), String> {
    let event_db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
    let current_db = Db::open(&config.db_path).map_err(|error| format!("db error: {error}"))?;
    let api = crate::mobile_api::MobileApi::new(config.clone(), current_db);
    for event in &mut batch.events {
        let Some(event_object) = event.as_object_mut() else {
            continue;
        };
        event_object.insert(
            "machineId".to_string(),
            Value::String(config.desktop_id.clone()),
        );
        let Some(task_id) = event_object
            .get("taskId")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(task) = api.get_task(&task_id)? else {
            continue;
        };
        let event_seq = event_object.get("seq").and_then(Value::as_i64);
        let include_latest_run = matches!(
            event_object.get("type").and_then(Value::as_str),
            Some(
                "run.finished"
                    | "task.awaiting_input"
                    | "task.awaiting_advance"
                    | "task.runtime_changed"
            )
        );
        let payload = event_object.entry("payload").or_insert_with(|| json!({}));
        let Some(payload) = payload.as_object_mut() else {
            continue;
        };
        if payload.get("currentState").and_then(Value::as_bool) == Some(true) {
            payload.insert("stage".into(), json!(task.stage));
            payload.insert(
                "latestRunFinishedWithoutCompletion".into(),
                json!(
                    matches!(task.runtime_state.as_deref(), Some("idle" | "exited"))
                        && task
                            .latest_run
                            .as_ref()
                            .is_some_and(|run| run.status == "running")
                ),
            );
            payload.insert(
                "reconciliationReason".into(),
                json!(match task.runtime_state.as_deref() {
                    Some("idle")
                        if task
                            .latest_run
                            .as_ref()
                            .is_some_and(|run| run.status == "running") =>
                        "idle_without_verdict",
                    Some("waiting") => "awaiting_input",
                    Some("exited") => "session_exited",
                    _ => "settled",
                }),
            );
        }
        if !payload.contains_key("stage") {
            let event_stage = event_seq
                .map(|seq| event_db.task_event_stage_at(&task_id, seq))
                .transpose()
                .map_err(|error| format!("db error: {error}"))?
                .flatten();
            if let Some(event_stage) = event_stage {
                payload.insert("stage".to_string(), Value::String(event_stage));
            }
        }
        payload.insert(
            "machineId".to_string(),
            Value::String(config.desktop_id.clone()),
        );
        if orchestration_notifications {
            let run = payload
                .get("runId")
                .and_then(Value::as_str)
                .map(|id| event_db.stage_run(id))
                .transpose()
                .map_err(|error| format!("db error: {error}"))?
                .flatten();
            let latest = event_db
                .latest_stage_run(&task_id)
                .map_err(|error| format!("db error: {error}"))?;
            payload.insert("notificationContext".into(), json!({
                "completionTransition": run.as_ref().and_then(|run| run.completion_transition.as_deref()),
                "mainCompletionHasContinuation": run.as_ref()
                    .filter(|run| run.status == "succeeded" && run.kind == "main").map(|run|
                    crate::task_creator::main_completion_continuation(&event_db, &task_id, &run.stage)
                ).and_then(Result::ok).flatten(),
                "closed": task.closed_at.is_some(),
                "runtimeState": task.runtime_state,
                "providerParked": task.provider_rejection.as_ref()
                    .is_some_and(|rejection| rejection.recovery.starts_with("parked-")),
                "lifecyclePending": event_db.has_lifecycle_operation_for_task(&task_id)
                    .map_err(|error| format!("db error: {error}"))?,
                "latestRun": latest.as_ref().map(|run| json!({
                    "id": run.id, "kind": run.kind, "status": run.status,
                    "completionTransition": run.completion_transition,
                    "mainCompletionHasContinuation": if run.status == "succeeded" && run.kind == "main" {
                        crate::task_creator::main_completion_continuation(
                            &event_db, &task_id, &run.stage).ok().flatten()
                    } else { None },
                })),
            }));
        }
        let latest_run = if include_latest_run {
            task.latest_run.map_or(Value::Null, |run| {
                json!({
                    "id": run.id,
                    "status": run.status,
                    "kind": run.kind,
                    "stage": run.stage,
                    "summarySnippet": run.summary.as_deref().map(summary_snippet),
                })
            })
        } else {
            Value::Null
        };
        payload.insert(
            "currentTask".to_string(),
            json!({
                "title": task.title,
                "stage": task.stage,
                "activity": task.activity,
                "stageTransition": task.stage_transition,
                "latestRun": latest_run,
            }),
        );
    }
    Ok(())
}

fn append_current_activity_snapshots(
    db_path: &str,
    scope: &TaskEventScope,
    filters: &TaskEventFilters,
    batch: &mut EventBatch,
    limit: i64,
    progress: &mut CurrentActivityProgress,
) -> Result<(), rusqlite::Error> {
    let remaining = (limit as usize).saturating_sub(batch.events.len());
    if remaining == 0 || progress.settled_complete {
        return Ok(());
    }
    // A synthetic row states the same fact as the durable event it is named
    // after, so a caller that filtered that type out — by excluding it, or by
    // naming an allow-list without it — does not want it here either.
    if !filters.allows_event_type(CURRENT_RUNTIME_SNAPSHOT_TYPE) {
        progress.settled_complete = true;
        return Ok(());
    }
    let db = Db::open(db_path)?;
    let mut rows = db.list_non_busy_task_runtime_states(
        scope,
        filters,
        progress.settled_after_task_id.as_deref(),
        remaining.saturating_add(1) as i64,
    )?;
    let has_more = rows.len() > remaining;
    rows.truncate(remaining);
    for (task_id, runtime_state) in rows {
        progress.settled_after_task_id = Some(task_id.clone());
        batch.events.push(json!({
            "seq": Value::Null,
            "taskId": task_id,
            "type": CURRENT_RUNTIME_SNAPSHOT_TYPE,
            "payload": {
                "previousRuntimeState": Value::Null,
                "runtimeState": runtime_state,
                "currentState": true,
            },
            "createdAt": Value::Null,
            "synthetic": true,
        }));
    }
    progress.settled_complete = !has_more;
    batch.has_more |= has_more;
    Ok(())
}

async fn wait_local_task_events(
    state: Arc<AppState>,
    query: TaskEventsQuery,
    tolerate_missing_tasks: bool,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let supplied_current_cursor = query
        .cursor
        .as_deref()
        .map(decode_current_activity_cursor)
        .transpose()?
        .flatten();
    let mut current_progress = supplied_current_cursor
        .as_ref()
        .map(|cursor| CurrentActivityProgress {
            settled_after_task_id: cursor.settled_after_task_id.clone(),
            settled_complete: cursor.settled_complete,
        })
        .unwrap_or_default();
    let mut cursor = parse_cursor(
        supplied_current_cursor
            .as_ref()
            .map(|cursor| cursor.durable_cursor.as_str())
            .or(query.cursor.as_deref()),
    )?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    // Clamped in code, not only in the tool catalog: an override catalog must
    // not be able to hand back a window the MCP client is guaranteed to kill.
    let timeout_secs = kanna_tool_catalog::clamp_wait_timeout_secs(
        query
            .timeout_secs
            .unwrap_or(kanna_tool_catalog::DEFAULT_WAIT_TIMEOUT_SECS),
    );

    let db_path = state.config().db_path.clone();
    let (scope, filters) = {
        let db = Db::open(&db_path).map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("db error: {e}"),
            )
        })?;
        (
            resolve_scope(&db, &query, tolerate_missing_tasks)?,
            resolve_exclusions(&db, &query)?,
        )
    };
    let from_now = starts_at_current_tail(&query)?;
    if cursor.is_none() && from_now {
        let db = Db::open(&db_path).map_err(db_error)?;
        let head_seq = db.latest_task_event_seq().map_err(db_error)?;
        cursor = Some(match &scope {
            TaskEventScope::Children(parent_task_id) => {
                TaskEventsCursor::ParentV3(ParentTaskEventsCursorV3 {
                    parent_task_id: parent_task_id.clone(),
                    event_seq: head_seq,
                })
            }
            TaskEventScope::Tasks(_)
            | TaskEventScope::Repo(_)
            | TaskEventScope::RepoRemoteUrlHash(_) => TaskEventsCursor::Sequence(head_seq),
        });
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    // Reuse the aggregate's existing zero-time drain budget for a paged
    // bootstrap. Positive waits must never extend their receiver deadline
    // merely because excluded rows remain.
    let drain_deadline = if timeout_secs == 0 {
        tokio::time::Instant::now() + ZERO_TIMEOUT_DRAIN_BUDGET
    } else {
        deadline
    };
    let mut timing = super::subscription_timing::Collection::default();
    let min_events = kanna_tool_catalog::clamp_task_event_min_events(query.min_events, limit);
    let debounce = hold_duration(query.debounce_ms);
    // Collected across re-reads, not per read: a batched wait returns one
    // response for a stretch of the feed, and the checkpoint it hands back is
    // the last read's, so nothing between the two is skipped or repeated.
    let mut collected: Vec<Value> = Vec::new();
    let mut collected_has_more = false;
    // Set by the first event of the batch, never restarted by later ones: the
    // window bounds how long a burst is held, not how long the last event of a
    // steady stream can defer the response.
    let mut debounce_deadline: Option<tokio::time::Instant> = None;
    // Measured from the start of the call rather than from an event, so a
    // caller that loops as fast as it can still wakes at most once per
    // interval.
    let interval_deadline = interval_hold_deadline(query.min_interval_ms, deadline);
    loop {
        // Arm the wake-up *before* reading, and `enable()` it so it is actually
        // registered: an append landing between the read and the await must
        // wake this call, not wait for the next re-check.
        let mut appended = Box::pin(crate::db::task_event_appended());
        appended.as_mut().enable();
        let remaining_limit = limit.saturating_sub(collected.len() as i64).max(1);
        let mut batch = read_batch(&db_path, &scope, &filters, cursor.as_ref(), remaining_limit)?;
        if query.include_current_activity {
            append_current_activity_snapshots(
                &db_path,
                &scope,
                &filters,
                &mut batch,
                remaining_limit,
                &mut current_progress,
            )
            .map_err(db_error)?;
        }
        enrich_event_batch(
            state.config(),
            &mut batch,
            query.orchestration_notifications,
        )
        .map_err(|error| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, error))?;
        let output_cursor = if query.include_current_activity {
            encode_current_activity_cursor(&CurrentActivityCursor {
                durable_cursor: batch.cursor.clone(),
                settled_after_task_id: current_progress.settled_after_task_id.clone(),
                settled_complete: current_progress.settled_complete,
            })?
        } else {
            batch.cursor.clone()
        };
        if query.orchestration_notifications {
            batch
                .events
                .retain(kanna_tool_catalog::is_relevant_subscription_event);
            // Raw pagination is work for this wait, not a full actionable
            // batch. Continue from its checkpoint even on a zero-time scan.
            collected_has_more =
                batch.has_more && collected.len() + batch.events.len() >= limit as usize;
        } else {
            collected_has_more |= batch.has_more;
        }
        let read_events = !batch.events.is_empty();
        if query.subscription_timing {
            timing.observe(&batch.events, tokio::time::Instant::now());
            #[cfg(test)]
            super::subscription_timing::observed(&state, batch.events.len());
        }
        collected.append(&mut batch.events);
        if read_events && debounce_deadline.is_none() && !debounce.is_zero() {
            debounce_deadline = Some((tokio::time::Instant::now() + debounce).min(deadline));
        }
        let now = tokio::time::Instant::now();
        let hold_until = hold_deadline(debounce_deadline, interval_deadline);
        let batch_complete = if query.subscription_timing {
            timing.ready(collected.len(), limit, deadline, now)
        } else {
            kanna_tool_catalog::task_event_batch_is_complete(
                collected.len(),
                collected_has_more,
                limit,
                min_events,
                hold_elapsed(hold_until, now),
            )
        };
        if batch_complete {
            return Ok(Json(json!({
                "waitOutcome": "events",
                "cursor": output_cursor,
                "events": collected,
                "hasMore": collected_has_more,
            })));
        }

        if query.orchestration_notifications
            && batch.has_more
            && collected.len() < limit as usize
            && now < drain_deadline
        {
            tokio::task::yield_now().await;
            // Yielding can cross the deadline. Return the checkpoint already
            // consumed, without starting another read or losing the backlog.
            if tokio::time::Instant::now() < drain_deadline {
                cursor = parse_cursor(Some(&batch.cursor))?;
                continue;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            // The window closed before the batch filled. Whatever accumulated
            // is still returned and still acknowledged by the cursor: holding
            // events back for a batch the caller never asked to wait longer
            // for would lose them for a whole extra window.
            let collected_count = collected.len();
            return Ok(Json(json!({
                "waitOutcome": "timeout",
                "cursor": output_cursor,
                "events": collected,
                "hasMore": query.orchestration_notifications && batch.has_more,
                "waitTimeoutSecs": timeout_secs,
                "waitHint": if collected_count == 0 {
                    format!(
                        "no matching task events within {timeout_secs}s. This is not an error \
                         and nothing was lost — call kanna_wait_events again with the returned \
                         cursor to keep watching."
                    )
                } else {
                    format!(
                        "{collected_count} matching task events within {timeout_secs}s, fewer \
                         than the {min_events} requested. They are returned and acknowledged by \
                         the cursor — call kanna_wait_events again with it to keep watching."
                    )
                },
            })));
        }
        // Advance the in-flight checkpoint before waiting. Without this, every
        // five-second recheck would rescan unrelated rows between the caller's
        // original cursor and the latest head.
        cursor = parse_cursor(Some(&batch.cursor))?;
        // A batch already at `min_events` is only waiting out its hold window;
        // anything short of it waits for the full timeout.
        let wake_deadline = if query.subscription_timing {
            timing.deadline(deadline)
        } else {
            match hold_until {
                Some(hold_until) if collected.len() >= min_events => hold_until,
                _ => deadline,
            }
        };
        let _ = tokio::time::timeout(
            (wake_deadline.max(now) - now).min(Duration::from_secs(WAIT_RECHECK_SECS)),
            appended,
        )
        .await;
    }
}

fn hold_duration(hold_ms: Option<u64>) -> Duration {
    Duration::from_millis(kanna_tool_catalog::clamp_task_event_hold_ms(hold_ms))
}

/// `minIntervalMs` as an absolute instant, known before any event arrives and
/// capped at the wait's own deadline — a floor on the call, never an extension
/// of it.
fn interval_hold_deadline(
    min_interval_ms: Option<u64>,
    deadline: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    let interval = hold_duration(min_interval_ms);
    (!interval.is_zero()).then(|| (tokio::time::Instant::now() + interval).min(deadline))
}

/// The later of the two hold windows: both must close before a batch that is
/// otherwise ready may be returned.
fn hold_deadline(
    debounce_deadline: Option<tokio::time::Instant>,
    interval_deadline: Option<tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    match (debounce_deadline, interval_deadline) {
        (Some(debounce), Some(interval)) => Some(debounce.max(interval)),
        (held, None) | (None, held) => held,
    }
}

fn hold_elapsed(hold_until: Option<tokio::time::Instant>, now: tokio::time::Instant) -> bool {
    hold_until.is_none_or(|hold_until| now >= hold_until)
}

fn invalid_aggregate_cursor() -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        "cursor is not a valid multi-machine cursor returned by this endpoint".to_string(),
    )
}

/// `ks1` used to embed native cursors directly, so deployed values may contain
/// either a numeric/p3 cursor or a `kc1` current-state continuation. Accept
/// both, but serialize every per-machine checkpoint through one `ke1` envelope
/// so a caller never has to infer meaning from mixed map values.
fn decode_machine_cursor(cursor: &str) -> Result<String, (axum::http::StatusCode, String)> {
    let Some(encoded) = cursor.strip_prefix(MACHINE_CURSOR_PREFIX) else {
        return Ok(cursor.to_string());
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_aggregate_cursor())?;
    let decoded = String::from_utf8(bytes).map_err(|_| invalid_aggregate_cursor())?;
    if decoded.is_empty() || decoded.starts_with(MACHINE_CURSOR_PREFIX) {
        return Err(invalid_aggregate_cursor());
    }
    Ok(decoded)
}

fn encode_machine_cursor(cursor: &str) -> Result<String, (axum::http::StatusCode, String)> {
    let native = decode_machine_cursor(cursor)?;
    if native.is_empty() {
        return Err(invalid_aggregate_cursor());
    }
    Ok(format!(
        "{MACHINE_CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(native.as_bytes())
    ))
}

fn decode_aggregate_cursor(
    cursor: &str,
) -> Result<AggregateCursor, (axum::http::StatusCode, String)> {
    if cursor.len() > MAX_CURSOR_LEN {
        return Err(invalid_aggregate_cursor());
    }
    let encoded = cursor
        .strip_prefix(AGGREGATE_CURSOR_PREFIX)
        .ok_or_else(invalid_aggregate_cursor)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_aggregate_cursor())?;
    let mut cursor: AggregateCursor =
        serde_json::from_slice(&bytes).map_err(|_| invalid_aggregate_cursor())?;
    if cursor.local_machine_id.trim().is_empty()
        || cursor
            .machine_ids
            .iter()
            .any(|machine_id| machine_id.trim().is_empty())
        || cursor.machine_ids.len() > MAX_AGGREGATE_MACHINES
        || cursor
            .cursors_by_machine
            .keys()
            .any(|machine_id| !cursor.machine_ids.contains(machine_id))
        || cursor
            .machines_with_more
            .iter()
            .any(|machine_id| !cursor.machine_ids.contains(machine_id))
    {
        return Err(invalid_aggregate_cursor());
    }
    cursor.machine_ids.sort();
    cursor.machine_ids.dedup();
    for machine_cursor in cursor.cursors_by_machine.values_mut() {
        *machine_cursor = encode_machine_cursor(machine_cursor)?;
    }
    Ok(cursor)
}

fn encode_aggregate_cursor(
    cursor: &AggregateCursor,
) -> Result<String, (axum::http::StatusCode, String)> {
    let bytes = serde_json::to_vec(cursor).map_err(|error| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to encode multi-machine task event cursor: {error}"),
        )
    })?;
    let encoded = format!(
        "{AGGREGATE_CURSOR_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    if encoded.len() > MAX_CURSOR_LEN {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode a bounded multi-machine task event cursor".to_string(),
        ));
    }
    Ok(encoded)
}

fn normalized_values(raw: Option<&str>) -> Vec<String> {
    let mut values = raw
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    values.sort();
    values.dedup();
    values
}

fn resolve_aggregate_scope(
    db: &Db,
    query: &TaskEventsQuery,
) -> Result<AggregateScope, (axum::http::StatusCode, String)> {
    let task_ids = normalized_values(query.task_ids.as_deref());
    if !task_ids.is_empty() {
        return Ok(AggregateScope::Tasks { task_ids });
    }
    if let Some(parent_task_id) = query
        .parent_task_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parent_task_id = db
            .resolve_pipeline_item_id(parent_task_id)
            .map_err(db_error)?
            .unwrap_or_else(|| parent_task_id.to_string());
        return Ok(AggregateScope::Children { parent_task_id });
    }
    if let Some(repo_id) = query
        .repo_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let repo = db.get_repo(repo_id).map_err(db_error)?.ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("repo not found: {repo_id}"),
            )
        })?;
        let remote_url_hash = repo.remote_url_hash.ok_or_else(|| {
            (
                axum::http::StatusCode::CONFLICT,
                format!(
                    "repository {repo_id} has no remote URL hash, so it cannot be matched across machines"
                ),
            )
        })?;
        return Ok(AggregateScope::Repo { remote_url_hash });
    }
    if let Some(remote_url_hash) = query
        .repo_remote_url_hash
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(AggregateScope::Repo {
            remote_url_hash: remote_url_hash.to_string(),
        });
    }
    Err((
        axum::http::StatusCode::BAD_REQUEST,
        "task_ids, parent_task_id, repo_id or repo_remote_url_hash is required: an unscoped event feed would hand an orchestrator every other task's events too".to_string(),
    ))
}

fn local_query_for_aggregate(
    scope: &AggregateScope,
    filters: &TaskEventFilters,
    cursor: Option<String>,
    timeout_secs: u64,
    limit: i64,
    include_current_activity: bool,
    from_now: bool,
) -> TaskEventsQuery {
    let mut query = TaskEventsQuery {
        cursor,
        timeout_secs: Some(timeout_secs),
        limit: Some(limit),
        local_only: true,
        include_current_activity,
        from: from_now.then(|| "now".to_string()),
        exclude_task_ids: (!filters.exclude_task_ids.is_empty())
            .then(|| filters.exclude_task_ids.join(",")),
        exclude_event_types: (!filters.exclude_event_types.is_empty())
            .then(|| filters.exclude_event_types.join(",")),
        event_types: (!filters.include_event_types.is_empty())
            .then(|| filters.include_event_types.join(",")),
        exclude_own: filters.exclude_own_deliveries,
        // `minEvents` and `debounceMs` are deliberately *not* forwarded: they
        // shape the aggregated response, and a per-machine minimum would hold
        // a leg's events back while the fan-out already had enough of them.
        ..TaskEventsQuery::default()
    };
    match scope {
        AggregateScope::Tasks { task_ids } => query.task_ids = Some(task_ids.join(",")),
        AggregateScope::Children { parent_task_id } => {
            query.parent_task_id = Some(parent_task_id.clone());
        }
        AggregateScope::Repo { remote_url_hash } => {
            query.repo_remote_url_hash = Some(remote_url_hash.clone());
        }
    }
    query
}

fn aggregate_query_path(query: &TaskEventsQuery) -> String {
    let mut params = vec![
        format!("timeoutSecs={}", query.timeout_secs.unwrap_or_default()),
        format!("limit={}", query.limit.unwrap_or(DEFAULT_EVENT_LIMIT)),
        "localOnly=true".to_string(),
    ];
    params.push(format!(
        "includeCurrentActivity={}",
        query.include_current_activity
    ));
    if let Some(from) = query.from.as_deref() {
        params.push(format!("from={}", encode_path_segment(from)));
    }
    if let Some(task_ids) = query.task_ids.as_deref() {
        params.push(format!("taskIds={}", encode_path_segment(task_ids)));
    }
    if let Some(parent_task_id) = query.parent_task_id.as_deref() {
        params.push(format!(
            "parentTaskId={}",
            encode_path_segment(parent_task_id)
        ));
    }
    if let Some(remote_url_hash) = query.repo_remote_url_hash.as_deref() {
        params.push(format!(
            "repoRemoteUrlHash={}",
            encode_path_segment(remote_url_hash)
        ));
    }
    if let Some(exclude_task_ids) = query.exclude_task_ids.as_deref() {
        params.push(format!(
            "excludeTaskIds={}",
            encode_path_segment(exclude_task_ids)
        ));
    }
    if let Some(exclude_event_types) = query.exclude_event_types.as_deref() {
        params.push(format!(
            "excludeEventTypes={}",
            encode_path_segment(exclude_event_types)
        ));
    }
    if let Some(event_types) = query.event_types.as_deref() {
        params.push(format!("eventTypes={}", encode_path_segment(event_types)));
    }
    if query.orchestration_notifications {
        params.push("orchestrationNotifications=true".to_string());
    }
    if query.exclude_own {
        params.push("excludeOwn=true".to_string());
    }
    if let Some(cursor) = query.cursor.as_deref() {
        params.push(format!("cursor={}", encode_path_segment(cursor)));
    }
    format!("/v1/task-events?{}", params.join("&"))
}

fn aggregate_machine_wait_error(
    status: axum::http::StatusCode,
    error: String,
) -> AggregateMachineWaitError {
    let lower = error.to_ascii_lowercase();
    if status == axum::http::StatusCode::BAD_REQUEST
        && lower.contains("cursor")
        && (lower.contains("invalid") || lower.contains("not a valid") || lower.contains("expired"))
    {
        AggregateMachineWaitError::CursorRejected(error)
    } else {
        AggregateMachineWaitError::Unavailable(error)
    }
}

fn spawn_aggregate_wait(
    session: &mut AggregateWaitSession,
    state: Arc<AppState>,
    machine_id: String,
    timeout_secs: u64,
    limit: i64,
) -> Result<(), (axum::http::StatusCode, String)> {
    let mut query = local_query_for_aggregate(
        &session.cursor.scope,
        &session.filters,
        session
            .cursor
            .cursors_by_machine
            .get(&machine_id)
            .map(|cursor| decode_machine_cursor(cursor))
            .transpose()?,
        timeout_secs,
        limit,
        session.include_current_activity,
        session.from_now,
    );
    query.orchestration_notifications = session.orchestration_notifications;
    let local_machine_id = session.cursor.local_machine_id.clone();
    let completed_machine_id = machine_id.clone();
    let waited_machine_id = machine_id.clone();
    session.pending.spawn(async move {
        let result = if waited_machine_id == local_machine_id {
            wait_local_task_events(state, query, true)
                .await
                .map(|Json(value)| value)
                .map_err(|(status, error)| aggregate_machine_wait_error(status, error))
        } else {
            let path = aggregate_query_path(&query);
            match state
                .invoke_relay_desktop(waited_machine_id, "GET".to_string(), path, Value::Null)
                .await
            {
                Ok(response) if (200..300).contains(&response.status) => {
                    response.body.ok_or_else(|| {
                        AggregateMachineWaitError::Unavailable(
                            "peer returned an empty task-event response".to_string(),
                        )
                    })
                }
                Ok(response) => {
                    let status = axum::http::StatusCode::from_u16(response.status)
                        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
                    let error = response.error.unwrap_or_else(|| {
                        format!("peer task-event wait failed with HTTP {}", response.status)
                    });
                    Err(aggregate_machine_wait_error(status, error))
                }
                Err(_error) => Err(AggregateMachineWaitError::Unavailable(
                    state.desktop_routing_unreachable_error(),
                )),
            }
        };
        AggregateWaitCompletion {
            machine_id: completed_machine_id,
            result,
        }
    });
    session.pending_machines.insert(machine_id);
    Ok(())
}

fn apply_aggregate_completion(
    session: &mut AggregateWaitSession,
    completion: AggregateWaitCompletion,
    events: &mut Vec<Value>,
    machine_errors: &mut Vec<Value>,
    failed_machines: &mut HashSet<String>,
    has_more: &mut bool,
    limit: i64,
) -> Result<bool, (axum::http::StatusCode, String)> {
    session.pending_machines.remove(&completion.machine_id);
    let response = match completion.result {
        Ok(response) => response,
        Err(AggregateMachineWaitError::CursorRejected(error)) => {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "machine {} rejected its embedded task-event cursor ({error}); restart kanna_wait_events without a cursor to safely replay retained history",
                    completion.machine_id
                ),
            ));
        }
        Err(AggregateMachineWaitError::Unavailable(error)) => {
            failed_machines.insert(completion.machine_id.clone());
            machine_errors.push(json!({
                "machineId": completion.machine_id,
                "error": error,
                "stale": true,
            }));
            return Ok(false);
        }
    };
    let cursor = response
        .get("cursor")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!(
                    "machine {} returned a task-event response without a cursor",
                    completion.machine_id
                ),
            )
        })?;
    let response_has_more = response
        .get("hasMore")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let returned_events = response
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!(
                    "machine {} returned a task-event response without events",
                    completion.machine_id
                ),
            )
        })?;
    let remaining = (limit as usize).saturating_sub(events.len());
    // Consume excluded rows as well as selected ones, stopping immediately
    // before the first relevant row that would overflow the aggregate. The
    // checkpoint follows consumption, while thresholds count selection only.
    let consumed_count = if session.orchestration_notifications {
        let mut selected = 0;
        returned_events
            .iter()
            .take_while(|event| {
                if kanna_tool_catalog::is_relevant_subscription_event(event) {
                    if selected == remaining {
                        return false;
                    }
                    selected += 1;
                }
                true
            })
            .count()
    } else {
        returned_events.len().min(remaining)
    };
    if consumed_count < returned_events.len() || response_has_more {
        session
            .cursor
            .machines_with_more
            .insert(completion.machine_id.clone());
    } else {
        session
            .cursor
            .machines_with_more
            .remove(&completion.machine_id);
    }
    let before_count = events.len();
    let mut consumed_seq = None;
    let mut consumed_synthetic = false;
    let mut consumed_task_id = None;
    for event in returned_events.iter().take(consumed_count) {
        let mut event = event.as_object().cloned().ok_or_else(|| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!(
                    "machine {} returned a non-object task event",
                    completion.machine_id
                ),
            )
        })?;
        event.insert(
            "machineId".to_string(),
            Value::String(completion.machine_id.clone()),
        );
        consumed_seq = event.get("seq").and_then(Value::as_i64);
        consumed_synthetic = event
            .get("synthetic")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        consumed_task_id = event
            .get("taskId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let event = Value::Object(event);
        if !session.orchestration_notifications
            || kanna_tool_catalog::is_relevant_subscription_event(&event)
        {
            events.push(event);
        }
    }
    let next_cursor = if consumed_count == returned_events.len() {
        Some(cursor.to_string())
    } else if consumed_count == 0 {
        None
    } else if consumed_synthetic && consumed_seq.is_none() {
        let returned = decode_current_activity_cursor(cursor)?.ok_or_else(|| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!(
                    "machine {} returned paged synthetic task state without a current-activity cursor",
                    completion.machine_id
                ),
            )
        })?;
        Some(encode_current_activity_cursor(&CurrentActivityCursor {
            durable_cursor: returned.durable_cursor,
            settled_after_task_id: consumed_task_id,
            settled_complete: false,
        })?)
    } else {
        let event_seq = consumed_seq.ok_or_else(|| {
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!(
                    "machine {} returned a truncated task event without a numeric seq",
                    completion.machine_id
                ),
            )
        })?;
        let previous_wire_cursor = session
            .cursor
            .cursors_by_machine
            .get(&completion.machine_id);
        let previous_native_cursor = previous_wire_cursor
            .map(|cursor| decode_machine_cursor(cursor))
            .transpose()?;
        let previous_current = previous_native_cursor
            .as_deref()
            .map(decode_current_activity_cursor)
            .transpose()?
            .flatten();
        let durable_cursor = match &session.cursor.scope {
            AggregateScope::Children { parent_task_id } => cursor_after_truncated_parent_batch(
                parent_task_id,
                previous_native_cursor
                    .as_deref()
                    .map(durable_cursor_from_wire)
                    .transpose()?
                    .as_deref(),
                event_seq,
            )?,
            AggregateScope::Tasks { .. } | AggregateScope::Repo { .. } => event_seq.to_string(),
        };
        if session.include_current_activity {
            Some(encode_current_activity_cursor(&CurrentActivityCursor {
                durable_cursor,
                settled_after_task_id: previous_current
                    .as_ref()
                    .and_then(|cursor| cursor.settled_after_task_id.clone()),
                settled_complete: previous_current
                    .as_ref()
                    .is_some_and(|cursor| cursor.settled_complete),
            })?)
        } else {
            Some(durable_cursor)
        }
    };
    if let Some(next_cursor) = next_cursor {
        session.cursor.cursors_by_machine.insert(
            completion.machine_id.clone(),
            encode_machine_cursor(&next_cursor)?,
        );
    }
    if session.orchestration_notifications {
        *has_more = events.len() >= limit as usize && !session.cursor.machines_with_more.is_empty();
    } else {
        *has_more |= !session.cursor.machines_with_more.is_empty();
    }
    Ok(if session.orchestration_notifications {
        events.len() > before_count
    } else {
        !returned_events.is_empty()
    })
}

async fn wait_aggregate_task_events(
    state: Arc<AppState>,
    query: TaskEventsQuery,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let from_now = starts_at_current_tail(&query)?;
    let timeout_secs = kanna_tool_catalog::clamp_wait_timeout_secs(
        query
            .timeout_secs
            .unwrap_or(kanna_tool_catalog::DEFAULT_WAIT_TIMEOUT_SECS),
    );
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    let db = Db::open(&state.config().db_path).map_err(db_error)?;
    let requested_scope = resolve_aggregate_scope(&db, &query)?;
    let filters = TaskEventFilters {
        exclude_task_ids: normalized_values(query.exclude_task_ids.as_deref()),
        exclude_event_types: normalized_values(query.exclude_event_types.as_deref()),
        include_event_types: normalized_values(query.event_types.as_deref()),
        exclude_own_deliveries: query.exclude_own,
    };
    let supplied_cursor = query.cursor.as_deref();
    let decoded = supplied_cursor
        .filter(|cursor| cursor.starts_with(AGGREGATE_CURSOR_PREFIX))
        .map(decode_aggregate_cursor)
        .transpose()?;
    let local_machine_id = state.config().desktop_id.clone();
    let cursor = match decoded {
        Some(cursor) => {
            if cursor.local_machine_id != local_machine_id {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    format!(
                        "multi-machine event cursor belongs to local machine {}, but this server is {}",
                        cursor.local_machine_id, local_machine_id
                    ),
                ));
            }
            if cursor.scope != requested_scope {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    "cursor belongs to a different task-event scope".to_string(),
                ));
            }
            cursor
        }
        None => {
            let mut cursors_by_machine = BTreeMap::new();
            if let Some(native_cursor) = supplied_cursor {
                cursors_by_machine.insert(
                    local_machine_id.clone(),
                    encode_machine_cursor(native_cursor)?,
                );
            }
            AggregateCursor {
                local_machine_id: local_machine_id.clone(),
                scope: requested_scope,
                machine_ids: vec![local_machine_id.clone()],
                cursors_by_machine,
                machines_with_more: BTreeSet::new(),
            }
        }
    };
    drop(db);

    let input_cursor_key = supplied_cursor
        .filter(|cursor| cursor.starts_with(AGGREGATE_CURSOR_PREFIX))
        .map(str::to_string);
    ensure_aggregate_wait_reaper(&state);
    let mut session = {
        let mut registry = state
            .aggregate_task_event_waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = tokio::time::Instant::now();
        registry.evict_expired(now);
        input_cursor_key
            .as_ref()
            .and_then(|key| registry.sessions.remove(key))
            .unwrap_or_else(|| AggregateWaitSession {
                filters: filters.clone(),
                cursor,
                pending: tokio::task::JoinSet::new(),
                pending_machines: HashSet::new(),
                last_touched: now,
                include_current_activity: query.include_current_activity,
                from_now,
                orchestration_notifications: query.orchestration_notifications,
            })
    };
    if session.include_current_activity != query.include_current_activity
        || session.filters != filters
        || session.orchestration_notifications != query.orchestration_notifications
    {
        // Inherited legs were started under the old filter. A fresh JoinSet
        // aborts them on drop; joining them here would surface their
        // cancellation as a failed wait.
        session.pending = tokio::task::JoinSet::new();
        session.pending_machines.clear();
        session.include_current_activity = query.include_current_activity;
        session.filters = filters;
        session.orchestration_notifications = query.orchestration_notifications;
    }
    // A discovery fault may seal a subscription page before any leg starts.
    // Pin the local from-now boundary here so acknowledgement/recovery cannot
    // move it past facts appended while that fault page was pending. A native
    // cursor leaves the initial settled scan unacknowledged, as on a local wait.
    if query.subscription_timing
        && session.from_now
        && !session
            .cursor
            .cursors_by_machine
            .contains_key(&local_machine_id)
        && !session.pending_machines.contains(&local_machine_id)
    {
        let db = Db::open(&state.config().db_path).map_err(db_error)?;
        let head = db.latest_task_event_seq().map_err(db_error)?;
        let native_cursor = match &session.cursor.scope {
            AggregateScope::Children { parent_task_id } => encode_v3_cursor(parent_task_id, head)?,
            _ => head.to_string(),
        };
        session.cursor.cursors_by_machine.insert(
            local_machine_id.clone(),
            encode_machine_cursor(&native_cursor)?,
        );
    }
    let mut machine_errors = Vec::new();
    let mut active_machines = HashSet::from([local_machine_id.clone()]);
    match state.list_active_relay_desktops().await {
        Ok(machine_ids) => {
            active_machines.extend(machine_ids);
        }
        Err(_error) => {
            // Known peer ids below get the stable outage state. Before a
            // first successful listing, attribute discovery failure to this
            // machine's relay route rather than emitting an unowned error.
            if session.cursor.machine_ids.len() == 1 {
                machine_errors.push(json!({
                    "machineId": local_machine_id,
                    "error": state.desktop_routing_unreachable_error(),
                    "stale": true,
                }));
            }
        }
    }
    for machine_id in &active_machines {
        if !session.cursor.machine_ids.contains(machine_id) {
            session.cursor.machine_ids.push(machine_id.clone());
        }
    }
    session.cursor.machine_ids.sort();
    session.cursor.machine_ids.dedup();
    if session.cursor.machine_ids.len() > MAX_AGGREGATE_MACHINES {
        return Err((
            axum::http::StatusCode::BAD_GATEWAY,
            "relay returned too many machines for one task-event cursor".to_string(),
        ));
    }
    for machine_id in &session.cursor.machine_ids {
        if !active_machines.contains(machine_id) {
            machine_errors.push(json!({
                "machineId": machine_id,
                "error": state.desktop_routing_unreachable_error(),
                "stale": true,
            }));
        }
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let zero_timeout_deadline = tokio::time::Instant::now() + ZERO_TIMEOUT_DRAIN_BUDGET;
    let mut events = Vec::new();
    let mut completed_machines = HashSet::new();
    let mut failed_machines = HashSet::new();
    let mut has_more =
        !query.orchestration_notifications && !session.cursor.machines_with_more.is_empty();
    // Batching is applied by the machine serving the wait, over the events of
    // every leg together — the same place the timeout is enforced. A leg that
    // has already answered is simply re-armed while the batch is still filling,
    // which is how a burst split across machines still returns as one response.
    let mut timing = super::subscription_timing::Collection::default();
    let min_events = kanna_tool_catalog::clamp_task_event_min_events(query.min_events, limit);
    let debounce = hold_duration(query.debounce_ms);
    let mut debounce_deadline: Option<tokio::time::Instant> = None;
    let interval_deadline = interval_hold_deadline(query.min_interval_ms, deadline);
    loop {
        if query.subscription_timing && !machine_errors.is_empty() {
            break;
        }
        let remaining_secs = if timeout_secs == 0 {
            0
        } else {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() > 0))
                .max(1)
        };
        let machines_to_start = active_machines
            .iter()
            .filter(|machine_id| {
                !(session.pending_machines.contains(*machine_id)
                    || completed_machines.contains(*machine_id)
                    || failed_machines.contains(*machine_id))
            })
            .cloned()
            .collect::<Vec<_>>();
        for machine_id in machines_to_start {
            spawn_aggregate_wait(
                &mut session,
                Arc::clone(&state),
                machine_id,
                remaining_secs,
                limit,
            )?;
        }

        // A batch already holding `min_events` is only waiting out its hold
        // window; anything short of it waits for the whole timeout.
        let join_deadline = if query.subscription_timing {
            timing.deadline(deadline)
        } else {
            match hold_deadline(debounce_deadline, interval_deadline) {
                Some(hold_until) if events.len() >= min_events => hold_until,
                _ => deadline,
            }
        };
        let joined = if timeout_secs == 0 {
            tokio::time::timeout_at(zero_timeout_deadline, session.pending.join_next())
                .await
                .unwrap_or_default()
        } else {
            tokio::time::timeout_at(join_deadline, session.pending.join_next())
                .await
                .unwrap_or_default()
        };
        let Some(joined) = joined else {
            break;
        };
        let completion = joined.map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("machine wait task failed: {error}"),
            )
        })?;
        let completed_machine_id = completion.machine_id.clone();
        completed_machines.insert(completed_machine_id.clone());
        let before_count = events.len();
        let completion_had_events = apply_aggregate_completion(
            &mut session,
            completion,
            &mut events,
            &mut machine_errors,
            &mut failed_machines,
            &mut has_more,
            limit,
        )?;
        if query.subscription_timing {
            timing.observe(&events[before_count..], tokio::time::Instant::now());
            #[cfg(test)]
            super::subscription_timing::observed(&state, events.len() - before_count);
        }
        if completion_had_events && debounce_deadline.is_none() && !debounce.is_zero() {
            debounce_deadline = Some((tokio::time::Instant::now() + debounce).min(deadline));
        }
        let now = tokio::time::Instant::now();
        let batch_complete = if query.subscription_timing {
            timing.ready(events.len(), limit, deadline, now) || !machine_errors.is_empty()
        } else {
            kanna_tool_catalog::task_event_batch_is_complete(
                events.len(),
                has_more,
                limit,
                min_events,
                hold_elapsed(hold_deadline(debounce_deadline, interval_deadline), now),
            )
        };
        // Re-arm the leg that just answered whenever this wait is still
        // filling its batch. Without events that is the existing behaviour;
        // with `minEvents` or `debounceMs` it is also what lets a machine's
        // next events land in the same response instead of the next one.
        if !batch_complete
            && !failed_machines.contains(&completed_machine_id)
            && ((timeout_secs > 0 && now < deadline)
                || (timeout_secs == 0
                    && now < zero_timeout_deadline
                    && query.orchestration_notifications
                    && session
                        .cursor
                        .machines_with_more
                        .contains(&completed_machine_id)))
        {
            completed_machines.remove(&completed_machine_id);
        }
        if batch_complete {
            break;
        }
        if !machine_errors.is_empty() && completed_machines.len() >= active_machines.len() {
            break;
        }
        if completed_machines.len() >= active_machines.len()
            || (timeout_secs > 0 && tokio::time::Instant::now() >= deadline)
        {
            break;
        }
    }

    session.last_touched = tokio::time::Instant::now();
    let output_cursor = encode_aggregate_cursor(&session.cursor)?;
    {
        let mut registry = state
            .aggregate_task_event_waits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.evict_expired(tokio::time::Instant::now());
        registry.insert_bounded(output_cursor.clone(), session);
    }
    // Recomputed after the loop: a batch that broke out on its debounce
    // deadline is complete by the time the response is built, and a window
    // that closed short of `min_events` reports `timeout` with whatever
    // accumulated, exactly like the single-machine wait.
    let batch_complete = if query.subscription_timing {
        timing.ready(events.len(), limit, deadline, tokio::time::Instant::now())
    } else {
        kanna_tool_catalog::task_event_batch_is_complete(
            events.len(),
            has_more,
            limit,
            min_events,
            hold_elapsed(
                hold_deadline(debounce_deadline, interval_deadline),
                tokio::time::Instant::now(),
            ),
        )
    };
    let wait_outcome = if batch_complete {
        "events"
    } else if !machine_errors.is_empty() {
        "partial"
    } else {
        "timeout"
    };
    Ok(Json(json!({
        "waitOutcome": wait_outcome,
        "cursor": output_cursor,
        "events": events,
        "hasMore": has_more,
        "machineErrors": machine_errors,
        "waitTimeoutSecs": timeout_secs,
        "waitHint": if wait_outcome == "events" {
            Value::Null
        } else {
            Value::String("Pass the cursor back unchanged to keep watching every known machine. A stale machine keeps its previous native cursor and catches up after reconnect while retained history remains available.".to_string())
        },
    })))
}

pub(super) async fn wait_task_events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TaskEventsQuery>,
    account_wide_access: AccountWideTaskEventAccess,
    tunneled: Option<Extension<TunneledHttpInvoke>>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    wait_events_in_process(
        state,
        query,
        account_wide_access.is_authorized(),
        tunneled.is_some(),
    )
    .await
}

pub(super) async fn wait_subscription_events(
    state: Arc<AppState>,
    query: Value,
) -> Result<Value, String> {
    let mut query: TaskEventsQuery =
        serde_json::from_value(query).map_err(|error| format!("invalid event scope: {error}"))?;
    query.orchestration_notifications = true;
    query.subscription_timing = true;
    wait_events_in_process(state, query, true, false)
        .await
        .map(|Json(value)| value)
        .map_err(|(_, error)| error)
}

async fn wait_events_in_process(
    state: Arc<AppState>,
    mut query: TaskEventsQuery,
    account_wide: bool,
    tunneled: bool,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let mut input_short_cursor = query
        .cursor
        .as_deref()
        .filter(|cursor| cursor.starts_with(SHORT_CURSOR_PREFIX))
        .map(str::to_string);
    let use_short_cursor = query.short_cursor
        || query
            .cursor
            .as_deref()
            .is_some_and(|cursor| cursor.starts_with(SHORT_CURSOR_PREFIX));
    let cursor_reset = match resolve_short_cursor(&state, query.cursor.as_deref())? {
        ShortCursorResolution::Cursor(cursor) => {
            query.cursor = cursor;
            None
        }
        ShortCursorResolution::Reset(reason) => {
            log::warn!("[task-events] {reason}");
            // A fresh handle, so the response cursor visibly differs from the
            // one the caller replayed and the reset cannot pass unnoticed.
            input_short_cursor = None;
            query.cursor = None;
            Some(reason)
        }
    };

    let result = if tunneled || query.local_only {
        wait_local_task_events(Arc::clone(&state), query, tunneled).await
    } else if !account_wide {
        if query
            .cursor
            .as_deref()
            .is_some_and(|cursor| cursor.starts_with(AGGREGATE_CURSOR_PREFIX))
        {
            return Err((
                axum::http::StatusCode::FORBIDDEN,
                "this request is not authorized to resume an account-wide task-event cursor; retry with the local task-event bearer credential or start a new local-only wait".to_string(),
            ));
        }
        query.local_only = true;
        wait_local_task_events(Arc::clone(&state), query, false).await
    } else {
        let aggregate_cursor = query
            .cursor
            .as_deref()
            .is_some_and(|cursor| cursor.starts_with(AGGREGATE_CURSOR_PREFIX));
        if state.desktop_routing_available() || aggregate_cursor {
            wait_aggregate_task_events(Arc::clone(&state), query).await
        } else {
            // Preserve native numeric/p3 cursors when this server has never had
            // a relay route. The explicit error makes the incompleteness visible
            // without forcing every single-machine caller into a composite cursor.
            if query.subscription_timing {
                query.timeout_secs = Some(0);
            }
            let Json(mut response) =
                wait_local_task_events(Arc::clone(&state), query, false).await?;
            if let Some(response) = response.as_object_mut() {
                response.insert(
                    "machineErrors".to_string(),
                    json!([{
                        "machineId": state.config().desktop_id,
                        "error": state.desktop_routing_unreachable_error(),
                        "stale": true,
                    }]),
                );
            }
            Ok(Json(response))
        }
    }?;

    let mut result = if use_short_cursor {
        shorten_response_cursor(&state, input_short_cursor.as_deref(), result)?
    } else {
        result
    };
    if let Some(reason) = cursor_reset {
        if let Some(response) = result.0.as_object_mut() {
            response.insert("cursorReset".to_string(), Value::Bool(true));
            response.insert("cursorResetReason".to_string(), Value::String(reason));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod aggregate_wait_registry_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CancellationMarker(Arc<AtomicUsize>);

    impl Drop for CancellationMarker {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn pending_session(
        key: &str,
        last_touched: tokio::time::Instant,
        cancellations: Arc<AtomicUsize>,
    ) -> AggregateWaitSession {
        let machine_id = format!("machine-{key}");
        let mut pending = tokio::task::JoinSet::new();
        pending.spawn(async move {
            let _marker = CancellationMarker(cancellations);
            std::future::pending::<AggregateWaitCompletion>().await
        });
        tokio::task::yield_now().await;
        AggregateWaitSession {
            filters: TaskEventFilters::default(),
            cursor: AggregateCursor {
                local_machine_id: machine_id.clone(),
                scope: AggregateScope::Tasks {
                    task_ids: vec![key.to_string()],
                },
                machine_ids: vec![machine_id.clone()],
                cursors_by_machine: BTreeMap::new(),
                machines_with_more: BTreeSet::new(),
            },
            pending,
            pending_machines: HashSet::from([machine_id]),
            last_touched,
            include_current_activity: false,
            from_now: false,
            orchestration_notifications: false,
        }
    }

    #[tokio::test]
    async fn registry_cap_evicts_and_aborts_the_oldest_session() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let mut registry = AggregateWaitRegistry::default();
        let now = tokio::time::Instant::now();
        for index in 0..=MAX_AGGREGATE_WAIT_SESSIONS {
            let session = pending_session(
                &index.to_string(),
                now + Duration::from_nanos(index as u64),
                Arc::clone(&cancellations),
            )
            .await;
            registry.insert_bounded(index.to_string(), session);
        }
        tokio::task::yield_now().await;

        assert_eq!(registry.sessions.len(), MAX_AGGREGATE_WAIT_SESSIONS);
        assert!(!registry.sessions.contains_key("0"));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);

        for (_, session) in registry.sessions.drain() {
            AggregateWaitRegistry::abort_session(session);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_expires_abandoned_sessions_and_aborts_their_legs() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(std::sync::Mutex::new(AggregateWaitRegistry::default()));
        let session = pending_session(
            "expired",
            tokio::time::Instant::now(),
            Arc::clone(&cancellations),
        )
        .await;
        registry
            .lock()
            .expect("registry")
            .insert_bounded("expired".to_string(), session);
        start_aggregate_wait_reaper(Arc::clone(&registry));
        tokio::task::yield_now().await;

        tokio::time::advance(AGGREGATE_WAIT_SESSION_TTL).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(registry.lock().expect("registry").sessions.is_empty());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    }
}
