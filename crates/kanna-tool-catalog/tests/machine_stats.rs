use kanna_tool_catalog::{bundled_catalog, resolve_request, Method, ResponseKind};
use serde_json::json;

#[test]
fn machine_stats_remains_a_server_aggregated_read_only_json_snapshot() {
    let catalog = bundled_catalog();
    let request = resolve_request(&catalog, "kanna_machine_stats", &json!({})).unwrap();
    assert_eq!(request.method, Method::Get);
    assert_eq!(request.kind, ResponseKind::Json);
    assert_eq!(request.path, "/v1/machine-stats");
    assert_eq!(request.machine_id, None);
    assert_eq!(request.body, json!({}));
    assert!(request.wait.is_none());
    assert!(request.local_response.is_none());

    let detailed =
        resolve_request(&catalog, "kanna_machine_stats", &json!({"detailed": true})).unwrap();
    assert_eq!(detailed.path, "/v1/machine-stats?detailed=true");
}
