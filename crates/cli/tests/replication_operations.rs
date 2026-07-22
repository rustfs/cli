//! Process-level contracts for bucket replication check and resync commands.

use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn run_rc(args: &[&str]) -> (Output, TempDir) {
    let config_dir = tempfile::tempdir().expect("create isolated config dir");
    let output = Command::new(env!("CARGO_BIN_EXE_rc"))
        .args(args)
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("execute rc");
    (output, config_dir)
}

fn assert_v3_usage_error(output: &Output, operation: &str) {
    assert_v3_error(output, operation, 2, "usage_error");
}

fn assert_v3_error(output: &Output, operation: &str, code: i32, error_type: &str) {
    assert_eq!(output.status.code(), Some(code));
    assert!(output.stdout.is_empty(), "JSON errors belong on stderr");
    let value: Value = serde_json::from_slice(&output.stderr).expect("v3 error JSON");
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "replication_operations");
    assert_eq!(value["status"], "error");
    assert_eq!(value["error"]["type"], error_type);
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(operation))
    );
}

#[test]
fn replication_check_requires_confirmation_before_alias_setup() {
    let (output, _config_dir) = run_rc(&[
        "--json",
        "bucket",
        "replication",
        "check",
        "missing-alias/source-bucket",
    ]);

    assert_v3_usage_error(&output, "check");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
}

#[test]
fn replication_check_missing_alias_is_not_found_before_network() {
    let (output, _config_dir) = run_rc(&[
        "--json",
        "bucket",
        "replication",
        "check",
        "missing-alias/source-bucket",
        "--yes",
    ]);

    assert_v3_error(&output, "check", 5, "not_found");
}

#[test]
fn replication_resync_start_requires_confirmation_before_alias_setup() {
    let (output, _config_dir) = run_rc(&[
        "--json",
        "bucket",
        "replication",
        "resync",
        "start",
        "missing-alias/source-bucket",
    ]);

    assert_v3_usage_error(&output, "resync_start");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
}

#[test]
fn replication_resync_start_missing_alias_is_not_found_before_network() {
    let (output, _config_dir) = run_rc(&[
        "--json",
        "bucket",
        "replication",
        "resync",
        "start",
        "missing-alias/source-bucket",
        "--yes",
    ]);

    assert_v3_error(&output, "resync_start", 5, "not_found");
}

#[test]
fn replication_resync_status_validates_target_before_alias_setup() {
    let (output, _config_dir) = run_rc(&[
        "--json",
        "bucket",
        "replication",
        "resync",
        "status",
        "missing-alias/source-bucket",
        "--target-arn",
        "not-an-arn",
    ]);

    assert_v3_usage_error(&output, "resync_status");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--target-arn"));
}

#[test]
fn replication_resync_status_missing_alias_is_not_found_before_network() {
    let (output, _config_dir) = run_rc(&[
        "--json",
        "bucket",
        "replication",
        "resync",
        "status",
        "missing-alias/source-bucket",
    ]);

    assert_v3_error(&output, "resync_status", 5, "not_found");
}
