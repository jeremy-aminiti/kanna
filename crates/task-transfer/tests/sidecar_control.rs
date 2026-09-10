#[path = "support/sidecar.rs"]
mod sidecar;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use kanna_task_transfer::crypto::{
    parse_public_key, public_key_to_string, seal_json, TransferIdentity,
};
use kanna_task_transfer::peer_store::{PeerRecord, PeerStore};
use kanna_task_transfer::protocol::{
    ControlRequest, ControlResponse, PeerRegistryEntry, PeerRequest, PeerResponse,
    CURRENT_PROTOCOL_VERSION,
};
use kanna_task_transfer::registry::PeerRegistry;
use serde_json::json;
use sidecar::SidecarProcess;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_mark_read_does_not_monopolize_sidecar_control() {
    let temp = tempfile::tempdir().unwrap();
    let registry_dir = sidecar::registry_dir(temp.path());
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let target_identity = TransferIdentity::generate();
    let target_public_key = public_key_to_string(&target_identity.public_key);
    PeerRegistry::new(registry_dir.clone())
        .write_entry(&PeerRegistryEntry {
            peer_id: "peer-target".into(),
            display_name: "Target".into(),
            endpoint: format!("127.0.0.1:{port}"),
            pid: std::process::id(),
            public_key: target_public_key.clone(),
            protocol_version: CURRENT_PROTOCOL_VERSION,
            accepting_transfers: true,
        })
        .unwrap();
    PeerStore::new(
        registry_dir
            .join("trusted-peers")
            .join(format!("{}.json", URL_SAFE_NO_PAD.encode("peer-primary"))),
    )
    .upsert(PeerRecord {
        peer_id: "peer-target".into(),
        display_name: "Target".into(),
        public_key: target_public_key,
        capabilities_json: json!({
            "protocolVersion": CURRENT_PROTOCOL_VERSION,
            "authenticatedTaskRequests": true,
            "authenticatedTaskRequestVersion": 1,
        })
        .to_string(),
        paired_at: "2026-07-26T00:00:00Z".into(),
        last_seen_at: None,
        revoked_at: None,
    })
    .unwrap();

    let (peer_event_tx, mut peer_event_rx) = mpsc::unbounded_channel::<String>();
    let first_input_release = std::sync::Arc::new(Notify::new());
    let snapshot_release = std::sync::Arc::new(Notify::new());
    let peer_first_input_release = std::sync::Arc::clone(&first_input_release);
    let peer_snapshot_release = std::sync::Arc::clone(&snapshot_release);
    let peer_server = tokio::spawn(async move {
        let mut handlers = tokio::task::JoinSet::new();
        // Each admitted privileged operation first fetches the restart-specific
        // owner epoch, then opens its action connection. The two overloads are
        // rejected by sidecar admission before either connection is opened.
        for _ in 0..8 {
            let (stream, _) = listener.accept().await.unwrap();
            let peer_event_tx = peer_event_tx.clone();
            let first_input_release = std::sync::Arc::clone(&peer_first_input_release);
            let snapshot_release = std::sync::Arc::clone(&peer_snapshot_release);
            handlers.spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: PeerRequest = serde_json::from_str(line.trim()).unwrap();
                match request {
                    PeerRequest::GetAuthenticatedRequestEpoch { request_id } => {
                        let response = PeerResponse::AuthenticatedRequestEpoch {
                            request_id,
                            epoch: "sidecar-owner-epoch".into(),
                        };
                        reader
                            .get_mut()
                            .write_all(
                                format!("{}\n", serde_json::to_string(&response).unwrap())
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                    PeerRequest::MarkTaskRead { .. } => {
                        peer_event_tx.send("mark-started".into()).unwrap();
                        let mut remainder = Vec::new();
                        reader.read_to_end(&mut remainder).await.unwrap();
                        peer_event_tx.send("mark-closed".into()).unwrap();
                    }
                    PeerRequest::SendSessionInput {
                        request_id, data, ..
                    } => {
                        let input = String::from_utf8(data).unwrap();
                        peer_event_tx.send(format!("input:{input}")).unwrap();
                        if input == "first" {
                            first_input_release.notified().await;
                        }
                        let response = PeerResponse::SendSessionInput { request_id };
                        reader
                            .get_mut()
                            .write_all(
                                format!("{}\n", serde_json::to_string(&response).unwrap())
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                    PeerRequest::GetTaskSnapshot { request_id, .. } => {
                        peer_event_tx.send("snapshot-started".into()).unwrap();
                        snapshot_release.notified().await;
                        let response = PeerResponse::TaskSnapshot {
                            request_id,
                            peer_id: "peer-target".into(),
                            display_name: "Target".into(),
                            snapshot: json!({ "schemaVersion": 1, "tasks": [] }),
                        };
                        reader
                            .get_mut()
                            .write_all(
                                format!("{}\n", serde_json::to_string(&response).unwrap())
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                    other => panic!("unexpected peer request: {other:?}"),
                }
            });
        }
        while handlers.join_next().await.is_some() {}
    });

    let mut sidecar = SidecarProcess::spawn(temp.path(), |command| {
        command
            .env("KANNA_TRANSFER_CONTROL_MAX_IN_FLIGHT", "3")
            .env("KANNA_TRANSFER_MARK_READ_CONTROL_MAX_IN_FLIGHT", "1");
    });

    sidecar.write_control(&ControlRequest::MarkPeerTaskRead {
        request_id: "mark".into(),
        target_peer_id: "peer-target".into(),
        task_id: "task-unread".into(),
        expected_activity_revision: 7,
    });
    assert_eq!(
        sidecar
            .expect_alive("mark-read never reached the peer", peer_event_rx.recv())
            .await
            .as_deref(),
        Some("mark-started")
    );
    sidecar.write_control(&ControlRequest::MarkPeerTaskRead {
        request_id: "mark-overload".into(),
        target_peer_id: "peer-target".into(),
        task_id: "task-unread".into(),
        expected_activity_revision: 7,
    });
    let overloaded =
        sidecar.next_response("excess mark-read control did not receive bounded backpressure");
    assert!(
        matches!(
            overloaded,
            ControlResponse::Error {
                ref request_id,
                ref message,
            } if request_id == "mark-overload" && message.contains("too many mark-read")
        ),
        "unexpected overload response: {overloaded:?}",
    );
    sidecar.write_control(&ControlRequest::SendPeerSessionInput {
        request_id: "input-first".into(),
        target_peer_id: "peer-target".into(),
        session_id: "task-unread".into(),
        data: b"first".to_vec(),
        submission_boundary: false,
        control_input: false,
    });
    sidecar.write_control(&ControlRequest::SendPeerSessionInput {
        request_id: "input-second".into(),
        target_peer_id: "peer-target".into(),
        session_id: "task-unread".into(),
        data: b"second".to_vec(),
        submission_boundary: false,
        control_input: false,
    });
    sidecar.write_control(&ControlRequest::ListPeerTaskSnapshots {
        request_id: "refresh".into(),
    });
    let mut started = vec![
        sidecar
            .expect_alive(
                "first admitted operation never reached the peer",
                peer_event_rx.recv(),
            )
            .await
            .unwrap(),
        sidecar
            .expect_alive(
                "second admitted operation never reached the peer",
                peer_event_rx.recv(),
            )
            .await
            .unwrap(),
    ];
    started.sort_unstable();
    assert_eq!(started, vec!["input:first", "snapshot-started"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), peer_event_rx.recv())
            .await
            .is_err(),
        "second terminal input overtook the first response",
    );
    sidecar.write_control(&ControlRequest::ResizePeerSession {
        request_id: "ordinary-overload".into(),
        target_peer_id: "peer-target".into(),
        session_id: "task-unread".into(),
        cols: 100,
        rows: 30,
    });
    let ordinary_overload =
        sidecar.next_response("excess ordinary control did not receive bounded backpressure");
    assert!(
        matches!(
            ordinary_overload,
            ControlResponse::Error {
                ref request_id,
                ref message,
            } if request_id == "ordinary-overload" && message.contains("too many transfer")
        ),
        "unexpected ordinary overload response: {ordinary_overload:?}",
    );
    first_input_release.notify_one();
    snapshot_release.notify_one();

    let first = sidecar.next_response("terminal control waited behind stalled mark-read");
    let second = sidecar.next_response("LAN refresh waited behind stalled mark-read");
    let third = sidecar.next_response("second terminal input did not run after the first response");
    let mut completed_ids = vec![control_response_id(&first), control_response_id(&second)];
    completed_ids.push(control_response_id(&third));
    completed_ids.sort_unstable();
    assert_eq!(
        completed_ids,
        vec!["input-first", "input-second", "refresh"]
    );
    assert_eq!(
        sidecar
            .expect_alive(
                "queued terminal input never reached the peer",
                peer_event_rx.recv()
            )
            .await
            .as_deref(),
        Some("input:second")
    );

    // The 2000ms lower-layer deadline asserted below is the one under test;
    // this outer wait only has to outlast it by enough that load cannot fire
    // it first.
    let mark = sidecar.next_response("mark-read did not finish at its lower-layer deadline");
    assert_eq!(control_response_id(&mark), "mark");
    assert!(
        matches!(mark, ControlResponse::Error { ref message, .. } if message.contains("timed out after 2000ms")),
        "unexpected mark-read response: {mark:?}",
    );
    assert_eq!(
        sidecar
            .expect_alive(
                "stalled peer work survived mark-read timeout",
                peer_event_rx.recv()
            )
            .await,
        Some("mark-closed".into()),
    );
    sidecar
        .expect_alive(
            "peer server never saw every admitted operation",
            peer_server,
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sidecar_fails_closed_in_both_shipped_v4_terminal_input_directions() {
    let temp = tempfile::tempdir().unwrap();
    let registry_dir = sidecar::registry_dir(temp.path());
    let shipped_v4_target_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let target_identity = TransferIdentity::generate();
    let target_public_key = public_key_to_string(&target_identity.public_key);
    let viewer_identity = TransferIdentity::generate();
    let viewer_public_key = public_key_to_string(&viewer_identity.public_key);
    let registry = PeerRegistry::new(registry_dir.clone());
    registry
        .write_entry(&PeerRegistryEntry {
            peer_id: "peer-v4-target".into(),
            display_name: "Shipped v4 Target".into(),
            endpoint: shipped_v4_target_listener.local_addr().unwrap().to_string(),
            pid: std::process::id(),
            public_key: target_public_key.clone(),
            protocol_version: 4,
            accepting_transfers: true,
        })
        .unwrap();
    registry
        .write_entry(&PeerRegistryEntry {
            peer_id: "peer-v4-viewer".into(),
            display_name: "Shipped v4 Viewer".into(),
            endpoint: "127.0.0.1:9".into(),
            pid: std::process::id(),
            public_key: viewer_public_key.clone(),
            protocol_version: 4,
            accepting_transfers: true,
        })
        .unwrap();
    let peer_store = PeerStore::new(
        registry_dir
            .join("trusted-peers")
            .join(format!("{}.json", URL_SAFE_NO_PAD.encode("peer-primary"))),
    );
    for (peer_id, display_name, public_key) in [
        ("peer-v4-target", "Shipped v4 Target", target_public_key),
        ("peer-v4-viewer", "Shipped v4 Viewer", viewer_public_key),
    ] {
        peer_store
            .upsert(PeerRecord {
                peer_id: peer_id.into(),
                display_name: display_name.into(),
                public_key,
                capabilities_json:
                    "{\"protocolVersion\":4,\"authenticatedTaskRequests\":true,\"authenticatedTaskRequestVersion\":1}"
                        .into(),
                paired_at: "2026-08-16T00:00:00Z".into(),
                last_seen_at: None,
                revoked_at: None,
            })
            .unwrap();
    }

    let mut sidecar = SidecarProcess::spawn(temp.path(), |command| {
        command.env("KANNA_DAEMON_DIR", temp.path().join("no-daemon"));
    });

    // Liveness: a freshly spawned sidecar either advertises its listener or
    // never does — and a sidecar that refused to start never will, which the
    // wait reports instead of running out its deadline.
    let primary_entry = sidecar
        .expect_alive("current sidecar did not advertise its listener", async {
            loop {
                if let Some(entry) = registry
                    .list_peers("")
                    .unwrap()
                    .into_iter()
                    .find(|entry| entry.peer_id == "peer-primary")
                {
                    break entry;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

    for (request_id, data, submission_boundary, control_input) in [
        ("outbound-boundary", b"\r".to_vec(), true, false),
        ("outbound-control", b"\x1b[<65;1;1M".to_vec(), false, true),
    ] {
        sidecar.write_control(&ControlRequest::SendPeerSessionInput {
            request_id: request_id.into(),
            target_peer_id: "peer-v4-target".into(),
            session_id: "task-with-draft".into(),
            data,
            submission_boundary,
            control_input,
        });
        let response = sidecar.next_response(&format!("no control response for {request_id}"));
        assert!(
            matches!(
                response,
                ControlResponse::Error { request_id: ref response_id, ref message }
                    if response_id == request_id
                        && message.contains("protocol v4")
                        && message.contains("explicit terminal submission/control semantics")
            ),
            "current sidecar accepted outbound shipped-v4 terminal input: {response:?}",
        );
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            shipped_v4_target_listener.accept(),
        )
        .await
        .is_err(),
        "current sidecar connected to the shipped-v4 target before refusing input",
    );

    let owner_epoch = authenticated_request_epoch(&primary_entry.endpoint).await;
    let observe_payload = seal_sidecar_request(
        &viewer_identity,
        &primary_entry.public_key,
        "observe_session",
        "inbound-observe",
        &owner_epoch,
        json!({ "session_id": "task-with-draft" }),
    );
    let observe = send_raw_peer_value(
        &primary_entry.endpoint,
        &json!({
            "type": "observe_session",
            "request_id": "inbound-observe",
            "requester_peer_id": "peer-v4-viewer",
            "session_id": "task-with-draft",
            "sealed_payload": observe_payload,
        }),
    )
    .await;
    assert!(
        matches!(
            observe,
            PeerResponse::Error { ref message, .. }
                if message.contains("protocol v4")
                    && message.contains("duplex terminal control")
        ),
        "current owner sidecar admitted a shipped-v4 duplex observer: {observe:?}",
    );

    let input_payload = seal_sidecar_request(
        &viewer_identity,
        &primary_entry.public_key,
        "send_session_input",
        "inbound-input",
        &owner_epoch,
        json!({
            "session_id": "task-with-draft",
            "data": [13],
        }),
    );
    let input = send_raw_peer_value(
        &primary_entry.endpoint,
        &json!({
            "type": "send_session_input",
            "request_id": "inbound-input",
            "requester_peer_id": "peer-v4-viewer",
            "session_id": "task-with-draft",
            "data": [13],
            "sealed_payload": input_payload,
        }),
    )
    .await;
    assert!(
        matches!(
            input,
            PeerResponse::Error { ref message, .. }
                if message.contains("protocol v4")
                    && message.contains("explicit terminal submission/control semantics")
        ),
        "current owner sidecar admitted shipped-v4 fallback input: {input:?}",
    );
}

/// The renderer only learns that a duplicate push left state on disk if the
/// failure survives the control boundary. Anything the runtime swallows becomes
/// a success on stdout, and the operator warning can never fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoning_a_transfer_reports_cleanup_failure_over_the_control_channel() {
    let temp = tempfile::tempdir().unwrap();
    let mut sidecar = SidecarProcess::spawn(temp.path(), |_| {});

    let staged = temp.path().join("session.tar.gz");
    std::fs::write(&staged, b"session").unwrap();
    sidecar.write_control(&ControlRequest::StageTransferArtifact {
        request_id: "stage".into(),
        transfer_id: "transfer-abandon".into(),
        artifact_id: "claude-session".into(),
        path: staged.to_string_lossy().into_owned(),
        owned: true,
    });
    expect_response(&mut sidecar, "stage");
    sidecar.write_control(&ControlRequest::FetchTransferArtifact {
        request_id: "fetch".into(),
        transfer_id: "transfer-abandon".into(),
        artifact_id: "claude-session".into(),
    });
    let ControlResponse::FetchTransferArtifact { path, .. } =
        expect_response(&mut sidecar, "fetch")
    else {
        panic!("expected the staged artifact's managed path");
    };

    // Unlinking a directory is refused, so swapping the owned artifact for one
    // makes exactly this deletion fail and nothing else.
    let owned_artifact = std::path::PathBuf::from(path);
    std::fs::remove_file(&owned_artifact).unwrap();
    std::fs::create_dir(&owned_artifact).unwrap();

    sidecar.write_control(&ControlRequest::AbandonOutgoingTransfer {
        request_id: "abandon-blocked".into(),
        transfer_id: "transfer-abandon".into(),
    });
    let blocked = expect_response(&mut sidecar, "abandon-blocked");
    assert!(
        matches!(blocked, ControlResponse::Error { .. }),
        "abandon reported success over state it could not delete: {blocked:?}",
    );

    std::fs::remove_dir(&owned_artifact).unwrap();
    sidecar.write_control(&ControlRequest::AbandonOutgoingTransfer {
        request_id: "abandon-retry".into(),
        transfer_id: "transfer-abandon".into(),
    });
    let retried = expect_response(&mut sidecar, "abandon-retry");
    assert!(
        matches!(retried, ControlResponse::AbandonOutgoingTransfer { .. }),
        "the undeleted artifact was forgotten instead of retried: {retried:?}",
    );
    assert!(!owned_artifact.exists());

    // A transfer this sidecar never reserved is still a no-op, not an error the
    // renderer would have to tell the operator about.
    sidecar.write_control(&ControlRequest::AbandonOutgoingTransfer {
        request_id: "abandon-unknown".into(),
        transfer_id: "transfer-never-reserved".into(),
    });
    let unknown = expect_response(&mut sidecar, "abandon-unknown");
    assert!(
        matches!(unknown, ControlResponse::AbandonOutgoingTransfer { .. }),
        "abandoning an unknown transfer was not a no-op: {unknown:?}",
    );
}

async fn send_raw_peer_value(endpoint: &str, request: &serde_json::Value) -> PeerResponse {
    let mut stream = TcpStream::connect(endpoint).await.unwrap();
    stream
        .write_all(format!("{}\n", serde_json::to_string(request).unwrap()).as_bytes())
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .unwrap();
    serde_json::from_str(response.trim()).unwrap()
}

async fn authenticated_request_epoch(endpoint: &str) -> String {
    match send_raw_peer_value(
        endpoint,
        &json!({
            "type": "get_authenticated_request_epoch",
            "request_id": "epoch-probe",
        }),
    )
    .await
    {
        PeerResponse::AuthenticatedRequestEpoch { epoch, .. } => epoch,
        response => panic!("expected authenticated request epoch, got {response:?}"),
    }
}

fn seal_sidecar_request(
    sender: &TransferIdentity,
    receiver_public_key: &str,
    action: &str,
    request_id: &str,
    owner_epoch: &str,
    arguments: serde_json::Value,
) -> String {
    let mut payload = arguments.as_object().cloned().unwrap();
    payload.insert("action".into(), json!(action));
    payload.insert("request_id".into(), json!(request_id));
    payload.insert("owner_epoch".into(), json!(owner_epoch));
    payload.insert(
        "issued_at_unix_ms".into(),
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64),
    );
    seal_json(
        sender,
        &parse_public_key(receiver_public_key).unwrap(),
        &serde_json::Value::Object(payload),
    )
    .unwrap()
}

/// The next control response, asserting it answers the request just written.
fn expect_response(sidecar: &mut SidecarProcess, request_id: &str) -> ControlResponse {
    let response = sidecar.next_response(&format!("no control response for {request_id}"));
    assert_eq!(control_response_id(&response), request_id);
    response
}

fn control_response_id(response: &ControlResponse) -> &str {
    match response {
        ControlResponse::GetLocalIdentity { request_id, .. }
        | ControlResponse::Error { request_id, .. }
        | ControlResponse::ListPeers { request_id, .. }
        | ControlResponse::UpsertExternalPeer { request_id }
        | ControlResponse::RemoveExternalPeer { request_id }
        | ControlResponse::ClearExternalPeers { request_id }
        | ControlResponse::SetTaskSnapshot { request_id }
        | ControlResponse::ListPeerTaskSnapshots { request_id, .. }
        | ControlResponse::ObservePeerSession { request_id }
        | ControlResponse::UnobservePeerSession { request_id }
        | ControlResponse::ObservePeerCompanion { request_id }
        | ControlResponse::SendPeerCompanionEvent { request_id }
        | ControlResponse::UnobservePeerCompanion { request_id }
        | ControlResponse::SendPeerSessionInput { request_id }
        | ControlResponse::ResizePeerSession { request_id }
        | ControlResponse::ClosePeerTask { request_id }
        | ControlResponse::AdvancePeerTaskStage { request_id }
        | ControlResponse::ReadPeerTaskFile { request_id, .. }
        | ControlResponse::ReadPeerTaskDirectory { request_id, .. }
        | ControlResponse::ReadPeerTaskDiff { request_id, .. }
        | ControlResponse::MarkPeerTaskRead { request_id }
        | ControlResponse::StartPairing { request_id, .. }
        | ControlResponse::AcceptPairing { request_id, .. }
        | ControlResponse::RejectPairing { request_id, .. }
        | ControlResponse::StageTransferArtifact { request_id, .. }
        | ControlResponse::FetchTransferArtifact { request_id, .. }
        | ControlResponse::PrepareTransferPreflight { request_id, .. }
        | ControlResponse::RequestTaskPull { request_id, .. }
        | ControlResponse::ReportTaskPullRefusal { request_id }
        | ControlResponse::PrepareTransferCommit { request_id, .. }
        | ControlResponse::AbandonOutgoingTransfer { request_id, .. }
        | ControlResponse::FinalizeOutgoingTransfer { request_id, .. }
        | ControlResponse::CompleteOutgoingTransferFinalization { request_id, .. }
        | ControlResponse::AcknowledgeImportCommitted { request_id, .. }
        | ControlResponse::MarkIncomingEventRecorded { request_id, .. }
        | ControlResponse::MarkImportCommitApplied { request_id, .. }
        | ControlResponse::NackImportCommit { request_id, .. }
        | ControlResponse::MarkImportAckCompleted { request_id, .. } => request_id,
    }
}
