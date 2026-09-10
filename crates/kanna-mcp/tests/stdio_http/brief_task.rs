use super::*;

fn call(arguments: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"kanna_get_task", "arguments":arguments}})
}

fn brief() -> Value {
    json!({"view":"brief", "briefVersion":1, "machineId":"remote", "id":"task-1",
        "repoId":"repo-1", "title":"Task", "titleTruncated":false,
        "runtimeState":null, "runtimeSettled":false, "readState":"unread",
        "deliveredInputCount":7, "waitingPromptSnippet":"Choose recovery",
        "providerRejection":{"recovery":"parked-override-binding", "matchedText":"Usage limit reached"},
        "latestRun":{"id":"run-1", "kind":"main", "status":"running", "summary":null,
            "summaryTruncated":false, "providerOverride":{"source":"operator", "provider":"codex"}}})
}

#[test]
fn brief_task_remote_payload_has_one_content_block_and_no_backing_json() {
    let expected = brief();
    let (base, server) = start_http_fixture(vec![
        ExpectedRequest {
            method: "GET",
            path: "/v1/status",
            body: None,
            response_status: "200 OK",
            response_body: json!({"desktopId":"local"}),
        },
        ExpectedRequest {
            method: "POST",
            path: "/v1/cloud/desktops/remote/invoke",
            body: Some(
                json!({"method":"GET", "path":"/v1/tasks/task-1?brief=true&agentView=true", "body":null}),
            ),
            response_status: "200 OK",
            response_body: json!({"status":200, "body":expected, "error":null}),
        },
    ]);
    let responses = run_kanna_mcp(
        &base,
        &[call(
            json!({"task_id":"task-1", "brief":true, "machine_id":"remote"}),
        )],
    );
    server.join().unwrap();
    assert_eq!(tool_text(&responses[0]), expected);
    assert_eq!(
        responses[0]["result"]["content"].as_array().unwrap().len(),
        1
    );
    assert!(responses[0]["result"].get("structuredContent").is_none());
}

#[test]
fn brief_task_old_local_and_remote_peers_are_explicit_errors() {
    for remote in [false, true] {
        let old = json!({"id":"task-1", "prompt":"secret bulky terms".repeat(10000)});
        let mut requests = vec![];
        let mut args = json!({"task_id":"task-1", "brief":true});
        if remote {
            args["machine_id"] = json!("remote");
            requests.push(ExpectedRequest {
                method: "GET",
                path: "/v1/status",
                body: None,
                response_status: "200 OK",
                response_body: json!({"desktopId":"local"}),
            });
            requests.push(ExpectedRequest {method:"POST", path:"/v1/cloud/desktops/remote/invoke", body:Some(json!({"method":"GET", "path":"/v1/tasks/task-1?brief=true&agentView=true", "body":null})), response_status:"200 OK", response_body:json!({"status":200, "body":old})});
        } else {
            requests.push(ExpectedRequest {
                method: "GET",
                path: "/v1/tasks/task-1?brief=true&agentView=true",
                body: None,
                response_status: "200 OK",
                response_body: old,
            });
        }
        let (base, server) = start_http_fixture(requests);
        let responses = run_kanna_mcp(&base, &[call(args)]);
        server.join().unwrap();
        assert!(tool_error_text(&responses[0]).contains("brief_task_detail_unsupported"));
        assert!(tool_error_text(&responses[0]).contains("brief:false"));
        assert!(!responses[0].to_string().contains("secret bulky terms"));
    }
}

#[test]
fn brief_task_preserves_http_errors_and_remote_lookup_hint() {
    for (status, body) in [
        (
            "404 Not Found",
            json!("task found on machine remote; pass machine_id"),
        ),
        ("500 Internal Server Error", json!("db error: fixture")),
    ] {
        let (base, server) = start_http_fixture(vec![ExpectedRequest {
            method: "GET",
            path: "/v1/tasks/task-1?brief=true&agentView=true",
            body: None,
            response_status: status,
            response_body: body.clone(),
        }]);
        let responses = run_kanna_mcp(&base, &[call(json!({"task_id":"task-1", "brief":true}))]);
        server.join().unwrap();
        assert!(tool_error_text(&responses[0]).contains(body.as_str().unwrap()));
        assert!(!tool_error_text(&responses[0]).contains("brief_task_detail_unsupported"));
    }
}

#[test]
fn brief_task_confirmation_cannot_return_an_older_full_response() {
    let mut stopped = brief();
    stopped["activity"] = json!("idle");
    let (base, server) = start_http_fixture(vec![
        ExpectedRequest {
            method: "GET",
            path: "/v1/tasks/task-1?brief=true&agentView=true",
            body: None,
            response_status: "200 OK",
            response_body: stopped,
        },
        ExpectedRequest {
            method: "GET",
            path: "/v1/tasks/task-1?brief=true&agentView=true",
            body: None,
            response_status: "200 OK",
            response_body: json!({"id":"task-1", "activity":"idle", "prompt":"must not leak"}),
        },
    ]);
    let responses = run_kanna_mcp(&base, &[call(json!({"task_id":"task-1", "brief":true}))]);
    server.join().unwrap();
    assert!(tool_error_text(&responses[0]).contains("brief_task_detail_unsupported"));
}

#[test]
fn full_task_payload_is_unchanged_and_brief_is_advertised() {
    let mut full = brief();
    full.as_object_mut().unwrap().remove("view");
    full.as_object_mut().unwrap().remove("briefVersion");
    full["prompt"] = json!("original terms ".repeat(10000));
    full["workflowDefinition"] = json!({"stages":[{"prompt":"workflow terms ".repeat(10000)}]});
    full["ports"] = json!((0..100)
        .map(|n| json!({"name":format!("PORT_{n}"), "port":20000+n}))
        .collect::<Vec<_>>());
    let (base, server) = start_http_fixture(vec![ExpectedRequest {
        method: "GET",
        path: "/v1/tasks/task-1?agentView=true",
        body: None,
        response_status: "200 OK",
        response_body: full.clone(),
    }]);
    let responses = run_kanna_mcp(
        &base,
        &[
            json!({"jsonrpc":"2.0", "id":0, "method":"tools/list", "params":{}}),
            call(json!({"task_id":"task-1"})),
        ],
    );
    server.join().unwrap();
    let tool = responses[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "kanna_get_task")
        .unwrap();
    assert_eq!(
        tool["inputSchema"]["properties"]["brief"]["type"],
        "boolean"
    );
    assert_eq!(tool_text(&responses[1]), full);
    assert!(brief().to_string().len() * 100 < responses[1].to_string().len());
}
