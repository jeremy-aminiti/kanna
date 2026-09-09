use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};

fn run(args: &[&str], remote: bool, status: u16, body: Value) -> (Output, Vec<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..if remote { 2 } else { 1 } {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            let (code, response) = if remote && index == 0 {
                (200, json!({"desktopId":"local"}))
            } else if remote {
                (200, json!({"status":status, "body":body, "error":null}))
            } else {
                (status, body.clone())
            };
            let response = response.to_string();
            write!(stream, "HTTP/1.1 {code} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
        requests
    });
    let output = Command::new(env!("CARGO_BIN_EXE_kanna-cli"))
        .args(args)
        .env_clear()
        .env("KANNA_SERVER_BASE_URL", base)
        .output()
        .unwrap();
    (output, server.join().unwrap())
}

fn brief() -> Value {
    json!({"view":"brief", "briefVersion":1, "id":"task-1", "machineId":"peer", "repoId":"repo-1", "title":"Task",
        "runtimeState":null, "runtimeSettled":false, "readState":"unread", "deliveredInputCount":9,
        "blockedByTaskIds":["blocker"], "childTaskIds":["child"], "revisionRounds":2, "revisionLimit":5,
        "waitingPromptSnippet":"Choose recovery", "providerRejection":{"scope":null, "recovery":"parked-override-binding", "matchedText":"Usage limit reached"},
        "latestRun":{"id":"run-1", "kind":"main", "status":"failed", "summary":"failure", "summaryTruncated":false, "providerOverride":{"provider":"codex", "source":"operator"}}})
}

#[test]
fn brief_task_typed_and_catalog_cli_preserve_the_exact_projection() {
    for (args, remote, path) in [
        (
            vec!["task", "get", "--task-id", "task-1", "--brief"],
            false,
            "/v1/tasks/task-1?agentView=true&brief=true",
        ),
        (
            vec![
                "tool",
                "call",
                "kanna_get_task",
                "--arg",
                "task_id=task-1",
                "--arg",
                "brief=true",
                "--arg",
                "machine_id=peer",
            ],
            true,
            "/v1/tasks/task-1?brief=true&agentView=true",
        ),
    ] {
        let expected = brief();
        let (output, requests) = run(&args, remote, 200, expected.clone());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            expected
        );
        assert!(requests.last().unwrap().contains(path));
        assert!(output.stdout.len() < 2000);
    }
}

#[test]
fn brief_task_cli_rejects_old_peers_without_dumping_full_terms() {
    for (args, remote) in [
        (vec!["task", "get", "--task-id", "task-1", "--brief"], false),
        (
            vec![
                "tool",
                "call",
                "kanna_get_task",
                "--arg",
                "task_id=task-1",
                "--arg",
                "brief=true",
                "--arg",
                "machine_id=peer",
            ],
            true,
        ),
    ] {
        let (output, _) = run(
            &args,
            remote,
            200,
            json!({"id":"task-1", "prompt":"private original terms ".repeat(5000)}),
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("brief_task_detail_unsupported"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private original terms"));
    }
}

#[test]
fn brief_task_cli_preserves_destination_errors() {
    let (output, _) = run(
        &[
            "tool",
            "call",
            "kanna_get_task",
            "--arg",
            "task_id=task-1",
            "--arg",
            "brief=true",
            "--arg",
            "machine_id=peer",
        ],
        true,
        500,
        json!("db error from peer"),
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("db error from peer"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("brief_task_detail_unsupported"));
}

#[test]
fn brief_task_full_catalog_cli_is_unchanged_and_much_larger() {
    let mut full = brief();
    full.as_object_mut().unwrap().remove("view");
    full.as_object_mut().unwrap().remove("briefVersion");
    full["prompt"] = json!("original task terms ".repeat(5000));
    full["workflowDefinition"] =
        json!({"stages":[{"prompt":"workflow instructions ".repeat(5000)}]});
    full["ports"] = json!((0..100)
        .map(|n| json!({"name":format!("SERVICE_{n}"), "port":20000+n}))
        .collect::<Vec<_>>());
    let (output, requests) = run(
        &["tool", "call", "kanna_get_task", "--arg", "task_id=task-1"],
        false,
        200,
        full.clone(),
    );
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        full
    );
    assert!(requests[0].starts_with("GET /v1/tasks/task-1?agentView=true HTTP/1.1"));
    assert!(brief().to_string().len() * 100 < output.stdout.len());
}
