//! The wire contract for a task transfer payload.
//!
//! This is the sender/receiver agreement that used to live in
//! `apps/desktop/src/utils/taskTransfer.ts`. It moved with the orchestration:
//! the payload is now built by the source's `kanna-server` and validated by the
//! destination's, so no renderer has to be running at either end.
//!
//! Validation is deliberately strict and total. A payload arrives from another
//! machine, and everything downstream of it — repository acquisition, artifact
//! materialization, task creation — acts on what it says. Anything the receiver
//! cannot pin to a known provider contract is refused here rather than partly
//! applied later.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const TASK_INPUT_LEDGER_FILENAME: &str = "task-inputs.json";
const TASK_INPUT_LEDGER_VERSION: u8 = 1;

/// How the destination gets the repository the task lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepoAcquisitionMode {
    ReuseLocal,
    CloneRemote,
    BundleRepo,
    /// A complete task handoff: the repository bundle is applied even when
    /// the destination already has the repository, and the durable input
    /// ledger is part of the import contract. Older servers do not recognize
    /// this mode, so a rolling-version mismatch fails closed instead of
    /// accepting a payload while silently substituting main or dropping
    /// instructions.
    TaskBundle,
}

impl RepoAcquisitionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReuseLocal => "reuse-local",
            Self::CloneRemote => "clone-remote",
            Self::BundleRepo => "bundle-repo",
            Self::TaskBundle => "task-bundle",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "reuse-local" => Ok(Self::ReuseLocal),
            "clone-remote" => Ok(Self::CloneRemote),
            "bundle-repo" => Ok(Self::BundleRepo),
            "task-bundle" => Ok(Self::TaskBundle),
            other => Err(format!("unsupported repo acquisition mode {other}")),
        }
    }
}

// The shared `Session` prefix is the wire vocabulary, not redundancy: these
// variant names are what `rename_all` turns into the `session-*` strings a peer
// sends and `as_str`/`parse` round-trip, so dropping it would rename the
// protocol. The prefix also distinguishes them from the one artifact that is not
// session state — the repo bundle, which travels under `repo.bundle` rather than
// as a kind here.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferArtifactKind {
    SessionRollout,
    SessionArchive,
    SessionTranscript,
    /// One conversation, exported by the provider's own CLI. OpenCode keeps
    /// every session in a shared SQLite store rather than a per-session file,
    /// so neither copying a file nor archiving a directory describes it.
    SessionExport,
}

impl TransferArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionRollout => "session-rollout",
            Self::SessionArchive => "session-archive",
            Self::SessionTranscript => "session-transcript",
            Self::SessionExport => "session-export",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "session-rollout" => Ok(Self::SessionRollout),
            "session-archive" => Ok(Self::SessionArchive),
            "session-transcript" => Ok(Self::SessionTranscript),
            "session-export" => Ok(Self::SessionExport),
            other => Err(format!("unsupported transfer artifact kind {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferArtifactMaterialization {
    CopyFile,
    ExtractTarGz,
    /// Replayed through `opencode import` rather than placed under `$HOME`.
    ///
    /// The other two materializations write bytes through the
    /// `transfer_artifact` fence. This one writes nothing there: OpenCode owns
    /// its store and only its own CLI may write it, so an artifact carrying
    /// this materialization must never reach the filesystem fence at all.
    OpencodeImport,
}

impl TransferArtifactMaterialization {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CopyFile => "copy-file",
            Self::ExtractTarGz => "extract-tar-gz",
            Self::OpencodeImport => "opencode-import",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "copy-file" => Ok(Self::CopyFile),
            "extract-tar-gz" => Ok(Self::ExtractTarGz),
            "opencode-import" => Ok(Self::OpencodeImport),
            other => Err(format!(
                "unsupported transfer artifact materialization {other}"
            )),
        }
    }
}

/// The one filename an OpenCode session export may travel under.
pub const OPENCODE_SESSION_EXPORT_FILENAME: &str = "opencode-session.json";

/// The CLI-owned data directory `opencode import` writes into, recorded so the
/// payload still describes where session state lands. It is a description, not
/// an instruction: no code derives a destination from it, and `XDG_DATA_HOME`
/// can move the real directory elsewhere.
pub const OPENCODE_SESSION_DATA_DIR_HOME_REL_PATH: &str = ".local/share/opencode";

/// OpenCode session ids are `ses_` followed by base62 — not a uuid, unlike
/// every other provider Kanna resumes.
pub fn is_opencode_session_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("ses_") else {
        return false;
    };
    !rest.is_empty() && rest.len() <= 64 && rest.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferArtifactPayload {
    pub artifact_id: String,
    pub filename: String,
    pub provider: String,
    pub kind: TransferArtifactKind,
    pub home_rel_path: String,
    pub materialization: TransferArtifactMaterialization,
}

/// How the source session ended before its state was staged.
///
/// A PTY source is asked to wrap up and then to quit, by injected input
/// (`super::finalize`). Everything about that is the agent's to cooperate with:
/// it may never finish its turn, it may be parked on a permission prompt this
/// sequence deliberately will not answer, or it may not exit on the quit
/// command. None of those is rare enough to fail a transfer over and none is
/// quiet enough to swallow, so each is recorded here as a degradation and
/// carried to the receiver, which imports the task anyway and surfaces the
/// reason to the destination operator (`super::import`): the conversation
/// still crosses, and whoever now owns the task is told the handoff was not
/// clean. Refusing the import belongs to a *missing* artifact, below — a
/// promise the payload cannot back at all — not to a conversation that is
/// merely one turn short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferFinalizationState {
    pub cleanly_finalized: bool,
    pub degraded_reason: Option<String>,
}

/// Bounds what a peer can push into our persisted payload and our toasts.
const DEGRADED_REASON_MAX_CHARS: usize = 512;

impl TransferFinalizationState {
    pub fn clean() -> Self {
        Self {
            cleanly_finalized: true,
            degraded_reason: None,
        }
    }

