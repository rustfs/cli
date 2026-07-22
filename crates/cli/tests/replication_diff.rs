#![cfg(not(windows))]

mod admin_support;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_sequence_test_server, start_admin_test_server,
};
use jsonschema::Validator;
use serde_json::Value;

fn output_v3_validator() -> Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let schema: Value = serde_json::from_str(&contents).expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

fn assert_valid_v3(value: &Value) {
    let errors = output_v3_validator()
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "replication diff output must satisfy output v3:\n{}",
        errors.join("\n")
    );
}

#[test]
fn replication_diff_emits_v3_json_and_forwards_prefix() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{
            "Entries":[{
                "Object":"reports/a.json",
                "VersionID":"v1",
                "Size":42,
                "IsDeleteMarker":false,
                "ReplicationStatus":"FAILED",
                "LastModified":"2026-07-21T04:00:00Z",
                "TargetDetail":{"attempts":2}
            }],
            "IsTruncated":false,
            "ScannedVersions":24,
            "ServerRevision":7
        }"#,
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "bucket",
            "replication",
            "diff",
            "myalias/source",
            "--prefix",
            "reports/2026 Q3/",
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
    let payload: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(payload["schema_version"], 3);
    assert_eq!(payload["type"], "replication");
    assert_eq!(payload["data"]["operation"], "diff");
    assert_eq!(payload["data"]["entries"][0]["version_id"], "v1");
    assert_eq!(payload["data"]["extensions"]["ServerRevision"], 7);
    assert_valid_v3(&payload);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/replication/diff?bucket=source&prefix=reports%2F2026%20Q3%2F"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn replication_diff_generic_404_is_v3_unsupported_error() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_sequence_test_server(vec![("404 Not Found", r#"{"message":"route missing"}"#)]);

    let output = Command::new(rc_binary())
        .args(["--json", "bucket", "replication", "diff", "myalias/source"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    let payload: Value = serde_json::from_slice(&output.stderr).expect("JSON error output");
    assert_eq!(payload["type"], "replication");
    assert_eq!(payload["error"]["type"], "unsupported_feature");
    assert_eq!(payload["error"]["capability"], "replication_diff");
    assert_valid_v3(&payload);

    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}
