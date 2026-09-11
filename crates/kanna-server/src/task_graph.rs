use crate::db::Db;
use git2::Repository;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GraphCommit {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub author: String,
    pub timestamp: i64,
    pub parents: Vec<String>,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraph {
    pub task_id: String,
    pub commits: Vec<GraphCommit>,
    pub head_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskGraphError {
    TaskNotFound,
    WorkspaceUnavailable,
    Internal(String),
}

impl fmt::Display for TaskGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskNotFound => formatter.write_str("task not found"),
            Self::WorkspaceUnavailable => formatter.write_str("task workspace unavailable"),
            Self::Internal(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TaskGraphError {}

/// Reads the graph from the task owner's worktree. This deliberately resolves
/// the task id in the owner's database; callers must never pass a worktree
/// path from another machine across the desktop boundary.
pub fn read_task_graph(
    db: &Db,
    task_or_branch_id: &str,
    from_ref: Option<&str>,
) -> Result<TaskGraph, TaskGraphError> {
    let task_id = db
        .resolve_pipeline_item_id(task_or_branch_id)
        .map_err(|error| TaskGraphError::Internal(format!("db error: {error}")))?
        .ok_or(TaskGraphError::TaskNotFound)?;
    let worktree_path = db
        .get_task_worktree_path(&task_id)
        .map_err(|error| TaskGraphError::Internal(format!("db error: {error}")))?
        .ok_or(TaskGraphError::WorkspaceUnavailable)?;
    let root = Path::new(&worktree_path);
    if !root.is_absolute() || !root.is_dir() {
        return Err(TaskGraphError::WorkspaceUnavailable);
    }
    let repo =
        Repository::open(root).map_err(|error| TaskGraphError::Internal(error.to_string()))?;

    let mut ref_map: HashMap<git2::Oid, Vec<String>> = HashMap::new();
    for reference in repo
        .references()
        .map_err(|error| TaskGraphError::Internal(error.to_string()))?
    {
        let Ok(reference) = reference else { continue };
        let Some(name) = reference.name() else {
            continue;
        };
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };
        let display = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/remotes/"))
            .or_else(|| name.strip_prefix("refs/tags/"));
        if let Some(display) = display {
            ref_map
                .entry(commit.id())
                .or_default()
                .push(display.to_string());
        }
    }
    let head_commit = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.id().to_string());
    let mut walk = repo
        .revwalk()
        .map_err(|error| TaskGraphError::Internal(error.to_string()))?;
    if let Some(from_ref) = from_ref {
        walk.push_ref(from_ref)
            .map_err(|error| TaskGraphError::Internal(error.to_string()))?;
    } else {
        walk.push_glob("refs/heads/*")
            .map_err(|error| TaskGraphError::Internal(error.to_string()))?;
        let _ = walk.push_glob("refs/remotes/*");
    }
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)
        .map_err(|error| TaskGraphError::Internal(error.to_string()))?;
    let mut commits = Vec::new();
    for oid in walk {
        let oid = oid.map_err(|error| TaskGraphError::Internal(error.to_string()))?;
        let commit = repo
            .find_commit(oid)
            .map_err(|error| TaskGraphError::Internal(error.to_string()))?;
        let hash = oid.to_string();
        commits.push(GraphCommit {
            short_hash: hash[..7.min(hash.len())].to_string(),
            hash,
            message: commit
                .message()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
            author: commit.author().name().unwrap_or("").to_string(),
            timestamp: commit.time().seconds(),
            parents: commit
                .parent_ids()
                .map(|parent| parent.to_string())
                .collect(),
            refs: ref_map.remove(&oid).unwrap_or_default(),
        });
    }
    Ok(TaskGraph {
        task_id,
        commits,
        head_commit,
    })
}
