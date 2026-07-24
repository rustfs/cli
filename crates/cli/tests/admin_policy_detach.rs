#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};

const INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
const RUNTIME_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled","reason":"disabled by configuration"},"memory_sampling":{"state":"unsupported","reason":"not available on this platform"},"platform":{"state":"supported"},"topology":{"state":"unknown","reason":"storage is initializing"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"unknown"}}"#;
const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
const SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"disabled"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;
const DETACH_RESPONSE: &str = r#"{"policiesAttached":[],"policiesDetached":["readonly"],"updatedAt":"2026-07-24T08:00:00Z","secretKey":"must-not-leak"}"#;

#[test]
fn policy_detach_dispatches_typed_multi_policy_request_and_emits_v3_result() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        ("200 OK", RUNTIME_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", DETACH_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "policy",
            "detach",
            "myalias",
            "writeonly,readonly",
            "--user",
            "alice",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "iam_policy_detach");
    assert_eq!(value["data"]["entity"]["type"], "user");
    assert_eq!(value["data"]["entity"]["name"], "alice");
    assert_eq!(value["data"]["detached"], serde_json::json!(["readonly"]));
    assert_eq!(value["data"]["unchanged"], serde_json::json!(["writeonly"]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-leak"));

    for expected in [
        ("GET", "/rustfs/admin/v3/info"),
        ("GET", "/rustfs/admin/v4/runtime/capabilities"),
        ("GET", "/rustfs/admin/v4/extensions/catalog"),
        ("GET", "/rustfs/admin/v4/cluster/snapshot"),
    ] {
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("captured capability request");
        assert_eq!((request.method.as_str(), request.target.as_str()), expected);
    }
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured detach request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rustfs/admin/v3/idp/builtin/policy/detach");
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("detach body should be JSON");
    assert_eq!(
        body["policies"],
        serde_json::json!(["readonly", "writeonly"])
    );
    assert_eq!(body["user"], "alice");
    assert!(body.get("group").is_none());
    handle.join().expect("admin test server finished");
}
