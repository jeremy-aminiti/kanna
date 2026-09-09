use super::events::RuntimeError;
use super::state::{PendingTaskPullRequest, TransferRuntime};
use super::utils::unexpected_peer_response;
use super::TransferTransport;
use crate::protocol::{PeerRequest, PeerResponse};
use std::time::{Duration, Instant};

pub(super) const TASK_PULL_REQUEST_TTL: Duration = Duration::from_secs(5 * 60);

pub(super) fn validate_source_task_id(source_task_id: &str) -> Result<(), RuntimeError> {
    if source_task_id.trim().is_empty() {
        return Err(RuntimeError::Protocol(
            "source task ID must not be blank".into(),
        ));
    }
    if source_task_id.len() > 1024 {
        return Err(RuntimeError::Protocol(format!(
            "source task ID exceeds 1024 UTF-8 bytes (received {})",
            source_task_id.len()
        )));
    }
    if source_task_id.chars().any(char::is_control) {
        return Err(RuntimeError::Protocol(
            "source task ID contains a control character".into(),
        ));
    }
    Ok(())
}

/// Longest refusal text a peer may put into this machine's database and
/// toasts. The reason is diagnostic prose written by the *other* machine, so
/// it is bounded on arrival rather than wherever it is finally rendered.
pub(super) const MAX_REFUSAL_REASON_CHARS: usize = 512;

pub(super) fn truncate_refusal_reason(reason: String) -> String {
    match reason.char_indices().nth(MAX_REFUSAL_REASON_CHARS) {
        Some((index, _)) => format!("{}…", &reason[..index]),
        None => reason,
    }
}

/// A pull request id is minted by the peer as `pull-<peer-id>-<counter>`, and
/// the recipient builds durable primary keys out of it — the work-queue id and
/// the transfer id. Bounded and free of control characters for the same reason
/// [`validate_source_task_id`] is.
pub(super) fn validate_pull_request_id(pull_request_id: &str) -> Result<(), RuntimeError> {
    if pull_request_id.trim().is_empty() {
        return Err(RuntimeError::Protocol(
            "pull request ID must not be blank".into(),
        ));
    }
    if pull_request_id.len() > 256 {
        return Err(RuntimeError::Protocol(format!(
            "pull request ID exceeds 256 UTF-8 bytes (received {})",
            pull_request_id.len()
        )));
    }
    if pull_request_id.chars().any(char::is_control) {
        return Err(RuntimeError::Protocol(
            "pull request ID contains a control character".into(),
        ));
    }
    Ok(())
}

pub(super) fn prune_task_pull_requests(
    requests: &mut std::collections::HashMap<(String, String), PendingTaskPullRequest>,
) {
    let now = Instant::now();
    requests.retain(|_, request| {
        now.saturating_duration_since(request.created_at) < TASK_PULL_REQUEST_TTL
    });
}

impl TransferRuntime {
    pub async fn request_task_pull(
        &self,
        target_peer_id: &str,
        source_task_id: &str,
        transport: TransferTransport,
    ) -> Result<String, RuntimeError> {
        validate_source_task_id(source_task_id)?;
        if target_peer_id == self.config.peer_id {
            return Err(RuntimeError::Protocol(
                "cannot request a task pull from this runtime".into(),
            ));
        }

        let (target_peer, resolved_transport) = self
            .resolve_peer_with_transport(target_peer_id, transport)
            .await?;
        self.ensure_peer_is_trusted_for_transport(
            &target_peer.peer_id,
            &target_peer.public_key,
            resolved_transport,
        )?;
        let wire_request_id = self.next_request_id("task-pull");
        let sealed_payload = self
            .seal_authenticated_peer_request(
                &target_peer,
                "request_task_pull",
                &wire_request_id,
                serde_json::json!({
                    "requester_peer_id": self.config.peer_id,
                    "source_task_id": source_task_id,
                    "reserved_target_peer_id": target_peer.peer_id,
                }),
            )
            .await?;
        let response = self
            .send_peer_request(
                &target_peer,
                PeerRequest::RequestTaskPull {
                    request_id: wire_request_id,
                    requester_peer_id: self.config.peer_id.clone(),
                    sealed_payload,
                },
            )
            .await?;

        match response {
            PeerResponse::RequestTaskPull { request_id } => Ok(request_id),
            PeerResponse::Error { message, .. } => Err(RuntimeError::Protocol(message)),
            other => Err(unexpected_peer_response("task-pull", &other)),
        }
    }

    /// Tells the machine that asked for a pull that this one refused it.
    ///
    /// Best effort by construction: the refusal is already recorded on this
    /// machine, and a peer that cannot be reached (or is running a version
    /// without this request) must not turn a refusal into a retry loop. The
    /// caller logs what comes back.
    pub async fn report_task_pull_refusal(
        &self,
        requester_peer_id: &str,
        source_task_id: &str,
        pull_request_id: &str,
        reason: &str,
        transport: TransferTransport,
    ) -> Result<(), RuntimeError> {
        validate_source_task_id(source_task_id)?;
        if requester_peer_id == self.config.peer_id {
            return Err(RuntimeError::Protocol(
                "cannot report a task pull refusal to this runtime".into(),
            ));
        }
        let (requester, resolved_transport) = self
            .resolve_peer_with_transport(requester_peer_id, transport)
            .await?;
        self.ensure_peer_is_trusted_for_transport(
            &requester.peer_id,
            &requester.public_key,
            resolved_transport,
        )?;
        let wire_request_id = self.next_request_id("task-pull-refusal");
        let sealed_payload = self
            .seal_authenticated_peer_request(
                &requester,
                "report_task_pull_refused",
                &wire_request_id,
                serde_json::json!({
                    "source_peer_id": self.config.peer_id,
                    "source_task_id": source_task_id,
                    "pull_request_id": pull_request_id,
                    "reason": reason,
                    "reserved_target_peer_id": requester.peer_id,
                }),
            )
            .await?;
        let response = self
            .send_peer_request(
                &requester,
                PeerRequest::ReportTaskPullRefused {
                    request_id: wire_request_id,
                    source_peer_id: self.config.peer_id.clone(),
                    sealed_payload,
                },
            )
            .await?;

        match response {
            PeerResponse::ReportTaskPullRefused { .. } => Ok(()),
            PeerResponse::Error { message, .. } => Err(RuntimeError::Protocol(message)),
            other => Err(unexpected_peer_response("task-pull-refusal", &other)),
        }
    }
}
