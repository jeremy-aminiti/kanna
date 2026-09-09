use super::*;
use serde_json::{json, Value};

fn fixture() -> axum::Router {
    test_router_with_seed("brief-machine", "Brief fixture", |db| {
        db.insert_test_repo("repo-brief", "Brief repo").unwrap();
        // No display name: the full title falls back to the entire prompt.
        db.insert_test_pipeline_item(
            "brief-task",
            "repo-brief",
            &"task terms 🦀 ".repeat(5000),
            None,
            "in progress",
            "2026-09-09 00:00:00",
        )
        .unwrap();
        for id in ["brief-parent", "brief-child", "brief-blocker"] {
            db.insert_test_pipeline_item(
                id,
                "repo-brief",
                "Related task",
                Some(id),
                "in progress",
                "2026-09-09 00:00:00",
            )
            .unwrap();
        }
        db.update_pipeline_item_parent("brief-task", Some("brief-parent"))
            .unwrap();
        db.update_pipeline_item_parent("brief-child", Some("brief-task"))
            .unwrap();
        db.close_pipeline_item("brief-child").unwrap();
        db.insert_task_blocker("brief-task", "brief-blocker")
            .unwrap();
        db.update_test_pipeline_item_stage_context(
            "brief-task",
            "task-brief-task",
            "single-reviewer",
            None,
            "codex",
        )
        .unwrap();
        let workflow = json!({"stages": [{"name": "in progress", "agent": "build", "transition": "manual", "prompt": "workflow terms ".repeat(5000)}]});
        db.update_test_pipeline_item_pipeline_def("brief-task", &workflow.to_string())
            .unwrap();
        for index in 0..100 {
            db.claim_task_port(
                "brief-task",
                &format!("SERVICE_{index}_PORT"),
                20000 + index,
            )
            .unwrap();
        }
        let result =
            json!({"status": "failure", "summary": "diagnostic 🦀 ".repeat(1000)}).to_string();
        db.insert_stage_run_with_completion_transition(
            crate::db::NewStageRun {
                id: "brief-run",
                task_id: "brief-task",
                stage: "in progress",
                kind: "main",
                agent: Some("build"),
                agent_provider: Some("codex"),
                model: None,
                effort: None,
                status: "failed",
                result: Some(&result),
                feedback: None,
                session_id: None,
                provider_session_id: None,
                cwd: None,
                resumed_from_run_id: None,
            },
            Some("manual"),
        )
        .unwrap();
        db.set_test_stage_run_provider_override(
            "brief-run",
            &crate::db::StageProviderOverride {
                source: "operator".into(),
                provider: "codex".into(),
                model: None,
                effort: None,
            },
        )
        .unwrap();
        db.record_task_input(
            "brief-task",
            crate::db::TaskInputSource::Operator,
            "Durable owner directive",
        )
        .unwrap();
        db.record_provider_rejection(crate::db::NewProviderRejection {
            task_id: "brief-task",
            stage_run_id: "brief-run",
            stage: "in progress",
            provider: "codex",
            model: None,
            effort: None,
            source: crate::db::QuotaRejectionSource::Pty,
            rule_id: "fixture-quota",
            matched_text: "Usage limit reached",
            scope: None,
            cli_version: None,
            recovery: crate::db::QuotaRecovery::ParkedOverrideBinding,
            replacement_run_id: None,
        })
        .unwrap();
        db.update_pipeline_item_waiting_prompt("brief-task", "Choose a recovery option")
            .unwrap();
        db.update_pipeline_item_composer("brief-task", Some("provider suggestion"), "not-typed")
            .unwrap();
    })
}

async fn read(app: axum::Router, path: &str) -> Value {
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn assert_brief(full: &Value, brief: &Value) {
    assert_eq!(brief["view"], "brief");
    assert_eq!(brief["briefVersion"], 1);
    assert_eq!(brief["machineId"], "brief-machine");
    for key in ["prompt", "workflowDefinition", "ports", "pipelineName"] {
        assert!(full.get(key).is_some(), "fixture needs {key}");
        assert!(brief.get(key).is_none(), "brief leaked {key}");
    }
    assert_eq!(brief["title"].as_str().unwrap().chars().count(), 200);
    assert_eq!(brief["titleTruncated"], true);
    assert_eq!(
        brief["latestRun"]["summary"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1000
    );
    assert_eq!(brief["latestRun"]["summaryTruncated"], true);
    for (key, value) in full.as_object().unwrap() {
        if ![
            "prompt",
            "workflowDefinition",
            "ports",
            "pipelineName",
            "title",
            "latestRun",
        ]
        .contains(&key.as_str())
        {
            assert_eq!(brief.get(key), Some(value), "lost operational field {key}");
        }
    }
    for (key, value) in full["latestRun"].as_object().unwrap() {
        if key != "summary" {
            assert_eq!(&brief["latestRun"][key], value);
        }
    }
    assert_eq!(brief["deliveredInputCount"], 1);
    assert_eq!(brief["parentTaskId"], "brief-parent");
    assert_eq!(brief["childTaskIds"], json!(["brief-child"]));
    assert_eq!(brief["blockedByTaskIds"], json!(["brief-blocker"]));
    assert_eq!(
        brief["providerRejection"]["recovery"],
        "parked-override-binding"
    );
    assert!(brief["providerRejection"].get("scope").is_none());
    assert_eq!(brief["latestRun"]["providerOverride"]["source"], "operator");
    assert_eq!(brief["runtimeState"], Value::Null);
    assert!(brief.get("composer").is_none());
    assert!(brief.to_string().len() * 10 < full.to_string().len());
}

#[tokio::test]
async fn brief_task_http_projection_preserves_full_and_diagnostics() {
    let app = fixture();
    let full = read(app.clone(), "/v1/tasks/brief-task?agentView=true").await;
    let explicit_full = read(
        app.clone(),
        "/v1/tasks/brief-task?agentView=true&brief=false",
    )
    .await;
    assert_eq!(full, explicit_full);
    let brief = read(
        app.clone(),
        "/v1/tasks/brief-task?agentView=true&brief=true",
    )
    .await;
    assert_brief(&full, &brief);
    let human = read(app.clone(), "/v1/tasks/brief-task").await;
    assert_eq!(human["composer"]["text"], "provider suggestion");
    let human_brief = read(app.clone(), "/v1/tasks/brief-task?brief=true").await;
    assert_eq!(human_brief["composer"], human["composer"]);
    for path in [
        "/v1/tasks/missing?brief=true",
        "/v1/tasks/brief-task?brief=invalid",
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(response.status().is_client_error());
    }
    // Machine-invoke dispatch uses the real route and keeps the query intact.
    let mut request = Request::post("/v1/cloud/desktops/brief-machine/invoke")
        .header("content-type", "application/json")
        .body(Body::from(json!({"method":"GET", "path":"/v1/tasks/brief-task?brief=true&agentView=true", "body":null}).to_string())).unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            49152,
        ))));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let routed: Value = serde_json::from_slice(&body).unwrap();
    assert_brief(&full, &routed["body"]);
}

