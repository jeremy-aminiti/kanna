//! Durable record for the human-assisted pull-request review path: what PR a
//! review task is actually about, and the human decisions that authorize the
//! merge singleton to merge it.
//!
//! Two rows, deliberately separate, because they answer different questions
//! and only one of them is authority.
//!
//! [`TaskReviewContext`] is *candidate information*. A `pr-review-single`
//! child forks its worktree from `pull/<n>/head` into a local `pr/<n>` ref, so
//! neither its `task-*` branch nor that local ref names anything the forge can
//! merge, and the task that produced the PR may live on another machine or not
//! exist at all. Without a durable projection of the PR's own identity, every
//! consumer downstream of the review session — the read-only client projection, the merge
//! master reading the request on another machine — would be reduced to parsing
//! terminal text or guessing from a branch name. An agent supplies it; that
//! makes it a claim about the forge, never an approval.
//!
//! [`HumanReviewDecision`] is the authority. It is created only by an explicit
//! operator instruction (relayed by its review agent) and is immutable once written:
//! the exact PR, head SHA and base it was taken against, the exact sentence the
//! human instructed, and when. Delivery outcome is recorded *beside* it rather
//! than in it, so a redelivery never rewrites what was decided. One decision
//! per (task, reviewed head): a repeated call on the same head resolves to the
//! same row, and a PR that moves needs a fresh read and a fresh decision.
//!
//! The declared `operator-relayed` (or retained direct `operator`) origin is
//! not cryptographic proof of human presence — a local agent runs as the same OS user and can reach this API.
//! It is the same declared-but-unverified model the input ledger and revision
//! origin already use, and it is honest about that: what the row proves is that
//! *this* decision, with this text, was recorded at this time against this
//! exact head.

use super::{Db, TaskEventKind};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// What a review task is reviewing, as the forge identifies it.
///
/// Every field except `pr_url`, `head_sha` and `base_ref` is optional because
/// a standalone review — one created without the triage dispatcher — knows
/// less than a dispatched child does, and refusing to record the part it knows
/// would leave the reviewer unable to identify the PR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewContextInput {
    /// Canonical pull-request URL.
    pub pr_url: String,
    /// `owner/name` of the repository the head branch lives in. Present and
    /// different from the base repository for a fork PR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repo: Option<String>,
    /// The PR's own head branch name — never the review task's `task-*`
    /// branch and never the local `pr/<n>` fetch ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<String>,
    /// The exact commit reviewed. A decision is pinned to this.
    pub head_sha: String,
    /// The branch the PR merges into.
    pub base_ref: String,
    /// The base commit the diff was read against, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// The Kanna task that produced the PR, when the reviewer could resolve
    /// one. Optional by construction: an external contributor's PR has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producing_task_id: Option<String>,
    /// The machine that owns `producing_task_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producing_machine_id: Option<String>,
    /// The triage task that dispatched this review, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_parent_task_id: Option<String>,
    /// Triage's position for this PR in its proposed read order. Durable
    /// advice; it is not an authorization list and it is not a queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_rank: Option<i64>,
    /// Other open PRs triage found touching overlapping files, or that this
    /// PR is stacked on. Shown to the operator before they decide and passed
    /// to the merge master as ordering advice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_pr_urls: Vec<String>,
}

/// A stored [`ReviewContextInput`] plus the version a decision pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskReviewContext {
    /// Bumped on every refresh. A decision records the version it was taken
    /// against, so a context refreshed under a pending decision is visible as
    /// a mismatch rather than silently adopted.
    pub version: i64,
    #[serde(flatten)]
    pub context: ReviewContextInput,
    pub updated_at: String,
}

/// Why a review context was refused.
#[derive(Debug)]
pub enum ReviewContextError {
    Invalid(String),
    Db(rusqlite::Error),
}

impl std::fmt::Display for ReviewContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "{reason}"),
            Self::Db(error) => write!(f, "db error: {error}"),
        }
    }
}

