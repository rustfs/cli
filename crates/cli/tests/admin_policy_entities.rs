#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};

const INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
const RUNTIME_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled","reason":"disabled by configuration"},"memory_sampling":{"state":"unsupported","reason":"not available on this platform"},"platform":{"state":"supported"},"topology":{"state":"unknown","reason":"storage is initializing"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"unknown"}}"#;
const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
const SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"disabled"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;
const POLICY_ENTITIES_RESPONSE: &str = r#"{"timestamp":"2026-07-24T08:00:00Z","userMappings":[{"user":"alice","policies":["readonly"],"memberOfMappings":[{"group":"ops/team","policies":["diagnostics"]}],"secretKey":"must-not-leak"}],"groupMappings":[{"group":"ops/team","policies":["diagnostics"]}],"policyMappings":[{"policy":"read only","users":["alice"],"groups":[]}],"sessionToken":"must-not-leak"}"#;

#[test]
fn policy_entities_dispatches_after_capability_gate_and_emits_redacted_v3_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        ("200 OK", RUNTIME_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", POLICY_ENTITIES_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "policy",
            "entities",
            "myalias",
            "--user",
            "alice@example.com",
            "--user",
            "bob",
            "--group",
            "ops/team",
            "--policy",
            "read only",
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
    assert_eq!(value["type"], "iam_policy_entities");
    assert_eq!(value["data"]["user_mappings"][0]["user"], "alice");
    assert_eq!(
        value["data"]["user_mappings"][0]["member_of_mappings"][0]["group"],
        "ops/team"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("must-not-leak"));
    assert!(output.stderr.is_empty());

    for expected in [
        "/rustfs/admin/v3/info",
        "/rustfs/admin/v4/runtime/capabilities",
        "/rustfs/admin/v4/extensions/catalog",
        "/rustfs/admin/v4/cluster/snapshot",
        "/rustfs/admin/v3/idp/builtin/policy-entities?user=alice%40example.com&user=bob&group=ops%2Fteam&policy=read%20only",
    ] {
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("captured admin request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, expected);
    }
    handle.join().expect("admin test server finished");
}