#[tokio::test]
async fn brief_task_keeps_closed_runless_state_and_only_attested_drafts() {
    for (attestation, closed) in [
        ("typed", false),
        ("not-typed", false),
        ("unknown", false),
        ("typed", true),
    ] {
        let app = test_router_with_seed("brief-short", "Short fixture", |db| {
            db.insert_test_repo("repo-short", "Short repo").unwrap();
            db.insert_test_pipeline_item(
                "short",
                "repo-short",
                "Terms",
                Some("Short"),
                "review",
                "2026-09-09 00:00:00",
            )
            .unwrap();
            if closed {
                db.close_pipeline_item("short").unwrap();
            }
            db.update_pipeline_item_composer("short", Some("Draft"), attestation)
                .unwrap();
        });
        let brief = read(app, "/v1/tasks/short?brief=true&agentView=true").await;
        assert_eq!(brief["title"], "Short");
        assert_eq!(brief["titleTruncated"], false);
        assert_eq!(brief["latestRun"], Value::Null);
        assert_eq!(brief["deliveredInputCount"], 0);
        assert_eq!(brief["closedAt"].is_string(), closed);
        assert_eq!(
            brief.get("composer").is_some(),
            !closed && attestation == "typed"
        );
    }
}

/// Real DB -> HTTP listener -> actual CLI/MCP processes. Requires the two
/// adapters built from this worktree; no app or daemon startup is necessary.
#[tokio::test]
#[ignore = "build kanna-cli and kanna-mcp first; run brief_task_http_adapters_e2e --ignored"]
async fn brief_task_http_adapters_e2e() {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            fixture().into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    struct StopServer(tokio::task::JoinHandle<()>);
    impl Drop for StopServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = StopServer(server);
    let full: Value = reqwest::get(format!("{base}/v1/tasks/brief-task?agentView=true"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for (label, args, message, is_brief) in [
        (
            "kanna-cli",
            vec!["task", "get", "--task-id", "brief-task", "--brief"],
            None,
            true,
        ),
        (
            "kanna-cli",
            vec![
                "tool",
                "call",
                "kanna_get_task",
                "--arg",
                "task_id=brief-task",
                "--arg",
                "brief=true",
                "--arg",
                "machine_id=brief-machine",
            ],
            None,
            true,
        ),
        (
            "kanna-mcp",
            vec!["serve"],
            Some(
                json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"kanna_get_task", "arguments":{"task_id":"brief-task", "brief":true, "machine_id":"brief-machine"}}}),
            ),
            true,
        ),
        (
            "kanna-cli",
            vec![
                "tool",
                "call",
                "kanna_get_task",
                "--arg",
                "task_id=brief-task",
                "--arg",
                "brief=false",
            ],
            None,
            false,
        ),
        (
            "kanna-mcp",
            vec!["serve"],
            Some(
                json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"kanna_get_task", "arguments":{"task_id":"brief-task", "brief":false}}}),
            ),
            false,
        ),
    ] {
        let binary = root.join(".build/debug").join(label);
        assert!(binary.exists(), "build {} first", binary.display());
        let mut child = tokio::process::Command::new(binary)
            .args(args)
            .env_clear()
            .env("KANNA_SERVER_BASE_URL", &base)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        if let Some(message) = message {
            let mut input = child.stdin.take().unwrap();
            input
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        } else {
            drop(child.stdin.take());
        }
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let brief = if label == "kanna-mcp" {
            assert_eq!(value["result"]["content"].as_array().unwrap().len(), 1);
            assert!(value["result"].get("structuredContent").is_none());
            serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
        } else {
            value
        };
        if is_brief {
            assert_brief(&full, &brief);
        } else {
            assert_eq!(brief, full, "full adapter output changed");
            eprintln!("{label}: full adapter stdout={} bytes", output.stdout.len());
            continue;
        }
        eprintln!("{label}: full HTTP JSON={} bytes, brief JSON={} bytes, adapter stdout={} bytes\n{brief}", full.to_string().len(), brief.to_string().len(), output.stdout.len());
    }
}
