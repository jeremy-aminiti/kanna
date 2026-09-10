use kanna_tool_catalog::{bundled_catalog, resolve_request, validate_task_detail_view};
use serde_json::json;

#[test]
fn brief_task_catalog_is_explicit_and_routes_without_changing_full_default() {
    let catalog = bundled_catalog();
    let full = resolve_request(&catalog, "kanna_get_task", &json!({"task_id":"task 1"})).unwrap();
    assert_eq!(full.path, "/v1/tasks/task%201?agentView=true");
    let brief = resolve_request(
        &catalog,
        "kanna_get_task",
        &json!({"task_id":"task 1", "brief":true, "machine_id":"peer"}),
    )
    .unwrap();
    assert_eq!(brief.path, "/v1/tasks/task%201?brief=true&agentView=true");
    assert_eq!(brief.machine_id.as_deref(), Some("peer"));
    assert!(resolve_request(
        &catalog,
        "kanna_get_task",
        &json!({"task_id":"1", "brief":"yes"})
    )
    .is_err());
    assert!(
        validate_task_detail_view(&brief.path, &json!({"view":"brief", "briefVersion":1})).is_ok()
    );
    for incompatible in [
        json!({}),
        json!({"view":"brief"}),
        json!({"view":"brief", "briefVersion":2}),
    ] {
        assert!(validate_task_detail_view(&brief.path, &incompatible)
            .unwrap_err()
            .contains("brief_task_detail_unsupported"));
        assert!(validate_task_detail_view(&full.path, &incompatible).is_ok());
        assert!(validate_task_detail_view("/v1/tasks/1?brief=false", &incompatible).is_ok());
    }
}

#[test]
fn old_catalog_rejects_brief_instead_of_ignoring_it() {
    let mut old = bundled_catalog();
    old.tools
        .iter_mut()
        .find(|tool| tool.name == "kanna_get_task")
        .unwrap()
        .params
        .retain(|param| param.name != "brief");
    let error = resolve_request(
        &old,
        "kanna_get_task",
        &json!({"task_id":"task-1", "brief":true}),
    )
    .unwrap_err();
    assert!(error.contains("brief"));
    assert!(resolve_request(&old, "kanna_get_task", &json!({"task_id":"task-1"})).is_ok());
}
