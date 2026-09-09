//! The durable record of a provider refusing a task's turn for spent quota.
//!
//! A rejection is a *positive*, provider-stated observation, and it has to
//! outlive the terminal it was printed on for the same reason a delivered task
//! input does: the session dies at the next stage boundary, and a manager
//! reading the durable record afterwards would otherwise conclude the run
//! simply failed. Worse, without a record the recovery has no memory — the
//! rerun path feeds a run's recorded provider back in as an explicit override,
//! so a task rejected on one candidate is re-pinned to that candidate forever.
//!
//! One row per stage run per stated scope. That uniqueness is the whole
//! de-duplication story: the daemon latches its own announcement per session
//! incarnation, and a replayed or re-adopted announcement lands on the same
//! row instead of a second one.

use rusqlite::{params, OptionalExtension};

use super::Db;

/// What the server did about a rejection, decided once and recorded with it.
///
/// Deliberately not a status on the run or the task: a refused turn does not
/// make a live session dead, and inventing a runtime state for it is what the
/// quota consultation ruled out. This says what recovery happened, and every
/// value except `FallbackStarted` is a state a human has to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaRecovery {
    /// The next authorized candidate was started once, on the same task,
    /// stage, workspace and session.
    FallbackStarted,
    /// The ordered candidate list is exhausted: every candidate the pinned
    /// definition names has been rejected at this stage.
    ParkedNoCandidates,
    /// The rejection arrived after the attempt had already changed the
    /// workspace. Replaying that blind would discard or duplicate real work,
    /// so the task keeps its workspace and waits for a deliberate decision.
    ParkedWorkObserved,
    /// The run was started from an explicit single-provider override. An
    /// override is a caller's decision about which provider runs this stage,
    /// and quota recovery does not overrule one.
    ParkedOverrideBinding,
    /// The task's own definition names no ordered candidate list, so there is
    /// no next candidate to try. Recorded rather than silently ignored: the
    /// rejection is still the reason the task stopped making progress.
    ParkedNoCandidateList,
    /// A candidate was chosen but could not be started — the preparation was
    /// refused, the daemon was unreachable, or resolution walked back to the
    /// provider that had just refused. Distinct from the others because the
    /// cause is Kanna's, not the account's.
    ParkedFallbackFailed,
    /// The refusal arrived while the task was already being closed, rerun or
    /// advanced by somebody else. Recovery belongs to whoever holds that
    /// mutation, so the refusal is recorded and nothing else is done.
    ParkedConcurrentMutation,
}

impl QuotaRecovery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FallbackStarted => "fallback-started",
            Self::ParkedNoCandidates => "parked-no-candidates",
            Self::ParkedWorkObserved => "parked-work-observed",
            Self::ParkedOverrideBinding => "parked-override-binding",
            Self::ParkedNoCandidateList => "parked-no-candidate-list",
            Self::ParkedFallbackFailed => "parked-fallback-failed",
            Self::ParkedConcurrentMutation => "parked-concurrent-mutation",
        }
    }

    /// Whether this outcome leaves the task waiting for a person.
    pub fn is_parked(self) -> bool {
        !matches!(self, Self::FallbackStarted)
    }
}

/// Where the observation came from. Both are positive matches on something
/// the provider stated; they differ only in which surface it stated it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaRejectionSource {
    /// The CLI's rejection chrome, matched by a version-measured detection
    /// rule against the rendered terminal.
    Pty,
    /// The headless SDK's own `rate_limit_info.status == "rejected"`.
    Sdk,
}

impl QuotaRejectionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::Sdk => "sdk",
        }
    }
}

/// One observation, as it is written.
#[derive(Debug, Clone)]
pub struct NewProviderRejection<'a> {
    pub task_id: &'a str,
    pub stage_run_id: &'a str,
    pub stage: &'a str,
    pub provider: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub source: QuotaRejectionSource,
    pub rule_id: &'a str,
    /// The provider's own wording, so the claim can be checked against the
    /// pattern that produced it rather than believed.
    pub matched_text: &'a str,
    /// The scope the provider itself named. `None` means it named none — the
    /// claim is then about the account for that CLI, and still never about
    /// every model the provider offers.
    pub scope: Option<&'a str>,
    pub cli_version: Option<&'a str>,
    pub recovery: QuotaRecovery,
    /// The run the fallback started, when one did.
    pub replacement_run_id: Option<&'a str>,
}

/// One observation, as it is read back.
#[derive(Debug, Clone)]
pub struct ProviderRejection {
    pub id: i64,
    pub stage_run_id: String,
    pub stage: String,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub source: String,
    pub rule_id: String,
    pub matched_text: String,
    pub scope: Option<String>,
    pub cli_version: Option<String>,
    pub recovery: String,
    pub replacement_run_id: Option<String>,
    pub observed_at: String,
}

