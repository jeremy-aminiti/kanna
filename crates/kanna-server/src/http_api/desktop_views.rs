//! "Show this in the desktop" — one acknowledged action.
//!
//! An agent working inside a task can already read its files, its diff and its
//! commit graph; what it could not do was put one of them in front of the
//! person watching that task. `POST /v1/desktop/views/open` closes that gap:
//! it focuses one Kanna window, selects the named task, opens one whitelisted
//! read-only view of it, aims that view at an optional target, and answers
//! only once the window says the view and its target are on screen.
//!
//! Two properties are the whole point of the lane:
//!
//! * **A queued command is not an opened view.** The command is appended to a
//!   bounded in-memory lane the desktop long-polls; a closed desktop loses it.
//!   So the route does not answer when it appends — it registers the request
//!   id, waits for the window's acknowledgement, and reports
//!   `opened: false, code: "desktop_unavailable"` when none arrives. An agent
//!   telling a reviewer "I opened it for you" must be telling the truth.
//! * **Every target is resolved against the task's own current worktree**,
//!   which the database owns. A caller names a task and a repository-relative
//!   path; it never names a filesystem root, and a path that leaves the
//!   worktree — absolute, traversing, or through a symlink — is refused by the
//!   same descriptor-relative resolution the file and browse routes use,
//!   before anything is queued.
//!
//! The action navigates and nothing else. It writes no task state and no
//! `task_input` row: this is not an instruction to the agent, and the durable
//! instruction history must not read as though it were.

use super::lan_trust::{DesktopLocalAccess, PrivilegedTaskAccess};
use super::state::AppState;
use crate::db::Db;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::oneshot;

const DEFAULT_EVENT_LIMIT: usize = 100;
const MAX_EVENT_LIMIT: usize = 500;
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 25;
const MAX_WAIT_TIMEOUT_SECS: u64 = 120;

/// How long the route waits for a window to say the view is on screen.
///
/// Long enough for a cold view to read a file, a diff or a graph and render
/// it; short enough that a desktop which is not running is reported as absent
/// rather than leaving the caller hanging.
pub(crate) const DEFAULT_OPEN_TIMEOUT_MS: u64 = 10_000;

/// The views this action may open.
///
/// A whitelist rather than "any main tab": `shell` runs commands, `image`
/// takes an arbitrary URL, and `preferences` is not a view of a task. Adding a
/// kind here means adding its target shape, its renderer handler and its
/// readiness contract too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DesktopViewKind {
    Agent,
    File,
    Diff,
    Tree,
    Graph,
    Analytics,
}

impl DesktopViewKind {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "agent" => Self::Agent,
            "file" => Self::File,
            "diff" => Self::Diff,
            "tree" => Self::Tree,
            "graph" => Self::Graph,
            "analytics" => Self::Analytics,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::File => "file",
            Self::Diff => "diff",
            Self::Tree => "tree",
            Self::Graph => "graph",
            Self::Analytics => "analytics",
        }
    }
}

/// An expected failure: something the caller or the desktop can act on, told
/// as `opened: false` with a stable code rather than as an HTTP error, so one
/// field — `opened` — is the whole answer to "is it on their screen?".
#[derive(Debug, Clone)]
pub(super) struct OpenViewFailure {
    code: &'static str,
    message: String,
}

impl OpenViewFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn map_task_file_failure(error: crate::task_files::TaskFileError) -> OpenViewFailure {
    use crate::task_files::TaskFileError;
    let code = match &error {
        TaskFileError::InvalidPath(_) => "invalid_path",
        TaskFileError::TaskNotFound => "task_not_found",
        TaskFileError::WorkspaceUnavailable => "workspace_unavailable",
        TaskFileError::FileNotFound => "file_not_found",
        TaskFileError::TooLarge => "file_too_large",
        TaskFileError::UnsupportedContent => "unsupported_content",
        TaskFileError::RequestTooLarge => "invalid_target",
        TaskFileError::Internal(_) => "internal",
    };
    OpenViewFailure::new(code, error.to_string())
}

