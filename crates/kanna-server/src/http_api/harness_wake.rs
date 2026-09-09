//! Harness adapters only deliver a mailbox nudge. They do not own event
//! filtering, cursor progress, acknowledgement, or the subscription lifecycle.
use super::{task_input, AppState};
use crate::db::{Db, EventSubscription};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Delivery {
    /// Portable supervisory input, with an engine-owned provenance label.
    #[default]
    Input,
    /// Opt-in until the running CLI's app-server interface has been verified.
    CodexAppServer,
    /// Durable mailbox only; the harness is responsible for checking it.
    Poll,
}

impl Delivery {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::CodexAppServer => "codex_app_server",
            Self::Poll => "poll",
        }
    }
}

pub(super) async fn deliver(
    state: Arc<AppState>,
    row: &EventSubscription,
) -> Result<&'static str, task_input::EngineWakeFailure> {
    let message = format!(
        "[Kanna supervisor] Event subscription {} has pending events (batch {}). Read them with kanna_read_event_subscription, then acknowledge that batch after reconciling it. This is an engine wakeup, not an owner directive or a task-completion verdict.",
        row.id, row.batch_id,
    );
    match row.delivery.as_str() {
        "poll" => Ok("ready"),
        "input" => task_input::send_engine_wake(state, row, message)
            .await
            .map(|queued| if queued { "queued" } else { "notified" }),
        // A native turn may have been started before the failure was observed,
        // and an unknown adapter cannot start working by itself: both park.
        "codex_app_server" => native_codex(state, row)
            .await
            .map(|_| "notified")
            .map_err(park),
        other => Err(park(format!(
            "unsupported harness delivery adapter: {other}"
        ))),
    }
}

fn park(message: String) -> task_input::EngineWakeFailure {
    task_input::EngineWakeFailure {
        retry: task_input::DeliveryRetry::Park,
        message,
    }
}

fn wake_output(row: &EventSubscription) -> Value {
    json!({"name": "kanna_event_subscription", "output": json!({
        "subscriptionId": row.id, "batchId": row.batch_id, "pending": true,
        "nextTool": "kanna_read_event_subscription",
    }).to_string()})
}

fn same_worktree(left: &str, right: &str) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

