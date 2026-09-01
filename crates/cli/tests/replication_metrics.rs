//! Process-level contracts for MinIO-compatible replication metrics.

#![cfg(not(windows))]

mod admin_support;

use std::process::{Command, Output};
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

const MINIO_METRICS: &str =
    include_str!("../../core/tests/fixtures/replication_metrics_minio_v1.json");

fn run_status(json: bool) -> (Output, admin_support::CapturedAdminRequest) {
    let config_dir = tempfile::tempdir().expect("create isolated config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(MINIO_METRICS);
    let mut command = Command::new(rc_binary());
    command.arg("--no-color");
    if json {
        command.arg("--json");
    } else {
        command.args(["--format", "human"]);
    }
    let output = command
        .args(["bucket", "replication", "status", "myalias/source-bucket"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("execute replication status");
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured replication metrics request");
    handle.join().expect("admin test server finished");
    (output, request)
}

#[test]
fn replication_status_human_accepts_minio_metrics() {
    let (output, request) = run_status(false);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 human output");
    assert!(
        stdout.contains("provider=available, cluster=complete (1/1 nodes)"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Totals: replicated 1 / 0 objects, 20 / 0 bytes"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("1 / 20 bytes  0 / 0 bytes  unavailable  legacy_unknown"),
        "stdout: {stdout}"
    );
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/replicationmetrics?bucket=source-bucket"
    );
}

#[test]
fn replication_status_json_accepts_minio_metrics() {
    let (output, request) = run_status(true);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("v3 replication JSON");
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../schemas/output_v3.json"))
            .expect("output-v3 schema");
    let validator = jsonschema::validator_for(&schema).expect("compiled output-v3 schema");
    let errors = validator
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "replication status violates output-v3 schema: {}",
        errors.join("; ")
    );
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "replication");
    assert_eq!(value["status"], "success");
    assert_eq!(value["data"]["availability"], "available");
    assert_eq!(value["data"]["cluster"]["state"], "complete");
    assert_eq!(value["data"]["totals"]["replicated_count"], 1);
    assert_eq!(value["data"]["totals"]["replicated_size_bytes"], 20);
    assert_eq!(value["data"]["targets"][0]["replicated_count"], 1);
    assert_eq!(value["data"]["targets"][0]["replicated_size_bytes"], 20);
    assert_eq!(
        value["data"]["targets"][0]["latency"]["scope"],
        "unavailable"
    );
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/replicationmetrics?bucket=source-bucket"
    );
}