fn map_browse_failure(error: crate::repo_browser::BrowseError) -> OpenViewFailure {
    use crate::repo_browser::BrowseError;
    let code = match &error {
        BrowseError::InvalidPath | BrowseError::NotFile => "invalid_path",
        BrowseError::RootNotFound => "workspace_unavailable",
        BrowseError::TargetNotFound => "file_not_found",
        BrowseError::Internal(_) => "internal",
    };
    OpenViewFailure::new(code, error.to_string())
}

fn map_diff_failure(error: crate::task_diff::TaskDiffError) -> OpenViewFailure {
    use crate::task_diff::TaskDiffError;
    let code = match &error {
        TaskDiffError::InvalidRequest(_) => "invalid_target",
        TaskDiffError::TaskNotFound => "task_not_found",
        TaskDiffError::WorkspaceUnavailable => "workspace_unavailable",
        TaskDiffError::Internal(_) => "internal",
    };
    OpenViewFailure::new(code, error.to_string())
}

fn map_graph_failure(error: crate::task_graph::TaskGraphError) -> OpenViewFailure {
    use crate::task_graph::TaskGraphError;
    let code = match &error {
        TaskGraphError::TaskNotFound => "task_not_found",
        TaskGraphError::WorkspaceUnavailable => "workspace_unavailable",
        TaskGraphError::Internal(_) => "internal",
    };
    OpenViewFailure::new(code, error.to_string())
}

// ---------------------------------------------------------------------------
// Target shapes
// ---------------------------------------------------------------------------

/// `file`: a worktree-relative path, optionally aimed at a line or a range.
///
/// Coordinates are 1-based and inclusive, counted in Unicode scalar values —
/// the unit the viewer highlights in, so a caller reading a file and pointing
/// at what it read lands on what it meant.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileTarget {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    column: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default)]
    end_column: Option<u32>,
}

/// `tree`: reveal one file or directory in the explorer, or open it at the
/// worktree root when no path is given.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TreeTarget {
    #[serde(default)]
    path: Option<String>,
}

/// `diff`: a scope, and optionally one anchored line inside it.
///
/// The anchor is a path plus a side plus a line, never a hunk ordinal: hunk
/// numbering shifts with every commit, so an ordinal recorded by a reviewer is
/// pointing somewhere else by the time a human clicks it. The viewer opens the
/// hunk that contains the anchored line. `excerpt` is optional staleness
/// protection: when present, the anchored line must still contain it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiffTarget {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    excerpt: Option<String>,
}

/// `graph`: select one commit, named by its full object id.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GraphTarget {
    #[serde(default)]
    commit: Option<String>,
}

fn parse_target<T: serde::de::DeserializeOwned>(
    view: DesktopViewKind,
    target: Value,
) -> Result<T, OpenViewFailure> {
    serde_json::from_value(target).map_err(|error| {
        OpenViewFailure::new(
            "invalid_target",
            format!("{} target is invalid: {error}", view.as_str()),
        )
    })
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OpenDesktopViewRequest {
    task_id: String,
    /// Kept as a string so an unrecognised view is answered with the
    /// whitelist rather than with a deserialization rejection.
    view: String,
    #[serde(default)]
    target: Option<Value>,
}

pub(super) async fn open_desktop_view(
    _access: PrivilegedTaskAccess,
    State(state): State<Arc<AppState>>,
    Json(request): Json<OpenDesktopViewRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let prepared = {
        let state = Arc::clone(&state);
        super::blocking::run_handler_blocking("desktop view open", move || {
            let db = Db::open(&state.config().db_path).map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("db error: {error}"),
                )
            })?;
            Ok(prepare_open(&db, request))
        })
        .await?
    };

    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(failure) => return Ok(Json(failure_body(failure))),
    };

    // Only now is a window asked, and only now does the caller start waiting:
    // resolving first is what makes a mistyped path an error the agent can act
    // on rather than a window that quietly opens nothing.
    let acknowledgement = state.desktop_view_acks().register();
    let mut command = json!({
        "type": "desktop_view_open",
        "requestId": acknowledgement.request_id(),
        "taskId": prepared.task_id,
        "view": prepared.view.as_str(),
    });
    if let Some(target) = prepared.target.clone() {
        command["target"] = target;
    }
    state.desktop_view_commands().append(command);

    let timeout = Duration::from_millis(state.desktop_view_open_timeout_ms());
    match acknowledgement.wait(timeout).await {
        Some(DesktopViewAck { opened: true, .. }) => {
            let mut body = json!({ "opened": true, "view": prepared.view.as_str() });
            if let Some(target) = prepared.target {
                body["target"] = target;
            }
            Ok(Json(body))
        }
        Some(ack) => Ok(Json(failure_body(OpenViewFailure::new(
            renderer_failure_code(ack.code.as_deref()),
            ack.message
                .unwrap_or_else(|| "the desktop could not show the requested view".to_string()),
        )))),
        None => Ok(Json(failure_body(OpenViewFailure::new(
            "desktop_unavailable",
            "no Kanna window acknowledged the request; the desktop may be closed, \
             on another machine, or busy",
        )))),
    }
}

