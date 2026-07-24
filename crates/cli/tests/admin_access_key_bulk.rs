#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};

const INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
const RUNTIME_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled","reason":"disabled by configuration"},"memory_sampling":{"state":"unsupported","reason":"not available on this platform"},"platform":{"state":"supported"},"topology":{"state":"unknown"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"unknown"}}"#;
const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
const SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"disabled"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;

#[test]
fn bulk_access_keys_dispatches_advertised_mixed_providers_with_bounded_v3_output() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let builtin = r#"{"alice":{"serviceAccounts":[{"accessKey":"svc-builtin","parentUser":"alice","accountStatus":"on","secretKey":"secret-canary"}],"stsKeys":[]}}"#;
    let openid = r#"{"alice":{"serviceAccounts":[],"stsKeys":[{"accessKey":"sts-openid","parentUser":"alice","expiration":"2027-01-01T00:00:00Z","sessionToken":"token-canary"}]}}"#;
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        ("200 OK", RUNTIME_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", builtin),
        ("200 OK", openid),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "access-key",
            "ls",
            "myalias",
            "--provider",
            "openid,builtin",
            "--user",
            "alice",
            "--limit",
            "1",
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
    assert_eq!(value["type"], "iam_access_keys");
    assert_eq!(value["data"]["keys"][0]["provider"], "builtin");
    assert_eq!(value["data"]["keys"][0]["kind"], "service_account");
    assert_eq!(value["data"]["pagination"]["total"], 2);
    assert_eq!(value["data"]["pagination"]["truncated"], true);
    assert_eq!(value["data"]["pagination"]["next_offset"], 1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("secret-canary"));
    assert!(!stdout.contains("token-canary"));
    assert!(output.stderr.is_empty());

    for expected in [
        "/rustfs/admin/v3/info",
        "/rustfs/admin/v4/runtime/capabilities",
        "/rustfs/admin/v4/extensions/catalog",
        "/rustfs/admin/v4/cluster/snapshot",
        "/rustfs/admin/v3/list-access-keys-bulk?users=alice&listType=all",
        "/rustfs/admin/v3/idp/openid/list-access-keys-bulk?users=alice&listType=all",
    ] {
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("captured admin request");
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, expected);
    }
    handle.join().expect("admin test server finished");
}

#[test]
fn bulk_access_keys_retains_partial_failure_and_returns_non_success() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let builtin = r#"{"alice":{"serviceAccounts":[{"accessKey":"svc-builtin","parentUser":"alice"}],"stsKeys":[]}}"#;
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        ("200 OK", RUNTIME_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
        ("200 OK", builtin),
        ("403 Forbidden", r#"{"message":"secret-canary"}"#),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "access-key",
            "ls",
            "myalias",
            "--provider",
            "builtin,ldap",
            "--user",
            "alice",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
    assert_eq!(value["type"], "iam_access_keys");
    assert_eq!(value["status"], "error");
    assert_eq!(value["data"]["keys"][0]["access_key"], "svc-builtin");
    assert_eq!(value["data"]["failures"][0]["provider"], "ldap");
    assert_eq!(value["data"]["failures"][0]["type"], "auth_error");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-canary"));
    handle.join().expect("admin test server finished");
}