impl From<rusqlite::Error> for ReviewContextError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(error)
    }
}

fn trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn trimmed_opt(value: Option<&String>) -> Option<String> {
    value.and_then(|value| trimmed(value))
}

impl ReviewContextInput {
    /// Normalize and reject a context that cannot identify what was reviewed.
    ///
    /// A context missing the PR URL or the reviewed commit is worse than
    /// absent: the control would offer to queue a merge it cannot name, and
    /// the decision it produced could not be checked against anything.
    pub fn validated(&self) -> Result<Self, ReviewContextError> {
        let pr_url = trimmed(&self.pr_url).ok_or_else(|| {
            ReviewContextError::Invalid("review context prUrl must be non-empty".to_string())
        })?;
        if !(pr_url.starts_with("https://") || pr_url.starts_with("http://")) {
            return Err(ReviewContextError::Invalid(
                "review context prUrl must be an absolute http(s) URL".to_string(),
            ));
        }
        let head_sha = trimmed(&self.head_sha).ok_or_else(|| {
            ReviewContextError::Invalid("review context headSha must be non-empty".to_string())
        })?;
        if head_sha.len() < 7 || !head_sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ReviewContextError::Invalid(
                "review context headSha must be a hex commit id of at least 7 characters"
                    .to_string(),
            ));
        }
        let base_ref = trimmed(&self.base_ref).ok_or_else(|| {
            ReviewContextError::Invalid("review context baseRef must be non-empty".to_string())
        })?;
        let base_sha = trimmed_opt(self.base_sha.as_ref());
        if let Some(base_sha) = base_sha.as_deref() {
            if base_sha.len() < 7 || !base_sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ReviewContextError::Invalid(
                    "review context baseSha must be a hex commit id of at least 7 characters"
                        .to_string(),
                ));
            }
        }
        Ok(Self {
            pr_url,
            head_repo: trimmed_opt(self.head_repo.as_ref()),
            head_ref: trimmed_opt(self.head_ref.as_ref()),
            head_sha: head_sha.to_ascii_lowercase(),
            base_ref,
            base_sha: base_sha.map(|sha| sha.to_ascii_lowercase()),
            producing_task_id: trimmed_opt(self.producing_task_id.as_ref()),
            producing_machine_id: trimmed_opt(self.producing_machine_id.as_ref()),
            triage_parent_task_id: trimmed_opt(self.triage_parent_task_id.as_ref()),
            triage_rank: self.triage_rank,
            related_pr_urls: self
                .related_pr_urls
                .iter()
                .filter_map(|url| trimmed(url))
                .collect(),
        })
    }

    /// The head as the forge names it — `owner/name:branch` across a fork, the
    /// branch alone otherwise. This is what a merge request must carry: the
    /// review task's own branch names nothing that can be merged.
    pub fn qualified_head(&self) -> Option<String> {
        let head_ref = self.head_ref.as_deref()?;
        Some(match self.head_repo.as_deref() {
            Some(repo) => format!("{repo}:{head_ref}"),
            None => head_ref.to_string(),
        })
    }
}

/// How far a decision's delivery to the merge singleton got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecisionDelivery {
    /// Recorded, not yet handed to the merge agent.
    Pending,
    /// The merge agent's session acknowledged the request.
    Delivered,
    /// Delivery was refused before anything reached a terminal. Safe to
    /// retry: the same decision is redelivered, never a second one.
    Failed,
    /// Delivery stopped part-way or its acknowledgement was lost. **Never
    /// resent automatically** — the request may already be in the merge
    /// master's session, and a duplicate reads as a second authorization.
    Uncertain,
}

impl ReviewDecisionDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

