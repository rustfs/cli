#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};
use jsonschema::Validator;
use serde_json::Value;

const INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.10","drives":[]}]}}"#;
const RUNTIME_RESPONSE: &str = r#"{"summary":{"observability":{"state":"supported"},"userspace_profiling":{"state":"disabled","reason":"disabled by configuration"},"memory_sampling":{"state":"unsupported","reason":"not available on this platform"},"platform":{"state":"supported"},"topology":{"state":"unknown","reason":"storage is initializing"},"cluster_snapshot":{"state":"supported"}},"cluster_snapshot_path":"/rustfs/admin/v4/cluster/snapshot","cluster_snapshot_summary":{"state":"supported"},"observability":{},"workload_admission":{},"topology":null,"topology_status":{"state":"unknown"}}"#;
const EXTENSIONS_RESPONSE: &str = r#"{"extensions":[],"runtime_capabilities":{},"cluster_snapshot":{},"external_plugin_flow":{}}"#;
const SNAPSHOT_RESPONSE: &str = r#"{"snapshot":{"summary":{"runtime":{"state":"supported"},"topology":{"state":"supported"},"membership":{"state":"supported"},"peer_health":{"state":"supported"},"rpc_boundary":{"state":"supported"},"observability":{"state":"supported"},"workload_admission":{"state":"supported"},"actionable_pressure":{"state":"disabled"}},"runtime_capabilities_path":"/rustfs/admin/v4/runtime/capabilities","extensions_catalog_path":"/rustfs/admin/v4/extensions/catalog"}}"#;

fn output_v3_validator() -> Validator {
    let schema_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("schemas/output_v3.json");
    let schema = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", schema_path.display()));
    let schema: Value = serde_json::from_str(&schema).expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

fn assert_valid_v3(value: &Value) {
    let validator = output_v3_validator();
    let errors = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "capability output must satisfy output v3:\n{}",
        errors.join("\n")
    );
}

#[test]
fn capabilities_json_success_satisfies_output_v3() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        ("200 OK", RUNTIME_RESPONSE),
        ("200 OK", EXTENSIONS_RESPONSE),
        ("200 OK", SNAPSHOT_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "capabilities", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["data"]["api_version"], "v4");
    let memory_sampling = value["data"]["capabilities"]
        .as_array()
        .expect("capabilities should be an array")
        .iter()
        .find(|capability| capability["name"] == "runtime.memory-sampling")
        .expect("runtime memory sampling capability");
    assert_eq!(memory_sampling["support"], "unsupported");
    assert_ne!(memory_sampling["support"], "stub");
    assert_valid_v3(&value);

    for expected in [
        "/rustfs/admin/v3/info",
        "/rustfs/admin/v4/runtime/capabilities",
        "/rustfs/admin/v4/extensions/catalog",
        "/rustfs/admin/v4/cluster/snapshot",
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
fn capabilities_json_version_gate_satisfies_output_v3() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        (
            "404 Not Found",
            r#"{"code":"NotImplemented","message":"route body must not override HTTP 404"}"#,
        ),
    ]);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "capabilities", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["data"]["api_version"], Value::Null);
    assert_eq!(value["data"]["capabilities"][0]["support"], "unsupported");
    assert_valid_v3(&value);
    handle.join().expect("admin test server finished");
}

#[test]
fn capabilities_json_http_501_reports_stub_support() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        (
            "501 Not Implemented",
            r#"{"code":"NotImplemented","message":"route is not implemented"}"#,
        ),
    ]);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "capabilities", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["data"]["api_version"], "v4");
    assert_eq!(value["data"]["capabilities"][0]["support"], "stub");
    assert_eq!(
        value["data"]["capabilities"][0]["reason"],
        "route is not implemented"
    );
    assert_valid_v3(&value);
    handle.join().expect("admin test server finished");
}

#[test]
fn capabilities_json_auth_error_satisfies_output_v3() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", INFO_RESPONSE),
        (
            "403 Forbidden",
            r#"{"code":"NotImplemented","message":"Access denied"}"#,
        ),
    ]);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "capabilities", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(4));
    assert!(
        output.stdout.is_empty(),
        "errors must not be written to stdout"
    );
    let value: Value = serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
    assert_eq!(value["error"]["type"], "auth_error");
    assert_valid_v3(&value);
    handle.join().expect("admin test server finished");
}

#[test]
fn capabilities_missing_alias_keeps_top_level_error_contract() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "capabilities", "missing"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(5));
    let value: Value = serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
    assert!(value.get("schema_version").is_none());
    assert_eq!(value["details"]["type"], "not_found");
}