/// A window's own verdict is its own vocabulary, but an unknown code must not
/// leak through as though the server had classified it.
fn renderer_failure_code(code: Option<&str>) -> &'static str {
    match code {
        Some("task_not_found") => "task_not_found",
        Some("workspace_unavailable") => "workspace_unavailable",
        Some("file_not_found") => "file_not_found",
        Some("invalid_target") => "invalid_target",
        Some("diff_target_not_found") => "diff_target_not_found",
        Some("commit_not_found") => "commit_not_found",
        _ => "renderer_failed",
    }
}

fn failure_body(failure: OpenViewFailure) -> Value {
    json!({
        "opened": false,
        "code": failure.code,
        "message": failure.message,
    })
}

struct PreparedOpen {
    task_id: String,
    view: DesktopViewKind,
    target: Option<Value>,
}

fn prepare_open(db: &Db, request: OpenDesktopViewRequest) -> Result<PreparedOpen, OpenViewFailure> {
    let Some(view) = DesktopViewKind::parse(request.view.trim()) else {
        return Err(OpenViewFailure::new(
            "unsupported_view",
            format!(
                "unknown view {:?}; open_view shows agent, file, diff, tree, graph or analytics",
                request.view
            ),
        ));
    };

    let task_id = resolve_task(db, &request.task_id)?;
    let target = resolve_target(db, &task_id, view, request.target)?;
    Ok(PreparedOpen {
        task_id,
        view,
        target,
    })
}

/// Resolve a task id or its *current* branch name.
///
/// A task's work moves between workspaces, so an older branch name is not a
/// second name for the task — it names a workspace the task has left. Saying
/// so is worth a code of its own: "not found" would send a caller looking for
/// a task that is right there under a newer branch.
fn resolve_task(db: &Db, task_or_branch_id: &str) -> Result<String, OpenViewFailure> {
    let requested = task_or_branch_id.trim();
    if requested.is_empty() {
        return Err(OpenViewFailure::new(
            "invalid_target",
            "task_id must not be empty",
        ));
    }
    if let Some(task_id) = db
        .resolve_pipeline_item_id(requested)
        .map_err(|error| OpenViewFailure::new("internal", format!("db error: {error}")))?
    {
        return Ok(task_id);
    }
    if let Some(task_id) = db
        .resolve_task_by_workspace_branch(requested)
        .map_err(|error| OpenViewFailure::new("internal", format!("db error: {error}")))?
    {
        return Err(OpenViewFailure::new(
            "stale_branch_alias",
            format!(
                "branch {requested} is a workspace task {task_id} has left; pass the task id, \
                 which is stable across stages"
            ),
        ));
    }
    Err(OpenViewFailure::new(
        "task_not_found",
        format!("no task matches {requested}"),
    ))
}