/// An immutable record of a human authorizing the merge of one reviewed head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanReviewDecision {
    pub id: String,
    pub task_id: String,
    /// The review-context version this was decided against.
    pub review_context_version: i64,
    pub pr_url: String,
    /// The head the human actually read, as the forge names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub head_sha: String,
    pub base_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// The exact instruction the operator gave, stored verbatim.
    pub action_text: String,
    /// Declared, unverified: `operator-relayed` or retained direct `operator`.
    pub origin: String,
    /// The conversation channel and server-observed latest review run, or
    /// provenance carried by a retained direct request. Corroboration only,
    /// never proof of caller identity or human presence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_provenance: Option<serde_json::Value>,
    /// The machine the decision was taken on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_machine_id: Option<String>,
    pub created_at: String,
    /// Delivery state, recorded beside the decision and never inside it.
    pub delivery_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    /// The merge singleton task the request went to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_task_id: Option<String>,
    /// The desktop whose lifecycle owns that merge task — not always this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_desktop_id: Option<String>,
}

/// A decision to create, before it has an id or a delivery outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewHumanReviewDecision<'a> {
    pub task_id: &'a str,
    pub review_context_version: i64,
    pub pr_url: &'a str,
    pub head: Option<&'a str>,
    pub head_sha: &'a str,
    pub base_ref: &'a str,
    pub base_sha: Option<&'a str>,
    pub action_text: &'a str,
    pub origin: &'a str,
    pub device_provenance: Option<&'a serde_json::Value>,
    pub source_machine_id: Option<&'a str>,
}

impl Db {
    /// Store or refresh a task's review context, bumping its version.
    ///
    /// A refresh deliberately carries no decision forward: a decision names
    /// the version and head it was taken against, so re-reading the PR leaves
    /// any earlier decision visibly stale rather than silently re-applied.
    pub fn upsert_task_review_context(
        &self,
        task_id: &str,
        context: &ReviewContextInput,
    ) -> Result<TaskReviewContext, ReviewContextError> {
        let context = context.validated()?;
        self.with_immediate_transaction(|db| {
            let related = serde_json::to_string(&context.related_pr_urls)
                .unwrap_or_else(|_| "[]".to_string());
            db.conn.execute(
                "INSERT INTO task_review_context (
                     task_id, version, pr_url, head_repo, head_ref, head_sha, base_ref, base_sha,
                     producing_task_id, producing_machine_id, triage_parent_task_id, triage_rank,
                     related_pr_urls, updated_at
                 ) VALUES (?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
                 ON CONFLICT(task_id) DO UPDATE SET
                     version = version + 1,
                     pr_url = excluded.pr_url,
                     head_repo = excluded.head_repo,
                     head_ref = excluded.head_ref,
                     head_sha = excluded.head_sha,
                     base_ref = excluded.base_ref,
                     base_sha = excluded.base_sha,
                     producing_task_id = excluded.producing_task_id,
                     producing_machine_id = excluded.producing_machine_id,
                     triage_parent_task_id = excluded.triage_parent_task_id,
                     triage_rank = excluded.triage_rank,
                     related_pr_urls = excluded.related_pr_urls,
                     updated_at = datetime('now')",
                params![
                    task_id,
                    context.pr_url,
                    context.head_repo,
                    context.head_ref,
                    context.head_sha,
                    context.base_ref,
                    context.base_sha,
                    context.producing_task_id,
                    context.producing_machine_id,
                    context.triage_parent_task_id,
                    context.triage_rank,
                    related,
                ],
            )?;
            let stored = db
                .read_task_review_context(task_id)?
                .ok_or_else(|| ReviewContextError::Db(rusqlite::Error::QueryReturnedNoRows))?;
            db.append_task_event(
                task_id,
                TaskEventKind::ReviewContextChanged,
                json!({
                    "version": stored.version,
                    "prUrl": stored.context.pr_url,
                    "headSha": stored.context.head_sha,
                    "baseRef": stored.context.base_ref,
                }),
            )?;
            Ok(stored)
        })
    }