impl Db {
    /// Record one rejection, or report that this one is already recorded.
    ///
    /// `Ok(None)` means a row for this (run, provider, scope) already exists:
    /// the same refusal announced twice — by a daemon re-adopting the session,
    /// by a reconnecting watcher, by an SDK that repeats its rate-limit event
    /// — must not become two observations, and must certainly not become two
    /// fallback attempts.
    pub fn record_provider_rejection(
        &self,
        rejection: NewProviderRejection<'_>,
    ) -> Result<Option<ProviderRejection>, rusqlite::Error> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO task_provider_rejection (
                 task_id, stage_run_id, stage, provider, model, effort, source, rule_id,
                 matched_text, scope, cli_version, recovery, replacement_run_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                rejection.task_id,
                rejection.stage_run_id,
                rejection.stage,
                rejection.provider,
                rejection.model,
                rejection.effort,
                rejection.source.as_str(),
                rejection.rule_id,
                rejection.matched_text,
                rejection.scope.unwrap_or(""),
                rejection.cli_version,
                rejection.recovery.as_str(),
                rejection.replacement_run_id,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let id = self.conn.last_insert_rowid();
        self.provider_rejection(id)
    }

    fn provider_rejection(&self, id: i64) -> Result<Option<ProviderRejection>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT id, task_id, stage_run_id, stage, provider, model, effort, source,
                        rule_id, matched_text, scope, cli_version, recovery, replacement_run_id,
                        observed_at
                 FROM task_provider_rejection WHERE id = ?1",
                params![id],
                map_provider_rejection,
            )
            .optional()
    }

    /// Every rejection recorded for one task, oldest first.
    pub fn provider_rejections_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<ProviderRejection>, rusqlite::Error> {
        let mut statement = self.conn.prepare(
            "SELECT id, task_id, stage_run_id, stage, provider, model, effort, source,
                    rule_id, matched_text, scope, cli_version, recovery, replacement_run_id,
                    observed_at
             FROM task_provider_rejection WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let rows = statement.query_map(params![task_id], map_provider_rejection)?;
        rows.collect()
    }

    /// The providers already rejected at one stage of one task.
    ///
    /// This is what preserves the workflow's *original ordered intent* across
    /// the provider stamp: a rerun re-resolves the stage's candidate list and
    /// skips whatever is named here, instead of reproducing the run that was
    /// refused.
    pub fn providers_rejected_at_stage(
        &self,
        task_id: &str,
        stage: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT provider FROM task_provider_rejection
             WHERE task_id = ?1 AND stage = ?2 ORDER BY provider",
        )?;
        let rows = statement.query_map(params![task_id, stage], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// The most recent rejection recorded at one stage of one task.
    pub fn latest_provider_rejection_at_stage(
        &self,
        task_id: &str,
        stage: &str,
    ) -> Result<Option<ProviderRejection>, rusqlite::Error> {
        self.conn
            .query_row(
                "SELECT id, task_id, stage_run_id, stage, provider, model, effort, source,
                        rule_id, matched_text, scope, cli_version, recovery, replacement_run_id,
                        observed_at
                 FROM task_provider_rejection WHERE task_id = ?1 AND stage = ?2
                 ORDER BY id DESC LIMIT 1",
                params![task_id, stage],
                map_provider_rejection,
            )
            .optional()
    }
}

impl Db {
    /// Record what the recovery actually did, once it has run.
    ///
    /// The row is written before the attempt so a replay cannot start a
    /// second one; this is what makes it truthful afterwards.
    pub fn finish_provider_rejection_recovery(
        &self,
        id: i64,
        recovery: QuotaRecovery,
        replacement_run_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE task_provider_rejection SET recovery = ?2, replacement_run_id = ?3
             WHERE id = ?1",
            params![id, recovery.as_str(), replacement_run_id],
        )?;
        Ok(())
    }
}

fn map_provider_rejection(row: &rusqlite::Row<'_>) -> Result<ProviderRejection, rusqlite::Error> {
    let scope: String = row.get(10)?;
    Ok(ProviderRejection {
        id: row.get(0)?,
        stage_run_id: row.get(2)?,
        stage: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        effort: row.get(6)?,
        source: row.get(7)?,
        rule_id: row.get(8)?,
        matched_text: row.get(9)?,
        // Stored as `''` rather than NULL so the uniqueness constraint can
        // include it: SQLite treats NULLs as distinct, which would let the
        // same unscoped refusal insert twice.
        scope: (!scope.is_empty()).then_some(scope),
        cli_version: row.get(11)?,
        recovery: row.get(12)?,
        replacement_run_id: row.get(13)?,
        observed_at: row.get(14)?,
    })
}