fn resolve_target(
    db: &Db,
    task_id: &str,
    view: DesktopViewKind,
    target: Option<Value>,
) -> Result<Option<Value>, OpenViewFailure> {
    let target = match target {
        Some(Value::Null) | None => None,
        Some(value) if !value.is_object() => {
            return Err(OpenViewFailure::new(
                "invalid_target",
                "target must be an object",
            ))
        }
        Some(value) => Some(value),
    };

    match (view, target) {
        (DesktopViewKind::Agent | DesktopViewKind::Analytics, Some(_)) => {
            Err(OpenViewFailure::new(
                "unsupported_target",
                format!("the {} view takes no target", view.as_str()),
            ))
        }
        (DesktopViewKind::Agent | DesktopViewKind::Analytics, None) => Ok(None),
        (DesktopViewKind::File, None) => Err(OpenViewFailure::new(
            "invalid_target",
            "the file view needs a target naming the path to open",
        )),
        (DesktopViewKind::File, Some(value)) => {
            resolve_file_target(db, task_id, parse_target(view, value)?).map(Some)
        }
        (DesktopViewKind::Tree, value) => {
            let target = match value {
                Some(value) => parse_target::<TreeTarget>(view, value)?,
                None => TreeTarget { path: None },
            };
            resolve_tree_target(db, task_id, target)
        }
        (DesktopViewKind::Diff, value) => {
            let target = match value {
                Some(value) => parse_target::<DiffTarget>(view, value)?,
                None => DiffTarget {
                    scope: None,
                    path: None,
                    side: None,
                    line: None,
                    excerpt: None,
                },
            };
            resolve_diff_target(db, task_id, target).map(Some)
        }
        (DesktopViewKind::Graph, value) => {
            let target = match value {
                Some(value) => parse_target::<GraphTarget>(view, value)?,
                None => GraphTarget { commit: None },
            };
            resolve_graph_target(db, task_id, target)
        }
    }
}

fn resolve_file_target(
    db: &Db,
    task_id: &str,
    target: FileTarget,
) -> Result<Value, OpenViewFailure> {
    // The same resolution `/v1/tasks/{id}/files/content` performs, so an
    // absolute path, a traversal, a symlinked escape, a missing file, an
    // oversized one or one the viewer cannot render is refused here. The
    // content it reads on the way is what the range is checked against; the
    // desktop reads the file itself.
    let file = crate::task_files::read_task_file(db, task_id, &target.path)
        .map_err(map_task_file_failure)?;

    let mut resolved = json!({ "path": file.path });
    let Some(line) = positive("line", target.line)? else {
        if target.column.is_some() || target.end_line.is_some() || target.end_column.is_some() {
            return Err(OpenViewFailure::new(
                "invalid_target",
                "a column or end of range needs a line to start from",
            ));
        }
        return Ok(resolved);
    };

    let lines: Vec<&str> = file.content.split('\n').collect();
    let total = lines.len() as u32;
    if line > total {
        return Err(OpenViewFailure::new(
            "invalid_range",
            format!(
                "{} has {total} lines; line {line} is past its end",
                file.path
            ),
        ));
    }
    let end_line = positive("endLine", target.end_line)?.unwrap_or(line);
    if end_line < line {
        return Err(OpenViewFailure::new(
            "invalid_range",
            "endLine must not be before line",
        ));
    }
    if end_line > total {
        return Err(OpenViewFailure::new(
            "invalid_range",
            format!(
                "{} has {total} lines; endLine {end_line} is past its end",
                file.path
            ),
        ));
    }
    let column = positive("column", target.column)?;
    let end_column = positive("endColumn", target.end_column)?;
    check_column(&file.path, &lines, line, "column", column)?;
    check_column(&file.path, &lines, end_line, "endColumn", end_column)?;
    if line == end_line {
        if let (Some(column), Some(end_column)) = (column, end_column) {
            if end_column < column {
                return Err(OpenViewFailure::new(
                    "invalid_range",
                    "endColumn must not be before column on the same line",
                ));
            }
        }
    }

    resolved["line"] = json!(line);
    if let Some(column) = column {
        resolved["column"] = json!(column);
    }
    if end_line != line || end_column.is_some() {
        resolved["endLine"] = json!(end_line);
    }
    if let Some(end_column) = end_column {
        resolved["endColumn"] = json!(end_column);
    }
    Ok(resolved)
}

/// A column may sit one past the last character — that is the end of the line,
/// which is what an exclusive-feeling caller most often means by "to here".
fn check_column(
    path: &str,
    lines: &[&str],
    line: u32,
    field: &str,
    column: Option<u32>,
) -> Result<(), OpenViewFailure> {
    let Some(column) = column else { return Ok(()) };
    let text = lines[(line - 1) as usize].trim_end_matches('\r');
    let width = text.chars().count() as u32;
    if column > width.saturating_add(1) {
        return Err(OpenViewFailure::new(
            "invalid_range",
            format!("{path} line {line} is {width} characters; {field} {column} is past its end"),
        ));
    }
    Ok(())
}