    pub fn degraded(reason: String) -> Self {
        Self {
            cleanly_finalized: false,
            degraded_reason: Some(truncate_chars(reason, DEGRADED_REASON_MAX_CHARS)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferBundlePayload {
    pub artifact_id: String,
    pub filename: String,
    pub ref_name: Option<String>,
    /// Full source-side ref that names the immutable review base included in
    /// the bundle. TaskBundle requires this alongside `task.base_oid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferTaskPayload {
    pub cloud_task_id: String,
    pub source_peer_id: String,
    pub source_desktop_id: Option<String>,
    pub source_task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_task_id: Option<String>,
    pub resume_session_id: Option<String>,
    pub prompt: Option<String>,
    pub stage: String,
    pub branch: Option<String>,
    /// Exact committed source tip the destination must import and prove before
    /// it may acknowledge the transfer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    /// Exact source commit used as the transferred task's diff base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_oid: Option<String>,
    /// Immutable source workflow/context carried into the first destination
    /// preparation. These are snapshots, never local stage-run rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_stage_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_main_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_feedback: Option<String>,
    /// The task's workflow name. Emitted under both `workflow` (canonical)
    /// and `pipeline` (legacy) so a peer running either naming can import it;
    /// parsing accepts either key.
    #[serde(alias = "pipeline")]
    pub workflow: String,
    /// Legacy mirror of [`Self::workflow`], written on the wire only. Never
    /// read — [`parse_outgoing_transfer_payload`] resolves the pair, and the
    /// derived deserializer skips this key so `workflow`'s alias owns it.
    #[serde(rename = "pipeline", default, skip_deserializing)]
    pub legacy_pipeline: String,
    pub display_name: Option<String>,
    pub base_ref: Option<String>,
    pub agent_type: Option<String>,
    pub agent_provider: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRepoPayload {
    pub mode: RepoAcquisitionMode,
    pub remote_url: Option<String>,
    pub path: Option<String>,
    pub name: Option<String>,
    pub default_branch: Option<String>,
    pub bundle: Option<TransferBundlePayload>,
}

/// Out-of-band durable input ledger shipped with a task bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferInputLedgerPayload {
    pub artifact_id: String,
    pub filename: String,
    pub sha256: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransferInputLedger {
    version: u8,
    source_peer_id: String,
    source_task_id: String,
    inputs: Vec<TransferInputLedgerEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransferInputLedgerEntry {
    sequence: u64,
    source: String,
    stage: Option<String>,
    message: String,
    delivered_at: String,
    origin_peer_id: String,
    origin_task_id: String,
    origin_input_id: i64,
    origin_run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutgoingTransferPayload {
    pub target_peer_id: String,
    pub target_desktop_id: Option<String>,
    pub task: TransferTaskPayload,
    pub repo: TransferRepoPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ledger: Option<TransferInputLedgerPayload>,
    pub recovery: Option<crate::mobile_api::CreateTaskRecoverySnapshot>,
    #[serde(default)]
    pub artifacts: Vec<TransferArtifactPayload>,
    pub finalization: TransferFinalizationState,
}

/// The artifact each provider must ship for a resume to mean anything.
///
/// Claude's conversation is the cwd-keyed transcript, not the
/// `~/.claude/tasks/<id>` lock directory, so the transcript is the load-bearing
/// one. OpenCode keeps no per-session file at all — its conversations live in a
/// shared SQLite store — so its conversation ships as an `opencode export`.
/// Providers absent from this table keep no transferable session state, so for
/// them an empty artifact list is not a defect.
pub fn required_session_artifact_kind(
    agent_type: Option<&str>,
    agent_provider: Option<&str>,
    resume_session_id: Option<&str>,
) -> Option<TransferArtifactKind> {
    resume_session_id?;
    if agent_type != Some("pty") {
        return None;
    }
    match agent_provider? {
        "claude" => Some(TransferArtifactKind::SessionTranscript),
        "codex" => Some(TransferArtifactKind::SessionRollout),
        "copilot" => Some(TransferArtifactKind::SessionArchive),
        "opencode" => Some(TransferArtifactKind::SessionExport),
        _ => None,
    }
}

/// A transfer promised a resumable session and could not back it.
///
/// Typed rather than string-matched because both sides act on it: the source
/// fails the transfer instead of shipping an artifact-less payload, and the
/// receiver refuses the import instead of minting a fresh session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSessionArtifact(pub String);

impl std::fmt::Display for MissingSessionArtifact {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The base branch a destination task forks from.
///
/// A bundle carries the task's own branch, so the destination can fork from it
/// directly. Every other acquisition mode gives the destination a repository
/// that has never seen that branch, leaving the task's base ref as the only ref
/// both machines can be expected to share — and `None` rather than a guess when
/// there is not one, so the destination falls back to its own repo default
/// instead of forking from a ref that means something different here.
pub fn resolve_incoming_base_branch(payload: &OutgoingTransferPayload) -> Option<String> {
    if matches!(
        payload.repo.mode,
        RepoAcquisitionMode::BundleRepo | RepoAcquisitionMode::TaskBundle
    ) {
        return normalize_optional(payload.task.branch.as_deref())
            .or_else(|| normalize_optional(payload.task.base_ref.as_deref()));
    }
    normalize_optional(payload.task.base_ref.as_deref())
}

fn validate_hex(value: &str, lengths: &[usize], label: &str) -> Result<String, String> {
    if lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(value.to_string())
    } else {
        Err(format!("{label} is not a lowercase hexadecimal object id"))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Serializes the complete durable directive history. A row that has already
/// crossed a machine keeps its first origin, so a second transfer does not
/// rewrite history to claim the intermediate machine authored it.
pub fn encode_task_input_ledger(
    records: &[crate::db::TaskInputRecord],
    source_peer_id: &str,
    source_task_id: &str,
) -> Result<Vec<u8>, String> {
    let inputs = records
        .iter()
        .enumerate()
        .map(|(sequence, record)| {
            let origin = record.origin.clone().unwrap_or(crate::db::TaskInputOrigin {
                peer_id: source_peer_id.to_string(),
                task_id: source_task_id.to_string(),
                input_id: record.id,
                run_id: record.run_id.clone(),
            });
            TransferInputLedgerEntry {
                sequence: sequence as u64,
                source: record.source.clone(),
                stage: record.stage.clone(),
                message: record.message.clone(),
                delivered_at: record.delivered_at.clone(),
                origin_peer_id: origin.peer_id,
                origin_task_id: origin.task_id,
                origin_input_id: origin.input_id,
                origin_run_id: origin.run_id,
            }
        })
        .collect();
    serde_json::to_vec(&TransferInputLedger {
        version: TASK_INPUT_LEDGER_VERSION,
        source_peer_id: source_peer_id.to_string(),
        source_task_id: source_task_id.to_string(),
        inputs,
    })
    .map_err(|error| format!("failed to encode task input ledger: {error}"))
}

/// Verifies and decodes a fetched directive ledger before any destination task
/// exists. The digest binds sidecar artifact bytes to the finalized payload;
/// sequence and origin checks keep retries and later transfers deterministic.
pub fn decode_task_input_ledger(
    bytes: &[u8],
    metadata: &TransferInputLedgerPayload,
    source_peer_id: &str,
    source_task_id: &str,
) -> Result<Vec<crate::db::ImportedTaskInput>, String> {
    if sha256_hex(bytes) != metadata.sha256 {
        return Err("transferred task input ledger checksum does not match its payload".into());
    }
    let ledger: TransferInputLedger = serde_json::from_slice(bytes)
        .map_err(|error| format!("transferred task input ledger is invalid: {error}"))?;
    if ledger.version != TASK_INPUT_LEDGER_VERSION {
        return Err(format!(
            "unsupported task input ledger version {}",
            ledger.version
        ));
    }
    if ledger.source_peer_id != source_peer_id || ledger.source_task_id != source_task_id {
        return Err("transferred task input ledger source identity does not match its task".into());
    }
    if ledger.inputs.len() as u64 != metadata.count {
        return Err("transferred task input ledger count does not match its payload".into());
    }
    let mut seen_origins = HashSet::with_capacity(ledger.inputs.len());
    ledger
        .inputs
        .into_iter()
        .enumerate()
        .map(|(sequence, input)| {
            if input.sequence != sequence as u64 {
                return Err("transferred task input ledger is not in delivery order".into());
            }
            if !matches!(
                input.source.as_str(),
                "operator" | "manager" | "unspecified" | "engine" | "notify"
            ) {
                return Err(format!(
                    "transferred task input has unsupported source {}",
                    input.source
                ));
            }
            if input.origin_input_id <= 0
                || input.origin_peer_id.is_empty()
                || input.origin_task_id.is_empty()
                || input.delivered_at.is_empty()
            {
                return Err("transferred task input has incomplete origin provenance".into());
            }
            let origin_key = (
                input.origin_peer_id.clone(),
                input.origin_task_id.clone(),
                input.origin_input_id,
            );
            if !seen_origins.insert(origin_key) {
                return Err(
                    "transferred task input ledger contains duplicate origin identity".into(),
                );
            }
            Ok(crate::db::ImportedTaskInput {
                stage: input.stage,
                source: input.source,
                message: input.message,
                delivered_at: input.delivered_at,
                origin: crate::db::TaskInputOrigin {
                    peer_id: input.origin_peer_id,
                    task_id: input.origin_task_id,
                    input_id: input.origin_input_id,
                    run_id: input.origin_run_id,
                },
            })
        })
        .collect()
}

fn normalize_optional(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn truncate_chars(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    value.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))
}

fn required_string(
    record: &serde_json::Map<String, Value>,
    keys: &[&str],
    label: &str,
) -> Result<String, String> {
    for key in keys {
        if let Some(value) = record.get(*key).and_then(Value::as_str) {
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(label.to_string())
}

fn optional_string(record: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        record
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// Reads a field that is legitimately nullable, distinguishing "absent or
/// null" from "present but not a string" — the latter is a malformed payload,
/// not a missing value.
fn nullable_string(
    record: &serde_json::Map<String, Value>,
    keys: &[&str],
    label: &str,
) -> Result<Option<String>, String> {
    for key in keys {
        let Some(value) = record.get(*key) else {
            continue;
        };
        if value.is_null() {
            return Ok(None);
        }
        return match value.as_str() {
            Some(text) if !text.is_empty() => Ok(Some(text.to_string())),
            Some(_) => Ok(None),
            None => Err(label.to_string()),
        };
    }
    Ok(None)
}

/// One safe path component: no separators, no traversal, no control bytes.
///
/// Every artifact id and filename a peer sends ends up in a path, so this runs
/// before any of them is joined onto anything.
fn validate_component(value: &str, label: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 1024
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(|character| (character as u32) < 0x20)
    {
        return Err(format!("{label} must be one safe path component"));
    }
    Ok(value.to_string())
}

fn is_session_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    for length in [8usize, 4, 4, 4, 12] {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != length || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn is_claude_project_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

struct ArtifactContract {
    kind: TransferArtifactKind,
    materialization: TransferArtifactMaterialization,
    /// `None` means the path is receiver-computed and only checked for shape.
    exact_home_rel_path: Option<String>,
}

impl ArtifactContract {
    fn assert_home_rel_path(&self, value: &str, filename: &str) -> Result<String, String> {
        if let Some(expected) = &self.exact_home_rel_path {
            if value != expected {
                return Err(
                    "transfer artifact path does not match the provider session contract".into(),
                );
            }
            return Ok(value.to_string());
        }
        // A Claude transcript is keyed by the *source* session's cwd, so the
        // sender cannot name where it must land here. The field is checked for
        // shape and never used to place a file: the receiver derives its own
        // slug from its own worktree path.
        let slug = value
            .strip_prefix(".claude/projects/")
            .and_then(|rest| rest.strip_suffix(filename))
            .and_then(|slug| slug.strip_suffix('/'));
        match slug {
            Some(slug) if is_claude_project_slug(slug) => Ok(value.to_string()),
            _ => Err("transfer artifact path does not match the Claude transcript contract".into()),
        }
    }
}

/// The one shape each (provider, filename) pair is allowed to declare.
///
/// This is a security boundary, not a convenience: the filename decides the
/// contract, and the contract decides what the receiver will do with the bytes.
fn canonical_artifact_contract(
    provider: &str,
    resume_session_id: &str,
    filename: &str,
) -> Result<ArtifactContract, String> {
    let session_id = validate_component(resume_session_id, "transfer resume session id")?;
    match provider {
        "claude" if filename == format!("{session_id}.jsonl") => {
            if !is_session_uuid(&session_id) {
                return Err("transfer resume session id is not a Claude session uuid".into());
            }
            Ok(ArtifactContract {
                kind: TransferArtifactKind::SessionTranscript,
                materialization: TransferArtifactMaterialization::CopyFile,
                exact_home_rel_path: None,
            })
        }
        "claude" => {
            if filename != "claude-session.tar.gz" {
                return Err(
                    "transfer artifact filename does not match the Claude session contract".into(),
                );
            }
            Ok(ArtifactContract {
                kind: TransferArtifactKind::SessionArchive,
                materialization: TransferArtifactMaterialization::ExtractTarGz,
                exact_home_rel_path: Some(format!(".claude/tasks/{session_id}")),
            })
        }
        "opencode" => {
            if !is_opencode_session_id(&session_id) {
                return Err("transfer resume session id is not an OpenCode session id".into());
            }
            if filename != OPENCODE_SESSION_EXPORT_FILENAME {
                return Err(
                    "transfer artifact filename does not match the OpenCode session contract"
                        .into(),
                );
            }
            Ok(ArtifactContract {
                kind: TransferArtifactKind::SessionExport,
                materialization: TransferArtifactMaterialization::OpencodeImport,
                // Nothing is written to this path: `opencode import` owns its
                // store and the receiver never derives a destination from the
                // payload. The value is pinned anyway so a peer cannot smuggle
                // a path through the field.
                exact_home_rel_path: Some(OPENCODE_SESSION_DATA_DIR_HOME_REL_PATH.to_string()),
            })
        }
        "copilot" => {
            if filename != "copilot-session.tar.gz" {
                return Err(
                    "transfer artifact filename does not match the Copilot session contract".into(),
                );
            }
            Ok(ArtifactContract {
                kind: TransferArtifactKind::SessionArchive,
                materialization: TransferArtifactMaterialization::ExtractTarGz,
                exact_home_rel_path: Some(format!(".copilot/session-state/{session_id}")),
            })
        }
        "codex" => {
            validate_component(filename, "Codex rollout filename")?;
            let rollout = parse_codex_rollout_filename(filename, &session_id)?;
            Ok(ArtifactContract {
                kind: TransferArtifactKind::SessionRollout,
                materialization: TransferArtifactMaterialization::CopyFile,
                exact_home_rel_path: Some(format!(
                    ".codex/sessions/{}/{}/{}/{filename}",
                    rollout.0, rollout.1, rollout.2
                )),
            })
        }
        other => Err(format!(
            "transfer artifacts are unsupported for provider {other}"
        )),
    }
}

/// `rollout-YYYY-MM-DDT….-<session-id>.jsonl`, returning the date parts that
/// name the directory the receiver will place it in.
fn parse_codex_rollout_filename(
    filename: &str,
    session_id: &str,
) -> Result<(String, String, String), String> {
    let invalid =
        || "transfer artifact filename does not match the Codex rollout contract".to_string();
    let rest = filename.strip_prefix("rollout-").ok_or_else(invalid)?;
    let bytes = rest.as_bytes();
    if bytes.len() < 11 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return Err(invalid());
    }
    let (year, month, day) = (&rest[0..4], &rest[5..7], &rest[8..10]);
    let numeric = |value: &str| value.bytes().all(|byte| byte.is_ascii_digit());
    if !numeric(year) || !numeric(month) || !numeric(day) {
        return Err(invalid());
    }
    let month_value: u32 = month.parse().map_err(|_| invalid())?;
    let day_value: u32 = day.parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&month_value) || !(1..=31).contains(&day_value) {
        return Err(invalid());
    }
    if !filename.ends_with(&format!("-{session_id}.jsonl")) {
        return Err(invalid());
    }
    Ok((year.to_string(), month.to_string(), day.to_string()))
}

fn parse_artifacts(
    value: Option<&Value>,
    task_provider: &str,
    resume_session_id: Option<&str>,
) -> Result<Vec<TransferArtifactPayload>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| "artifacts must be an array".to_string())?;
    // A Claude PTY task ships two: the `~/.claude/tasks/<id>` session archive
    // and the conversation transcript. One artifact per kind, so neither can be
    // duplicated into a second destination.
    if entries.len() > 2 {
        return Err("at most two resume artifacts are supported".into());
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let resume_session_id =
        resume_session_id.ok_or_else(|| "artifact requires a resume session id".to_string())?;

    let mut seen_kinds = Vec::new();
    let mut seen_ids = Vec::new();
    let mut artifacts = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let record = object(entry, &format!("artifact {index}"))?;
        let provider = required_string(
            record,
            &["provider"],
            &format!("artifact {index} missing provider"),
        )?;
        if provider != task_provider {
            return Err("artifact provider does not match the task provider".into());
        }
        let artifact_id = validate_component(
            &required_string(
                record,
                &["artifact_id", "artifactId"],
                &format!("artifact {index} missing artifact id"),
            )?,
            "transfer artifact id",
        )?;
        let filename = validate_component(
            &required_string(
                record,
                &["filename"],
                &format!("artifact {index} missing filename"),
            )?,
            "transfer artifact filename",
        )?;
        if seen_ids.contains(&artifact_id) {
            return Err(format!("duplicate artifact id {artifact_id}"));
        }
        seen_ids.push(artifact_id.clone());

        let contract = canonical_artifact_contract(&provider, resume_session_id, &filename)?;
        let kind = TransferArtifactKind::parse(&required_string(
            record,
            &["kind"],
            &format!("artifact {index} missing kind"),
        )?)?;
        let materialization = match record.get("materialization") {
            None | Some(Value::Null) => contract.materialization,
            Some(_) => TransferArtifactMaterialization::parse(&required_string(
                record,
                &["materialization"],
                &format!("artifact {index} missing materialization"),
            )?)?,
        };
        let home_rel_path = required_string(
            record,
            &["home_rel_path", "homeRelPath"],
            &format!("artifact {index} missing home_rel_path"),
        )?;
        if kind != contract.kind || materialization != contract.materialization {
            return Err(
                "artifact kind, materialization, or path does not match the provider session contract"
                    .into(),
            );
        }
        if seen_kinds.contains(&kind) {
            return Err(format!("duplicate artifact kind {}", kind.as_str()));
        }
        seen_kinds.push(kind);
        artifacts.push(TransferArtifactPayload {
            artifact_id,
            home_rel_path: contract.assert_home_rel_path(&home_rel_path, &filename)?,
            filename,
            provider,
            kind,
            materialization,
        });
    }
    Ok(artifacts)
}

fn parse_finalization(value: Option<&Value>) -> Result<TransferFinalizationState, String> {
    // Senders predating this field report nothing; read that as clean rather
    // than inventing a degradation for every older peer.
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(TransferFinalizationState::clean());
    };
    let record = object(value, "finalization")?;
    let cleanly_finalized = record
        .get("cleanly_finalized")
        .or_else(|| record.get("cleanlyFinalized"))
        .and_then(Value::as_bool)
        .ok_or_else(|| "finalization.cleanly_finalized must be a boolean".to_string())?;
    let degraded_reason = nullable_string(
        record,
        &["degraded_reason", "degradedReason"],
        "finalization.degraded_reason must be a string or null",
    )?
    .map(|reason| truncate_chars(reason, DEGRADED_REASON_MAX_CHARS));
    Ok(TransferFinalizationState {
        cleanly_finalized,
        degraded_reason,
    })
}

fn parse_recovery(
    value: Option<&Value>,
) -> Result<Option<crate::mobile_api::CreateTaskRecoverySnapshot>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let snapshot: crate::mobile_api::CreateTaskRecoverySnapshot =
        serde_json::from_value(value.clone())
            .map_err(|error| format!("recovery payload is invalid: {error}"))?;
    snapshot.validate()?;
    Ok(Some(snapshot))
}

/// Validates a payload that arrived from another machine — or one this machine
/// persisted earlier and is about to act on again.
pub fn parse_outgoing_transfer_payload(value: &Value) -> Result<OutgoingTransferPayload, String> {
    let record = object(value, "transfer payload")?;
    let task = object(
        record
            .get("task")
            .ok_or_else(|| "transfer payload missing task".to_string())?,
        "transfer payload task",
    )?;
    let repo = object(
        record
            .get("repo")
            .ok_or_else(|| "transfer payload missing repo".to_string())?,
        "transfer payload repo",
    )?;

    let source_task_id = required_string(
        task,
        &["source_task_id", "sourceTaskId"],
        "task missing source_task_id",
    )?;
    let source_peer_id = required_string(
        task,
        &["source_peer_id", "sourcePeerId"],
        "task missing source_peer_id",
    )?;
    let agent_provider = required_string(
        task,
        &["agent_provider", "agentProvider"],
        "task missing agent_provider",
    )?;
    agent_provider
        .parse::<kanna_agent_protocol::AgentProvider>()
        .map_err(|_| "task has unsupported agent_provider".to_string())?;
    let resume_session_id = nullable_string(
        task,
        &["resume_session_id", "resumeSessionId"],
        "task resume_session_id must be a string or null",
    )?;
    // `pipeline` is the legacy spelling of `workflow` on this wire; a peer on
    // either naming must import.
    let workflow_name = required_string(
        task,
        &["workflow", "workflowName", "pipeline"],
        "task missing workflow",
    )?;
    let mode = RepoAcquisitionMode::parse(&required_string(repo, &["mode"], "repo missing mode")?)?;

    let bundle = match repo.get("bundle") {
        None | Some(Value::Null) => None,
        Some(bundle) => {
            let bundle = object(bundle, "repo bundle")?;
            let ref_name = nullable_string(
                bundle,
                &["ref_name", "refName"],
                "repo bundle ref_name must be a string or null",
            )?
            .map(|reference| {
                super::git::normalize_ref(Some(&reference))
                    .ok_or_else(|| "repo bundle ref_name is not a safe git ref".to_string())
            })
            .transpose()?;
            let base_ref_name = nullable_string(
                bundle,
                &["base_ref_name", "baseRefName"],
                "repo bundle base_ref_name must be a string or null",
            )?
            .map(|reference| {
                super::git::normalize_ref(Some(&reference))
                    .ok_or_else(|| "repo bundle base_ref_name is not a safe git ref".to_string())
            })
            .transpose()?;
            Some(TransferBundlePayload {
                artifact_id: validate_component(
                    &required_string(
                        bundle,
                        &["artifact_id", "artifactId"],
                        "repo bundle missing artifact id",
                    )?,
                    "transfer bundle artifact id",
                )?,
                filename: validate_component(
                    &required_string(bundle, &["filename"], "repo bundle missing filename")?,
                    "transfer bundle filename",
                )?,
                ref_name,
                base_ref_name,
            })
        }
    };
    if matches!(
        mode,
        RepoAcquisitionMode::BundleRepo | RepoAcquisitionMode::TaskBundle
    ) && bundle.is_none()
    {
        return Err(format!(
            "{} payload is missing bundle metadata",
            mode.as_str()
        ));
    }

    let input_ledger = match record.get("input_ledger") {
        None | Some(Value::Null) => None,
        Some(ledger) => {
            let ledger = object(ledger, "input ledger")?;
            let filename = validate_component(
                &required_string(ledger, &["filename"], "input ledger missing filename")?,
                "input ledger filename",
            )?;
            if filename != TASK_INPUT_LEDGER_FILENAME {
                return Err("input ledger filename does not match its contract".into());
            }
            Some(TransferInputLedgerPayload {
                artifact_id: validate_component(
                    &required_string(
                        ledger,
                        &["artifact_id", "artifactId"],
                        "input ledger missing artifact id",
                    )?,
                    "input ledger artifact id",
                )?,
                filename,
                sha256: validate_hex(
                    &required_string(ledger, &["sha256"], "input ledger missing sha256")?,
                    &[64],
                    "input ledger sha256",
                )?,
                count: ledger
                    .get("count")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "input ledger count must be an unsigned integer".to_string())?,
            })
        }
    };
    if mode == RepoAcquisitionMode::TaskBundle && input_ledger.is_none() {
        return Err("task-bundle payload is missing input ledger metadata".into());
    }
    if mode == RepoAcquisitionMode::TaskBundle
        && bundle
            .as_ref()
            .and_then(|bundle| bundle.ref_name.as_ref())
            .is_none()
    {
        return Err("task-bundle payload is missing its source ref".into());
    }
    if mode == RepoAcquisitionMode::TaskBundle
        && bundle
            .as_ref()
            .and_then(|bundle| bundle.base_ref_name.as_ref())
            .is_none()
    {
        return Err("task-bundle payload is missing its immutable base ref".into());
    }

    let artifacts = parse_artifacts(
        record.get("artifacts"),
        &agent_provider,
        resume_session_id.as_deref(),
    )?;
    let head_oid = nullable_string(
        task,
        &["head_oid", "headOid"],
        "task head_oid must be a string or null",
    )?
    .map(|oid| validate_hex(&oid, &[40, 64], "task head_oid"))
    .transpose()?;
    if mode == RepoAcquisitionMode::TaskBundle && head_oid.is_none() {
        return Err("task-bundle payload is missing the exact task head".into());
    }
    let base_oid = nullable_string(
        task,
        &["base_oid", "baseOid"],
        "task base_oid must be a string or null",
    )?
    .map(|oid| validate_hex(&oid, &[40, 64], "task base_oid"))
    .transpose()?;
    if mode == RepoAcquisitionMode::TaskBundle && base_oid.is_none() {
        return Err("task-bundle payload is missing the exact review base".into());
    }
    let workflow_definition = nullable_string(
        task,
        &["workflow_definition", "workflowDefinition"],
        "task workflow_definition must be a string or null",
    )?;
    if mode == RepoAcquisitionMode::TaskBundle && workflow_definition.is_none() {
        return Err("task-bundle payload is missing its pinned workflow definition".into());
    }

    Ok(OutgoingTransferPayload {
        target_peer_id: required_string(
            record,
            &["target_peer_id", "targetPeerId"],
            "transfer payload missing target_peer_id",
        )?,
        target_desktop_id: optional_string(record, &["target_desktop_id", "targetDesktopId"]),
        task: TransferTaskPayload {
            cloud_task_id: optional_string(task, &["cloud_task_id", "cloudTaskId"])
                .unwrap_or_else(|| source_task_id.clone()),
            source_peer_id,
            source_desktop_id: optional_string(task, &["source_desktop_id", "sourceDesktopId"]),
            source_task_id,
            local_task_id: nullable_string(
                task,
                &["local_task_id", "localTaskId"],
                "task local_task_id must be a string or null",
            )?,
            resume_session_id,
            prompt: nullable_string(task, &["prompt"], "task prompt must be a string or null")?,
            stage: required_string(task, &["stage"], "task missing stage")?,
            branch: nullable_string(task, &["branch"], "task branch must be a string or null")?,
            head_oid,
            base_oid,
            workflow_definition,
            previous_stage_result: nullable_string(
                task,
                &["previous_stage_result", "previousStageResult"],
                "task previous_stage_result must be a string or null",
            )?,
            previous_main_result: nullable_string(
                task,
                &["previous_main_result", "previousMainResult"],
                "task previous_main_result must be a string or null",
            )?,
            revision_feedback: nullable_string(
                task,
                &["revision_feedback", "revisionFeedback"],
                "task revision_feedback must be a string or null",
            )?,
            workflow: workflow_name.clone(),
            legacy_pipeline: workflow_name,
            display_name: nullable_string(
                task,
                &["display_name", "displayName"],
                "task display_name must be a string or null",
            )?,
            base_ref: nullable_string(
                task,
                &["base_ref", "baseRef"],
                "task base_ref must be a string or null",
            )?,
            agent_type: nullable_string(
                task,
                &["agent_type", "agentType"],
                "task agent_type must be a string or null",
            )?,
            agent_provider,
        },
        repo: TransferRepoPayload {
            mode,
            remote_url: nullable_string(
                repo,
                &["remote_url", "remoteUrl"],
                "repo remote_url must be a string or null",
            )?,
            path: nullable_string(repo, &["path"], "repo path must be a string or null")?,
            name: nullable_string(repo, &["name"], "repo name must be a string or null")?,
            default_branch: nullable_string(
                repo,
                &["default_branch", "defaultBranch"],
                "repo default_branch must be a string or null",
            )?,
            bundle,
        },
        input_ledger,
        recovery: parse_recovery(record.get("recovery"))?,
        artifacts,
        finalization: parse_finalization(record.get("finalization"))?,
    })
}

/// Round-trips a payload this machine built through the same validation a
/// receiver applies, so a payload that could not be imported is never
/// committed.
pub fn encode_outgoing_transfer_payload(
    payload: &OutgoingTransferPayload,
) -> Result<Value, String> {
    let encoded = serde_json::to_value(payload)
        .map_err(|error| format!("failed to encode transfer payload: {error}"))?;
    parse_outgoing_transfer_payload(&encoded)?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION_ID: &str = "364643cc-5e6d-48fc-86ca-ca7764380900";
    const OPENCODE_SESSION: &str = "ses_02645d9aaffeeOgwt2rbXIcTdp";

    fn payload_with(artifacts: Value) -> Value {
        json!({
            "target_peer_id": "peer-destination",
        "task": {
                "source_peer_id": "peer-source",
                "source_task_id": "task-source",
                "resume_session_id": SESSION_ID,
                "stage": "in progress",
                "pipeline": "single-reviewer",
                "agent_type": "pty",
                "agent_provider": "claude",
                "workflow_definition": "{\"stages\":[{\"name\":\"in progress\"}]}",
            },
            "repo": { "mode": "reuse-local", "path": "/repo" },
            "artifacts": artifacts,
        })
    }

    /// A peer on either naming must import, and every payload this machine
    /// emits must carry both keys so an older peer can read it.
    #[test]
    fn the_task_workflow_parses_from_either_key_and_re_encodes_under_both() {
        let legacy = parse_outgoing_transfer_payload(&payload_with(json!([])))
            .expect("legacy `pipeline` key should parse");
        assert_eq!(legacy.task.workflow, "single-reviewer");
        assert_eq!(legacy.task.legacy_pipeline, "single-reviewer");

        let mut canonical_value = payload_with(json!([]));
        let task = canonical_value
            .get_mut("task")
            .and_then(Value::as_object_mut)
            .expect("task object");
        task.remove("pipeline");
        task.insert("workflow".into(), json!("specialized-reviewers"));
        let canonical = parse_outgoing_transfer_payload(&canonical_value)
            .expect("canonical `workflow` key should parse");
        assert_eq!(canonical.task.workflow, "specialized-reviewers");

        let encoded = encode_outgoing_transfer_payload(&canonical).expect("re-encode");
        assert_eq!(encoded["task"]["workflow"], json!("specialized-reviewers"));
        assert_eq!(encoded["task"]["pipeline"], json!("specialized-reviewers"));
    }

    #[test]
    fn a_payload_without_a_workflow_under_either_key_is_refused() {
        let mut value = payload_with(json!([]));
        value
            .get_mut("task")
            .and_then(Value::as_object_mut)
            .expect("task object")
            .remove("pipeline");
        assert_eq!(
            parse_outgoing_transfer_payload(&value).unwrap_err(),
            "task missing workflow"
        );
    }

    #[test]
    fn a_claude_transcript_keeps_its_receiver_shaped_path_and_a_forged_one_is_refused() {
        let parsed = parse_outgoing_transfer_payload(&payload_with(json!([{
            "artifact_id": "t-1-claude-transcript",
            "filename": format!("{SESSION_ID}.jsonl"),
            "provider": "claude",
            "kind": "session-transcript",
            "materialization": "copy-file",
            "home_rel_path": format!(".claude/projects/-Users-x-repo/{SESSION_ID}.jsonl"),
        }])))
        .expect("a well-formed transcript artifact");
        assert_eq!(parsed.artifacts.len(), 1);
        assert_eq!(
            parsed.artifacts[0].kind,
            TransferArtifactKind::SessionTranscript
        );

        // The slug is the only variable part, and it is never used to place a
        // file — but a path that is not slug-shaped is a sender trying to name
        // a destination, which is refused outright.
        for forged in [
            format!(".claude/projects/../../{SESSION_ID}.jsonl"),
            format!(".ssh/{SESSION_ID}.jsonl"),
            format!(".claude/projects/a/b/{SESSION_ID}.jsonl"),
        ] {
            let error = parse_outgoing_transfer_payload(&payload_with(json!([{
                "artifact_id": "t-1-claude-transcript",
                "filename": format!("{SESSION_ID}.jsonl"),
                "provider": "claude",
                "kind": "session-transcript",
                "materialization": "copy-file",
                "home_rel_path": forged,
            }])))
            .expect_err("a forged transcript path was accepted");
            assert!(error.contains("Claude transcript contract"), "{error}");
        }
    }

    #[test]
    fn an_artifact_that_disagrees_with_its_filename_contract_is_refused() {
        let error = parse_outgoing_transfer_payload(&payload_with(json!([{
            "artifact_id": "t-1-claude-session",
            "filename": "claude-session.tar.gz",
            "provider": "claude",
            // The archive contract is extract-tar-gz; claiming copy-file would
            // land an archive verbatim where a directory is expected.
            "kind": "session-archive",
            "materialization": "copy-file",
            "home_rel_path": format!(".claude/tasks/{SESSION_ID}"),
        }])))
        .expect_err("a contract-violating materialization was accepted");
        assert!(error.contains("provider session contract"), "{error}");
    }

    #[test]
    fn two_artifacts_of_one_kind_cannot_target_two_destinations() {
        let artifact = |artifact_id: &str| {
            json!({
                "artifact_id": artifact_id,
                "filename": format!("{SESSION_ID}.jsonl"),
                "provider": "claude",
                "kind": "session-transcript",
                "materialization": "copy-file",
                "home_rel_path": format!(".claude/projects/-a/{SESSION_ID}.jsonl"),
            })
        };
        let error =
            parse_outgoing_transfer_payload(&payload_with(json!([artifact("a"), artifact("b")])))
                .expect_err("duplicate kinds were accepted");
        assert!(error.contains("duplicate artifact kind"), "{error}");
    }

    #[test]
    fn a_codex_rollout_is_placed_only_by_the_date_encoded_in_its_own_filename() {
        let filename = format!("rollout-2026-08-07T10-11-12-{SESSION_ID}.jsonl");
        let mut payload = payload_with(json!([{
            "artifact_id": "t-1-codex-rollout",
            "filename": filename,
            "provider": "codex",
            "kind": "session-rollout",
            "materialization": "copy-file",
            "home_rel_path": format!(".codex/sessions/2026/08/07/{filename}"),
        }]));
        payload["task"]["agent_provider"] = json!("codex");
        let parsed = parse_outgoing_transfer_payload(&payload).expect("a codex rollout");
        assert_eq!(
            parsed.artifacts[0].home_rel_path,
            format!(".codex/sessions/2026/08/07/{filename}")
        );

        payload["artifacts"][0]["home_rel_path"] =
            json!(format!(".codex/sessions/2020/01/01/{filename}"));
        let error = parse_outgoing_transfer_payload(&payload)
            .expect_err("a rollout landed somewhere its filename does not name");
        assert!(error.contains("provider session contract"), "{error}");
    }

    #[test]
    fn a_bundle_repo_payload_without_bundle_metadata_is_refused() {
        let mut payload = payload_with(json!([]));
        payload["repo"] = json!({ "mode": "bundle-repo" });
        let error = parse_outgoing_transfer_payload(&payload).expect_err("bundle metadata missing");
        assert!(error.contains("bundle metadata"), "{error}");
    }

    #[test]
    fn task_bundle_requires_exact_head_bundle_ref_and_input_ledger() {
        let mut value = payload_with(json!([]));
        value["repo"] = json!({
            "mode": "task-bundle",
            "bundle": {
                "artifact_id": "transfer-repo-bundle",
                "filename": "transfer.bundle",
                "ref_name": "refs/heads/task-source",
                "base_ref_name": "refs/heads/main",
            },
        });
        assert!(parse_outgoing_transfer_payload(&value)
            .unwrap_err()
            .contains("input ledger"));

        value["input_ledger"] = json!({
            "artifact_id": "transfer-inputs",
            "filename": TASK_INPUT_LEDGER_FILENAME,
            "sha256": "a".repeat(64),
            "count": 0,
        });
        value["task"]["base_oid"] = json!("a".repeat(40));
        assert!(parse_outgoing_transfer_payload(&value)
            .unwrap_err()
            .contains("exact task head"));

        value["task"]["head_oid"] = json!("b".repeat(40));
        let parsed = parse_outgoing_transfer_payload(&value).expect("complete task bundle");
        assert_eq!(parsed.repo.mode, RepoAcquisitionMode::TaskBundle);
    }

    #[test]
    fn task_input_ledger_round_trip_keeps_order_attribution_and_first_origin() {
        let records = vec![
            crate::db::TaskInputRecord {
                id: 7,
                task_id: "task-source".into(),
                run_id: Some("run-one".into()),
                stage: Some("in progress".into()),
                source: "operator".into(),
                message: "first".into(),
                delivered_at: "2026-09-09 01:00:00".into(),
                origin: None,
            },
            crate::db::TaskInputRecord {
                id: 12,
                task_id: "task-source".into(),
                run_id: None,
                stage: Some("review".into()),
                source: "manager".into(),
                message: "second".into(),
                delivered_at: "2026-09-09 02:00:00".into(),
                origin: Some(crate::db::TaskInputOrigin {
                    peer_id: "peer-original".into(),
                    task_id: "task-original".into(),
                    input_id: 3,
                    run_id: Some("run-original".into()),
                }),
            },
        ];
        let bytes = encode_task_input_ledger(&records, "peer-source", "task-source")
            .expect("encode ledger");
        let metadata = TransferInputLedgerPayload {
            artifact_id: "inputs".into(),
            filename: TASK_INPUT_LEDGER_FILENAME.into(),
            sha256: sha256_hex(&bytes),
            count: 2,
        };
        let decoded = decode_task_input_ledger(&bytes, &metadata, "peer-source", "task-source")
            .expect("decode ledger");

        assert_eq!(
            decoded
                .iter()
                .map(|input| (
                    input.source.as_str(),
                    input.stage.as_deref(),
                    input.message.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("operator", Some("in progress"), "first"),
                ("manager", Some("review"), "second"),
            ]
        );
        assert_eq!(decoded[0].origin.peer_id, "peer-source");
        assert_eq!(decoded[0].origin.task_id, "task-source");
        assert_eq!(decoded[0].origin.input_id, 7);
        assert_eq!(decoded[0].origin.run_id.as_deref(), Some("run-one"));
        assert_eq!(decoded[1].origin.peer_id, "peer-original");
        assert_eq!(decoded[1].origin.task_id, "task-original");
        assert_eq!(decoded[1].origin.input_id, 3);
        assert_eq!(decoded[1].origin.run_id.as_deref(), Some("run-original"));
    }

    #[test]
    fn task_input_ledger_rejects_conflicting_duplicate_origins() {
        let entry = TransferInputLedgerEntry {
            sequence: 0,
            source: "manager".into(),
            stage: Some("review".into()),
            message: "directive".into(),
            delivered_at: "2026-09-09 01:00:00".into(),
            origin_peer_id: "peer-studio".into(),
            origin_task_id: "task-source".into(),
            origin_input_id: 7,
            origin_run_id: Some("run-1".into()),
        };
        let duplicate = TransferInputLedgerEntry {
            sequence: 1,
            message: "different directive".into(),
            ..entry.clone()
        };
        let bytes = serde_json::to_vec(&TransferInputLedger {
            version: TASK_INPUT_LEDGER_VERSION,
            source_peer_id: "peer-studio".into(),
            source_task_id: "task-source".into(),
            inputs: vec![entry, duplicate],
        })
        .expect("ledger json");
        let metadata = TransferInputLedgerPayload {
            artifact_id: "ledger".into(),
            filename: TASK_INPUT_LEDGER_FILENAME.into(),
            sha256: sha256_hex(&bytes),
            count: 2,
        };
        let error = decode_task_input_ledger(&bytes, &metadata, "peer-studio", "task-source")
            .expect_err("duplicate origin must be refused");
        assert!(error.contains("duplicate origin"), "{error}");
    }

    #[test]
    fn bundle_identifiers_may_not_escape_their_directory() {
        let mut payload = payload_with(json!([]));
        payload["repo"] = json!({
            "mode": "bundle-repo",
            "bundle": { "artifact_id": "../escape", "filename": "b.bundle" },
        });
        let error = parse_outgoing_transfer_payload(&payload).expect_err("path escape accepted");
        assert!(error.contains("safe path component"), "{error}");
    }

    #[test]
    fn required_artifact_kinds_track_provider_and_session_state() {
        assert_eq!(
            required_session_artifact_kind(Some("pty"), Some("claude"), Some(SESSION_ID)),
            Some(TransferArtifactKind::SessionTranscript)
        );
        assert_eq!(
            required_session_artifact_kind(Some("pty"), Some("codex"), Some(SESSION_ID)),
            Some(TransferArtifactKind::SessionRollout)
        );
        // No session ever ran, the task is not a PTY, or the provider keeps
        // nothing transferable: an empty artifact list is the truth, not a bug.
        assert_eq!(
            required_session_artifact_kind(Some("pty"), Some("claude"), None),
            None
        );
        assert_eq!(
            required_session_artifact_kind(Some("agent"), Some("claude"), Some(SESSION_ID)),
            None
        );
        // OpenCode's conversation lives in a shared SQLite store, so it ships
        // as an export rather than a file or a directory — but it does ship,
        // and a promise of one that arrives empty is the same defect.
        assert_eq!(
            required_session_artifact_kind(Some("pty"), Some("opencode"), Some(OPENCODE_SESSION)),
            Some(TransferArtifactKind::SessionExport)
        );
        assert_eq!(
            required_session_artifact_kind(Some("agent"), Some("opencode"), Some(OPENCODE_SESSION)),
            None
        );
        // A provider with genuinely nothing transferable is still an absence,
        // not a defect.
        assert_eq!(
            required_session_artifact_kind(Some("pty"), Some("antigravity"), Some(SESSION_ID)),
            None
        );
    }

    /// OpenCode ids are `ses_` plus base62 — not uuids, which every other
    /// provider Kanna resumes uses. A uuid here would mean the wrong provider's
    /// id reached an OpenCode contract.
    #[test]
    fn opencode_session_ids_are_recognized_by_their_own_shape() {
        for valid in [
            "ses_02645d9aaffeeOgwt2rbXIcTdp",
            "ses_a",
            &format!("ses_{}", "a".repeat(64)),
        ] {
            assert!(is_opencode_session_id(valid), "{valid}");
        }
        for invalid in [
            SESSION_ID,
            "ses_",
            "ses-02645d9",
            "02645d9aaffeeOgwt2rbXIcTdp",
            "ses_has-a-dash",
            &format!("ses_{}", "a".repeat(65)),
        ] {
            assert!(!is_opencode_session_id(invalid), "{invalid}");
        }
    }

    /// The one filename, the one pinned path, and a materialization that never
    /// reaches the filesystem fence.
    #[test]
    fn an_opencode_export_is_pinned_to_its_contract_and_refuses_anything_else() {
        let mut payload = payload_with(json!([{
            "artifact_id": "t-1-opencode-session",
            "filename": OPENCODE_SESSION_EXPORT_FILENAME,
            "provider": "opencode",
            "kind": "session-export",
            "materialization": "opencode-import",
            "home_rel_path": OPENCODE_SESSION_DATA_DIR_HOME_REL_PATH,
        }]));
        payload["task"]["agent_provider"] = json!("opencode");
        payload["task"]["resume_session_id"] = json!(OPENCODE_SESSION);
        let parsed = parse_outgoing_transfer_payload(&payload).expect("a valid export artifact");
        assert_eq!(
            parsed.artifacts[0].kind,
            TransferArtifactKind::SessionExport
        );
        assert_eq!(
            parsed.artifacts[0].materialization,
            TransferArtifactMaterialization::OpencodeImport
        );

        // A peer cannot smuggle a path through the field that only describes
        // where OpenCode's own store lives.
        payload["artifacts"][0]["home_rel_path"] = json!(".ssh/authorized_keys");
        let error = parse_outgoing_transfer_payload(&payload)
            .expect_err("a forged export destination was accepted");
        assert!(error.contains("provider session contract"), "{error}");

        // And the export travels under exactly one name.
        payload["artifacts"][0]["home_rel_path"] = json!(OPENCODE_SESSION_DATA_DIR_HOME_REL_PATH);
        payload["artifacts"][0]["filename"] = json!("something-else.json");
        let error = parse_outgoing_transfer_payload(&payload)
            .expect_err("an off-contract export filename was accepted");
        assert!(error.contains("OpenCode session contract"), "{error}");

        // A uuid is not an OpenCode session id, so it cannot open this arm.
        payload["artifacts"][0]["filename"] = json!(OPENCODE_SESSION_EXPORT_FILENAME);
        payload["task"]["resume_session_id"] = json!(SESSION_ID);
        let error = parse_outgoing_transfer_payload(&payload)
            .expect_err("a non-OpenCode session id was accepted");
        assert!(error.contains("OpenCode session id"), "{error}");
    }

    #[test]
    fn a_degraded_reason_from_a_peer_is_bounded() {
        let mut payload = payload_with(json!([]));
        payload["finalization"] = json!({
            "cleanly_finalized": false,
            "degraded_reason": "x".repeat(4096),
        });
        let parsed = parse_outgoing_transfer_payload(&payload).expect("degraded finalization");
        assert!(!parsed.finalization.cleanly_finalized);
        assert_eq!(
            parsed.finalization.degraded_reason.as_deref().map(str::len),
            Some(DEGRADED_REASON_MAX_CHARS)
        );
    }

    /// A sender predating the field reports nothing, which must read as clean
    /// rather than as a degradation invented for every older peer.
    #[test]
    fn an_absent_finalization_state_reads_as_clean() {
        let parsed = parse_outgoing_transfer_payload(&payload_with(json!([])))
            .expect("payload without finalization");
        assert_eq!(parsed.finalization, TransferFinalizationState::clean());
    }

    #[test]
    fn a_payload_this_machine_builds_is_validated_before_it_is_committed() {
        let mut payload = parse_outgoing_transfer_payload(&payload_with(json!([])))
            .expect("a valid starting payload");
        assert!(encode_outgoing_transfer_payload(&payload).is_ok());
        payload.repo.mode = RepoAcquisitionMode::BundleRepo;
        let error = encode_outgoing_transfer_payload(&payload)
            .expect_err("an unimportable payload was committed");
        assert!(error.contains("bundle metadata"), "{error}");
    }

    /// Only a bundle carries the task's own branch, so only a bundle may fork
    /// from it. Every other mode hands the destination a repository that has
    /// never seen that branch.
    #[test]
    fn only_a_bundled_repo_forks_from_the_task_branch() {
        let mut payload = parse_outgoing_transfer_payload(&payload_with(json!([])))
            .expect("a valid starting payload");
        payload.task.branch = Some("task-1".into());
        payload.task.base_ref = Some("origin/main".into());

        assert_eq!(
            resolve_incoming_base_branch(&payload).as_deref(),
            Some("origin/main"),
            "a reused or cloned repo has no task-1 to fork from",
        );

        payload.repo.mode = RepoAcquisitionMode::BundleRepo;
        assert_eq!(
            resolve_incoming_base_branch(&payload).as_deref(),
            Some("task-1")
        );
        payload.task.branch = None;
        assert_eq!(
            resolve_incoming_base_branch(&payload).as_deref(),
            Some("origin/main"),
        );

        // No base ref and no branch means no answer, not the repo default: the
        // destination's own default is a better guess than a ref this payload
        // never named.
        payload.task.base_ref = None;
        payload.repo.default_branch = Some("main".into());
        assert_eq!(resolve_incoming_base_branch(&payload), None);
    }
}