    /// The task's review context, or `None` when it has none. An absent
    /// context is what makes a review task un-queueable: the control has no
    /// PR identity to offer and nothing to check a decision against.
    pub fn read_task_review_context(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskReviewContext>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT version, pr_url, head_repo, head_ref, head_sha, base_ref, base_sha,
                        producing_task_id, producing_machine_id, triage_parent_task_id,
                        triage_rank, related_pr_urls, updated_at
                 FROM task_review_context WHERE task_id = ?",
                [task_id],
                |row| {
                    let related: Option<String> = row.get(11)?;
                    Ok(TaskReviewContext {
                        version: row.get(0)?,
                        context: ReviewContextInput {
                            pr_url: row.get(1)?,
                            head_repo: row.get(2)?,
                            head_ref: row.get(3)?,
                            head_sha: row.get(4)?,
                            base_ref: row.get(5)?,
                            base_sha: row.get(6)?,
                            producing_task_id: row.get(7)?,
                            producing_machine_id: row.get(8)?,
                            triage_parent_task_id: row.get(9)?,
                            triage_rank: row.get(10)?,
                            related_pr_urls: related
                                .and_then(|raw| serde_json::from_str(&raw).ok())
                                .unwrap_or_default(),
                        },
                        updated_at: row.get(12)?,
                    })
                },
            )
            .optional()
    }

    /// Record a human's merge authorization for one reviewed head, or return
    /// the decision that already exists for it.
    ///
    /// Idempotent on `(task_id, head_sha)` so a duplicate call, a retried
    /// request, or a client that lost the response resolves to the same
    /// decision instead of manufacturing a second authorization. The returned
    /// flag says whether this call created it, which is what tells a caller
    /// whether it may deliver or is looking at an already-delivered one.
    pub fn record_human_review_decision(
        &self,
        decision: NewHumanReviewDecision<'_>,
    ) -> Result<(HumanReviewDecision, bool), rusqlite::Error> {
        self.with_immediate_transaction(|db| {
            if let Some(existing) =
                db.read_human_review_decision_for_head(decision.task_id, decision.head_sha)?
            {
                return Ok((existing, false));
            }
            let id: String =
                db.conn
                    .query_row("SELECT 'hrd-' || lower(hex(randomblob(16)))", [], |row| {
                        row.get(0)
                    })?;
            let provenance = decision
                .device_provenance
                .map(|value| value.to_string())
                .filter(|value| value != "null");
            db.conn.execute(
                "INSERT INTO human_review_decision (
                     id, task_id, review_context_version, pr_url, head, head_sha, base_ref,
                     base_sha, action_text, origin, device_provenance, source_machine_id,
                     created_at, delivery_status
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), ?)",
                params![
                    id,
                    decision.task_id,
                    decision.review_context_version,
                    decision.pr_url,
                    decision.head,
                    decision.head_sha,
                    decision.base_ref,
                    decision.base_sha,
                    decision.action_text,
                    decision.origin,
                    provenance,
                    decision.source_machine_id,
                    ReviewDecisionDelivery::Pending.as_str(),
                ],
            )?;
            db.append_task_event(
                decision.task_id,
                TaskEventKind::HumanReviewDecisionRecorded,
                json!({
                    "decisionId": id,
                    "prUrl": decision.pr_url,
                    "headSha": decision.head_sha,
                    "baseRef": decision.base_ref,
                    "origin": decision.origin,
                    "reviewContextVersion": decision.review_context_version,
                }),
            )?;
            let stored = db
                .read_human_review_decision(&id)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            Ok((stored, true))
        })
    }

    pub fn read_human_review_decision(
        &self,
        id: &str,
    ) -> Result<Option<HumanReviewDecision>, rusqlite::Error> {
        self.query_human_review_decision("id = ?", id)
    }

    /// The decision for one reviewed head, which is the only shape a duplicate
    /// request can legitimately resolve to.
    pub fn read_human_review_decision_for_head(
        &self,
        task_id: &str,
        head_sha: &str,
    ) -> Result<Option<HumanReviewDecision>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT id FROM human_review_decision WHERE task_id = ? AND head_sha = ?",
                params![task_id, head_sha],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| self.read_human_review_decision(&id))
            .transpose()
            .map(Option::flatten)
    }

    /// The task's most recent decision, for task detail. Older decisions stay
    /// in the table: a PR that moved and was re-reviewed has a history, and
    /// collapsing it would make the newest read as the only one ever taken.
    pub fn latest_human_review_decision(
        &self,
        task_id: &str,
    ) -> Result<Option<HumanReviewDecision>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT id FROM human_review_decision
                 WHERE task_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| self.read_human_review_decision(&id))
            .transpose()
            .map(Option::flatten)
    }

    /// How many distinct authorizations exist for a task. A retry must never
    /// move this: one human decision per reviewed head is the invariant the
    /// merge queue depends on.
    #[cfg(test)]
    pub fn count_test_human_review_decisions(&self, task_id: &str) -> Result<i64, rusqlite::Error> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM human_review_decision WHERE task_id = ?",
            [task_id],
            |row| row.get(0),
        )
    }

    fn query_human_review_decision(
        &self,
        predicate: &str,
        value: &str,
    ) -> Result<Option<HumanReviewDecision>, rusqlite::Error> {
        self.conn
            .query_row(
                &format!(
                    "SELECT id, task_id, review_context_version, pr_url, head, head_sha, base_ref,
                            base_sha, action_text, origin, device_provenance, source_machine_id,
                            created_at, delivery_status, delivery_detail, delivered_at,
                            merge_task_id, owner_desktop_id
                     FROM human_review_decision WHERE {predicate}"
                ),
                [value],
                |row| {
                    let provenance: Option<String> = row.get(10)?;
                    Ok(HumanReviewDecision {
                        id: row.get(0)?,
                        task_id: row.get(1)?,
                        review_context_version: row.get(2)?,
                        pr_url: row.get(3)?,
                        head: row.get(4)?,
                        head_sha: row.get(5)?,
                        base_ref: row.get(6)?,
                        base_sha: row.get(7)?,
                        action_text: row.get(8)?,
                        origin: row.get(9)?,
                        device_provenance: provenance
                            .and_then(|raw| serde_json::from_str(&raw).ok()),
                        source_machine_id: row.get(11)?,
                        created_at: row.get(12)?,
                        delivery_status: row.get(13)?,
                        delivery_detail: row.get(14)?,
                        delivered_at: row.get(15)?,
                        merge_task_id: row.get(16)?,
                        owner_desktop_id: row.get(17)?,
                    })
                },
            )
            .optional()
    }

    /// Record how far a decision's delivery got. The decision itself is never
    /// rewritten; only this outcome moves.
    pub fn record_human_review_decision_delivery(
        &self,
        id: &str,
        status: ReviewDecisionDelivery,
        detail: Option<&str>,
        merge_task_id: Option<&str>,
        owner_desktop_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.with_immediate_transaction(|db| {
            let delivered_at = matches!(status, ReviewDecisionDelivery::Delivered)
                .then_some("datetime('now')")
                .unwrap_or("delivered_at");
            db.conn.execute(
                &format!(
                    "UPDATE human_review_decision
                     SET delivery_status = ?, delivery_detail = ?, merge_task_id = ?,
                         owner_desktop_id = ?, delivered_at = {delivered_at}
                     WHERE id = ?"
                ),
                params![status.as_str(), detail, merge_task_id, owner_desktop_id, id],
            )?;
            let Some(decision) = db.read_human_review_decision(id)? else {
                return Ok(());
            };
            db.append_task_event(
                &decision.task_id,
                TaskEventKind::HumanReviewDecisionDelivery,
                json!({
                    "decisionId": id,
                    "status": status.as_str(),
                    "detail": detail,
                    "prUrl": decision.pr_url,
                    "headSha": decision.head_sha,
                    "mergeTaskId": merge_task_id,
                    "ownerDesktopId": owner_desktop_id,
                }),
            )?;
            Ok(())
        })
    }
}