fn positive(field: &str, value: Option<u32>) -> Result<Option<u32>, OpenViewFailure> {
    match value {
        Some(0) => Err(OpenViewFailure::new(
            "invalid_range",
            format!("{field} is 1-based, so 0 is not a position"),
        )),
        other => Ok(other),
    }
}

fn resolve_tree_target(
    db: &Db,
    task_id: &str,
    target: TreeTarget,
) -> Result<Option<Value>, OpenViewFailure> {
    let Some(path) = target.path else {
        return Ok(None);
    };
    let root = crate::repo_browser::task_root(db, task_id).map_err(map_browse_failure)?;
    // A tree target may be either kind of thing to reveal, so ask the browser
    // for it as a directory first and as a file second. Both resolutions are
    // descriptor-relative to the worktree root, so an escape fails both.
    match crate::repo_browser::list_directory(&root, &path, true, 0, 1, None) {
        Ok(listing) => Ok(Some(json!({ "path": listing.path, "kind": "directory" }))),
        Err(crate::repo_browser::BrowseError::Internal(message)) => {
            Err(OpenViewFailure::new("internal", message))
        }
        Err(directory_error) => {
            match crate::repo_browser::read_file_range(&root, &path, 0, 0, 1, true) {
                Ok(file) => Ok(Some(json!({ "path": file.path, "kind": "file" }))),
                // The directory attempt is the one that saw the path as a
                // path; report a traversal as a traversal rather than as the
                // file reader's second opinion.
                Err(crate::repo_browser::BrowseError::TargetNotFound) => {
                    Err(map_browse_failure(directory_error))
                }
                Err(file_error) => Err(map_browse_failure(file_error)),
            }
        }
    }
}

fn resolve_diff_target(
    db: &Db,
    task_id: &str,
    target: DiffTarget,
) -> Result<Value, OpenViewFailure> {
    let scope = match target.scope.as_deref() {
        None => "branch",
        Some("branch") => "branch",
        Some("working") => "working",
        Some(other) => {
            return Err(OpenViewFailure::new(
                "invalid_target",
                format!("unknown diff scope {other:?}; it is branch or working"),
            ))
        }
    };

    let anchored = target.path.is_some() || target.side.is_some() || target.line.is_some();
    if !anchored {
        if target.excerpt.is_some() {
            return Err(OpenViewFailure::new(
                "invalid_target",
                "an excerpt only guards an anchored line, so it needs path, side and line",
            ));
        }
        return Ok(json!({ "scope": scope }));
    }
    let (Some(path), Some(side), Some(line)) = (
        target.path.as_deref(),
        target.side.as_deref(),
        positive("line", target.line)?,
    ) else {
        return Err(OpenViewFailure::new(
            "invalid_target",
            "a diff line target needs path, side and line together",
        ));
    };
    if side != "old" && side != "new" {
        return Err(OpenViewFailure::new(
            "invalid_target",
            format!("unknown diff side {side:?}; it is old or new"),
        ));
    }

    let request =
        crate::task_diff::TaskDiffRequest::parse(Some(scope), None).map_err(map_diff_failure)?;
    let diff = crate::task_diff::read_task_diff(db, task_id, request).map_err(map_diff_failure)?;
    let anchor = locate_diff_line(&diff.patch, path, side, line)?;
    if let Some(excerpt) = target.excerpt.as_deref() {
        let excerpt = excerpt.trim();
        if !excerpt.is_empty() && !anchor.text.contains(excerpt) {
            return Err(OpenViewFailure::new(
                "diff_target_stale",
                format!(
                    "{path} {side} line {line} reads {:?}, which no longer contains the excerpt",
                    anchor.text
                ),
            ));
        }
    }

    // The anchor carries both sides' numbering and what kind of line it is,
    // because the rendered diff numbers each row by its own side: without
    // that, a context line's old and new numbers are indistinguishable in the
    // DOM and the view would scroll to whichever it found first.
    let mut resolved = json!({
        "scope": scope,
        "path": path,
        "side": side,
        "line": line,
        "anchorKind": anchor.kind,
    });
    if let Some(old_line) = anchor.old_line {
        resolved["oldLine"] = json!(old_line);
    }
    if let Some(new_line) = anchor.new_line {
        resolved["newLine"] = json!(new_line);
    }
    if let Some(excerpt) = target.excerpt {
        resolved["excerpt"] = json!(excerpt);
    }
    Ok(resolved)
}