async fn native_codex(state: Arc<AppState>, row: &EventSubscription) -> Result<(), String> {
    let Some(_mutation) = state.try_begin_requested_task_mutation(&row.task_id) else {
        return Err("subscriber is changing sessions; mailbox remains pending".into());
    };
    let (thread, cwd) = {
        let db = Db::open(&state.config().db_path).map_err(|e| e.to_string())?;
        let task = db
            .get_pipeline_item(&row.task_id)
            .map_err(|e| e.to_string())?
            .ok_or("subscriber task no longer exists")?;
        let run = db
            .latest_stage_run(&row.task_id)
            .map_err(|e| e.to_string())?
            .ok_or("subscriber has no run")?;
        if task.closed_at.is_some()
            || task.runtime_status.as_deref() == Some("exited")
            || run.agent_provider.as_deref() != Some("codex")
            || run.id != row.run_id
        {
            return Err("native wake does not match the current Codex task/run".into());
        }
        let thread = run.provider_session_id;
        (thread, run.cwd.ok_or("subscriber run has no worktree")?)
    };
    #[cfg(test)]
    let executable = match &state.subscription_proxy_executable {
        Some(executable) => executable.clone(),
        None => crate::task_creator::resolve_agent_executable(
            kanna_agent_protocol::AgentProvider::Codex,
        )?,
    };
    #[cfg(not(test))]
    let executable =
        crate::task_creator::resolve_agent_executable(kanna_agent_protocol::AgentProvider::Codex)?;
    let mut child = tokio::process::Command::new(executable)
        .args(["app-server", "proxy"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Codex app-server proxy unavailable: {e}"))?;
    let mut writer = child.stdin.take().ok_or("proxy stdin unavailable")?;
    let mut reader = BufReader::new(child.stdout.take().ok_or("proxy stdout unavailable")?);
    let operation = native_exchange(
        &mut writer,
        &mut reader,
        thread.as_deref(),
        &cwd,
        wake_output(row),
    );
    let result = tokio::time::timeout(Duration::from_secs(15), operation)
        .await
        .map_err(|_| {
            "Codex native wake delivery uncertain (timeout); do not blindly retry".to_string()
        })?;
    drop(writer);
    // This is only our proxy, never the provider-owned app server or TUI.
    if let Err(error) = child.kill().await {
        log::debug!("Codex wake proxy cleanup: {error}");
    }
    result
}

async fn native_exchange<W: tokio::io::AsyncWrite + Unpin, R: tokio::io::AsyncBufRead + Unpin>(
    writer: &mut W,
    reader: &mut R,
    thread: Option<&str>,
    cwd: &str,
    output: Value,
) -> Result<(), String> {
    rpc(
        writer,
        reader,
        1,
        "initialize",
        json!({
            "clientInfo": {"name": "kanna-event-wake", "version": "1"},
        }),
    )
    .await?;
    writer
        .write_all(b"{\"method\":\"initialized\"}\n")
        .await
        .map_err(|e| e.to_string())?;
    // Fresh PTY runs normally learn their native id only on Exit. Discover
    // only a unique loaded root in this stage's worktree; never select a
    // historical thread or a subagent merely because its cwd matches.
    let discovered;
    let thread = if let Some(thread) = thread {
        thread
    } else {
        discovered = discover_native_thread(writer, reader, cwd).await?;
        &discovered
    };
    let response = rpc(
        writer,
        reader,
        2,
        "thread/read",
        json!({"threadId": thread}),
    )
    .await?;
    let observed = response["thread"]["cwd"]
        .as_str()
        .ok_or("native thread has no cwd")?;
    if !same_worktree(observed, cwd) {
        return Err("Codex thread belongs to another worktree".to_string());
    }
    rpc(
        writer,
        reader,
        3,
        "turn/start",
        json!({
            "threadId": thread, "input": [], "toolOutput": output,
        }),
    )
    .await?;
    Ok(())
}

async fn discover_native_thread<
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncBufRead + Unpin,
>(
    writer: &mut W,
    reader: &mut R,
    cwd: &str,
) -> Result<String, String> {
    let mut cursor = Value::Null;
    let mut matches = Vec::new();
    loop {
        let page = rpc(
            writer,
            reader,
            4,
            "thread/loaded/list",
            json!({"cursor":cursor, "limit":100}),
        )
        .await?;
        let ids = page["data"]
            .as_array()
            .ok_or("Codex omitted loaded thread ids")?;
        for id in ids {
            let thread = rpc(writer, reader, 5, "thread/read", json!({"threadId":id})).await?;
            let thread = &thread["thread"];
            if thread["cwd"]
                .as_str()
                .is_some_and(|observed| same_worktree(observed, cwd))
                && thread["parentThreadId"].is_null()
                && matches!(thread["source"].as_str(), Some("cli" | "appServer"))
            {
                matches.push(id.as_str().ok_or("invalid loaded thread id")?.to_owned());
            }
        }
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    if matches.len() != 1 {
        return Err(format!("expected one loaded Codex root thread for the manager worktree, found {}; mailbox remains pending", matches.len()));
    }
    Ok(matches.remove(0))
}

async fn rpc<W: tokio::io::AsyncWrite + Unpin, R: tokio::io::AsyncBufRead + Unpin>(
    writer: &mut W,
    reader: &mut R,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let mut request = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))
        .map_err(|e| e.to_string())?;
    request.push(b'\n');
    writer
        .write_all(&request)
        .await
        .map_err(|e| format!("native wake write uncertain: {e}"))?;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?
            == 0
        {
            return Err("Codex control connection closed; delivery may be uncertain".into());
        }
        let response: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if response["id"].as_u64() != Some(id) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(format!("Codex rejected {method}: {error}"));
        }
        return response
            .get("result")
            .cloned()
            .ok_or_else(|| "Codex response omitted result".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exercise(observed_cwd: &str) -> (Result<(), String>, Vec<Value>) {
        let (client, server) = tokio::io::duplex(8192);
        let (client_read, mut client_write) = tokio::io::split(client);
        let mut client_read = BufReader::new(client_read);
        let cwd = observed_cwd.to_owned();
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            let mut requests = Vec::new();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                requests.push(request.clone());
                if request.get("id").is_none() {
                    continue;
                }
                let result = if request["method"] == "thread/read" {
                    json!({"thread":{"cwd":cwd}})
                } else {
                    json!({})
                };
                write
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });
        let result = native_exchange(
            &mut client_write,
            &mut client_read,
            Some("native-thread"),
            "/workspace/manager",
            json!({"name":"kanna_event_subscription", "output":"mailbox ready"}),
        )
        .await;
        drop(client_write);
        drop(client_read);
        (result, server.await.unwrap())
    }

    #[tokio::test]
    async fn native_adapter_starts_a_turn_with_tool_output_without_user_input() {
        let (result, requests) = exercise("/workspace/manager").await;
        result.unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|r| r["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "initialized", "thread/read", "turn/start"]
        );
        assert_eq!(requests[3]["params"]["input"], json!([]));
        assert_eq!(requests[3]["params"]["threadId"], "native-thread");
        assert_eq!(
            requests[3]["params"]["toolOutput"]["name"],
            "kanna_event_subscription"
        );
    }

    #[tokio::test]
    async fn native_adapter_refuses_another_worktree_before_starting_a_turn() {
        let (result, requests) = exercise("/workspace/other").await;
        assert!(result.unwrap_err().contains("another worktree"));
        assert!(!requests
            .iter()
            .any(|request| request["method"] == "turn/start"));
    }
    async fn discover_fixture(threads: Value) -> Result<String, String> {
        let (client, server) = tokio::io::duplex(8192);
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = if request["method"] == "thread/loaded/list" {
                    json!({"data": threads.as_object().unwrap().keys().collect::<Vec<_>>(), "nextCursor":null})
                } else {
                    json!({"thread":threads[request["params"]["threadId"].as_str().unwrap()]})
                };
                write
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let result = discover_native_thread(&mut write, &mut read, "/manager").await;
        drop(write);
        drop(read);
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn fresh_native_run_discovers_only_the_loaded_root_for_its_worktree() {
        let result = discover_fixture(json!({
            "root":{"cwd":"/manager","source":"cli","parentThreadId":null},
            "child":{"cwd":"/manager","source":{"subAgent":"other"},"parentThreadId":"root"},
            "other":{"cwd":"/other","source":"cli","parentThreadId":null},
        }))
        .await
        .unwrap();
        assert_eq!(result, "root");
    }

    #[tokio::test]
    async fn native_discovery_refuses_missing_or_ambiguous_roots() {
        assert!(discover_fixture(json!({}))
            .await
            .unwrap_err()
            .contains("found 0"));
        assert!(discover_fixture(json!({
            "one":{"cwd":"/manager","source":"cli"},
            "two":{"cwd":"/manager","source":"cli"},
        }))
        .await
        .unwrap_err()
        .contains("found 2"));
    }
}