#[derive(Debug)]
struct DiffAnchor {
    text: String,
    /// Which side(s) of the diff number this line. A deletion has no new-side
    /// number and an addition has no old-side one; a context line has both.
    kind: &'static str,
    old_line: Option<u32>,
    new_line: Option<u32>,
}

/// Find one line of one side of one file in a unified patch.
///
/// Walking the patch is what turns "line 40 of the new side" into a claim the
/// server has checked: the diff the window will render is the diff this read,
/// so an anchor that is not in it is refused here instead of arriving as a
/// window that scrolled nowhere. A path that appears on the requested side
/// more than once is ambiguous rather than resolved to the first one.
fn locate_diff_line(
    patch: &str,
    path: &str,
    side: &str,
    line: u32,
) -> Result<DiffAnchor, OpenViewFailure> {
    let wanted_old = side == "old";
    let mut matching_sections = 0usize;
    let mut found: Option<DiffAnchor> = None;
    let mut in_target_file = false;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut in_hunk = false;

    for raw in patch.lines() {
        if raw.starts_with("diff --git ") {
            in_target_file = false;
            in_hunk = false;
            continue;
        }
        if let Some(header_path) = raw.strip_prefix("--- ") {
            if wanted_old {
                in_target_file = patch_path_matches(header_path, path);
                if in_target_file {
                    matching_sections += 1;
                }
            }
            in_hunk = false;
            continue;
        }
        if let Some(header_path) = raw.strip_prefix("+++ ") {
            if !wanted_old {
                in_target_file = patch_path_matches(header_path, path);
                if in_target_file {
                    matching_sections += 1;
                }
            }
            in_hunk = false;
            continue;
        }
        if raw.starts_with("@@ ") {
            if let Some((old_start, new_start)) = parse_hunk_header(raw) {
                old_line = old_start;
                new_line = new_start;
                in_hunk = true;
            } else {
                in_hunk = false;
            }
            continue;
        }
        if !in_hunk || !in_target_file {
            continue;
        }
        let Some(marker) = raw.chars().next() else {
            // A bare empty line inside a hunk is an unchanged empty line.
            if (wanted_old && old_line == line) || (!wanted_old && new_line == line) {
                found.get_or_insert(DiffAnchor {
                    text: String::new(),
                    kind: "context",
                    old_line: Some(old_line),
                    new_line: Some(new_line),
                });
            }
            old_line += 1;
            new_line += 1;
            continue;
        };
        let body = raw[1..].to_string();
        match marker {
            ' ' => {
                if (wanted_old && old_line == line) || (!wanted_old && new_line == line) {
                    found.get_or_insert(DiffAnchor {
                        text: body,
                        kind: "context",
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                    });
                }
                old_line += 1;
                new_line += 1;
            }
            '-' => {
                if wanted_old && old_line == line {
                    found.get_or_insert(DiffAnchor {
                        text: body,
                        kind: "deletion",
                        old_line: Some(old_line),
                        new_line: None,
                    });
                }
                old_line += 1;
            }
            '+' => {
                if !wanted_old && new_line == line {
                    found.get_or_insert(DiffAnchor {
                        text: body,
                        kind: "addition",
                        old_line: None,
                        new_line: Some(new_line),
                    });
                }
                new_line += 1;
            }
            // `\ No newline at end of file` and anything else a generator
            // adds belong to neither side's numbering.
            _ => {}
        }
    }

    if matching_sections > 1 {
        return Err(OpenViewFailure::new(
            "diff_target_ambiguous",
            format!("{path} appears more than once on the {side} side of this diff"),
        ));
    }
    found.ok_or_else(|| {
        OpenViewFailure::new(
            "diff_target_not_found",
            if matching_sections == 0 {
                format!("{path} is not part of this diff on the {side} side")
            } else {
                format!("{side} line {line} of {path} is not inside any hunk of this diff")
            },
        )
    })
}

/// `--- a/src/main.rs` / `+++ b/src/main.rs`, with the tab-separated timestamp
/// some generators append, and `/dev/null` for an added or deleted file.
fn patch_path_matches(header: &str, path: &str) -> bool {
    let header = header.split('\t').next().unwrap_or(header).trim();
    if header == "/dev/null" {
        return false;
    }
    let stripped = header
        .strip_prefix("a/")
        .or_else(|| header.strip_prefix("b/"))
        .unwrap_or(header);
    stripped == path
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let inner = line.strip_prefix("@@ ")?;
    let inner = inner.split(" @@").next()?;
    let mut parts = inner.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let old_start = old.split(',').next()?.parse::<u32>().ok()?;
    let new_start = new.split(',').next()?.parse::<u32>().ok()?;
    Some((old_start, new_start))
}

fn resolve_graph_target(
    db: &Db,
    task_id: &str,
    target: GraphTarget,
) -> Result<Option<Value>, OpenViewFailure> {
    let Some(commit) = target.commit else {
        return Ok(None);
    };
    let commit = commit.trim().to_lowercase();
    if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(OpenViewFailure::new(
            "invalid_target",
            "commit must be a full 40-character object id; abbreviations are ambiguous",
        ));
    }
    let graph = crate::task_graph::read_task_graph(db, task_id, None).map_err(map_graph_failure)?;
    if !graph
        .commits
        .iter()
        .any(|entry| entry.hash.eq_ignore_ascii_case(&commit))
    {
        return Err(OpenViewFailure::new(
            "commit_not_found",
            format!("commit {commit} is not in this task's graph"),
        ));
    }
    Ok(Some(json!({ "commit": commit })))
}

// ---------------------------------------------------------------------------
// Acknowledgement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(super) struct DesktopViewAck {
    opened: bool,
    code: Option<String>,
    message: Option<String>,
}

/// The in-flight opens, keyed by request id.
///
/// In memory and per process on purpose: a request only means anything while
/// the caller is still waiting for it, and the server that issued the id is
/// the only one that can answer it.
#[derive(Default)]
pub(crate) struct DesktopViewAcks {
    pending: StdMutex<HashMap<String, oneshot::Sender<DesktopViewAck>>>,
}

pub(super) struct PendingDesktopViewOpen {
    request_id: String,
    receiver: oneshot::Receiver<DesktopViewAck>,
    acks: Arc<DesktopViewAcks>,
}

impl PendingDesktopViewOpen {
    fn request_id(&self) -> &str {
        &self.request_id
    }

    async fn wait(self, timeout: Duration) -> Option<DesktopViewAck> {
        let PendingDesktopViewOpen {
            request_id,
            receiver,
            acks,
        } = self;
        let result = tokio::time::timeout(timeout, receiver).await;
        // A timed-out or dropped request must not sit in the map: the window
        // may answer late, and the entry would otherwise be leaked.
        acks.forget(&request_id);
        match result {
            Ok(Ok(ack)) => Some(ack),
            _ => None,
        }
    }
}

impl DesktopViewAcks {
    pub(super) fn register(self: &Arc<Self>) -> PendingDesktopViewOpen {
        let request_id = format!(
            "view-{}",
            crate::transfer_engine::queue::unique_work_nonce()
        );
        let (sender, receiver) = oneshot::channel();
        self.lock().insert(request_id.clone(), sender);
        PendingDesktopViewOpen {
            request_id,
            receiver,
            acks: Arc::clone(self),
        }
    }

    /// Answer one in-flight open. False when nothing was waiting — the caller
    /// gave up, or this is a second acknowledgement of the same request.
    fn resolve(&self, request_id: &str, ack: DesktopViewAck) -> bool {
        let Some(sender) = self.lock().remove(request_id) else {
            return false;
        };
        sender.send(ack).is_ok()
    }

    fn forget(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<DesktopViewAck>>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct DesktopViewAckRequest {
    request_id: String,
    opened: bool,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DesktopViewAckResponse {
    /// False when no caller was still waiting: the open timed out, or this
    /// request was already answered.
    acknowledged: bool,
}

/// The window's answer. Loopback-only, like the command lane it answers: only
/// this machine's desktop can say what is on this machine's screen.
pub(super) async fn acknowledge_desktop_view(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Json(request): Json<DesktopViewAckRequest>,
) -> Json<DesktopViewAckResponse> {
    let acknowledged = state.desktop_view_acks().resolve(
        &request.request_id,
        DesktopViewAck {
            opened: request.opened,
            code: request.code,
            message: request.message,
        },
    );
    Json(DesktopViewAckResponse { acknowledged })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DesktopViewCommandsQuery {
    cursor: Option<u64>,
    stream_id: Option<String>,
    limit: Option<usize>,
    timeout_secs: Option<u64>,
}

/// Long-poll the desktop view command lane. Same cursor/streamId contract as
/// the transfer advisory lanes: a cursor is only meaningful inside the server
/// incarnation that issued it, and reading through one prunes it.
pub(super) async fn wait_desktop_view_commands(
    _access: DesktopLocalAccess,
    State(state): State<Arc<AppState>>,
    Query(query): Query<DesktopViewCommandsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    let timeout_secs = query
        .timeout_secs
        .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
        .clamp(1, MAX_WAIT_TIMEOUT_SECS);
    let batch = state
        .desktop_view_commands()
        .wait_for_events(
            query.cursor,
            query.stream_id.as_deref(),
            limit,
            Duration::from_secs(timeout_secs),
        )
        .await;
    Ok(Json(json!({
        "waitOutcome": if batch.events.is_empty() { "timeout" } else { "events" },
        "cursor": batch.cursor,
        "streamId": batch.stream_id,
        "events": batch.events,
        "hasMore": batch.has_more,
        "missedEvents": batch.missed_events,
    })))
}

#[cfg(test)]
mod tests {
    use super::{locate_diff_line, parse_hunk_header, patch_path_matches};

    // Written as joined lines rather than with `\`-continuations: a
    // continuation eats the next line's leading whitespace, which is exactly
    // the character that marks a context line in a patch.
    const PATCH: &str = concat!(
        "diff --git a/src/main.rs b/src/main.rs\n",
        "--- a/src/main.rs\n",
        "+++ b/src/main.rs\n",
        "@@ -10,4 +10,5 @@ fn main() {\n",
        " let kept = 1;\n",
        "-let removed = 2;\n",
        "+let added = 3;\n",
        "+let also_added = 4;\n",
        " let tail = 5;\n",
    );

    #[test]
    fn hunk_headers_give_both_starting_lines() {
        assert_eq!(parse_hunk_header("@@ -10,4 +12,5 @@ ctx"), Some((10, 12)));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ nonsense"), None);
    }

    #[test]
    fn patch_paths_ignore_the_a_and_b_prefixes_and_dev_null() {
        assert!(patch_path_matches("a/src/main.rs", "src/main.rs"));
        assert!(patch_path_matches(
            "b/src/main.rs\t2026-01-01",
            "src/main.rs"
        ));
        assert!(!patch_path_matches("/dev/null", "src/main.rs"));
        assert!(!patch_path_matches("a/src/other.rs", "src/main.rs"));
    }

    #[test]
    fn each_side_counts_only_the_lines_that_side_has() {
        // New side: 10 kept, 11 added, 12 also_added, 13 tail.
        assert_eq!(
            locate_diff_line(PATCH, "src/main.rs", "new", 12)
                .unwrap()
                .text,
            "let also_added = 4;"
        );
        let context = locate_diff_line(PATCH, "src/main.rs", "new", 10).unwrap();
        assert_eq!(context.kind, "context");
        assert_eq!((context.old_line, context.new_line), (Some(10), Some(10)));
        // Old side: 10 kept, 11 removed, 12 tail.
        assert_eq!(
            locate_diff_line(PATCH, "src/main.rs", "old", 11)
                .unwrap()
                .text,
            "let removed = 2;"
        );
    }

    #[test]
    fn a_line_outside_every_hunk_is_refused_rather_than_guessed_at() {
        let error = locate_diff_line(PATCH, "src/main.rs", "new", 900).unwrap_err();
        assert_eq!(error.code, "diff_target_not_found");
        let missing = locate_diff_line(PATCH, "src/other.rs", "new", 10).unwrap_err();
        assert_eq!(missing.code, "diff_target_not_found");
    }
}
