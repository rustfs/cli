#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_response_test_server};
use serde_json::Value;

const MANUAL_TRANSITION_RESPONSE: &str = r#"{
  "state":"completed",
  "mode":"enqueue_only",
  "job_id":null,
  "status_endpoint":null,
  "report":{
    "bucket":"photos",
    "prefix":"logs/",
    "tier":"COLDTIER",
    "dry_run":true,
    "lifecycle_config_found":true,
    "scanned":3,
    "eligible":2,
    "enqueued":0,
    "dry_run_eligible":2,
    "skipped_not_transition":1,
    "skipped_tier":0,
    "skipped_delete_marker":0,
    "skipped_directory":0,
    "skipped_replication":0,
    "skipped_already_transitioned":1,
    "skipped_already_in_flight":0,
    "skipped_queue_full":0,
    "skipped_queue_closed":0,
    "skipped_queue_timeout":0,
    "truncated_by_limit":false,
    "truncated_by_duration":true
  }
}"#;

const MANUAL_TRANSITION_CONTROL_RESPONSE: &str = r#"{
  "state":"completed\nspoofed",
  "mode":"enqueue_only",
  "job_id":null,
  "status_endpoint":null,
  "report":{
    "bucket":"photos",
    "prefix":"logs/\nnext",
    "tier":"COLD\tTIER",
    "dry_run":true,
    "lifecycle_config_found":true,
    "scanned":1,
    "eligible":1,
    "enqueued":0,
    "dry_run_eligible":1,
    "skipped_not_transition":0,
    "skipped_tier":0,
    "skipped_delete_marker":0,
    "skipped_directory":0,
    "skipped_replication":0,
    "skipped_already_transitioned":0,
    "skipped_already_in_flight":0,
    "skipped_queue_full":0,
    "skipped_queue_closed":0,
    "skipped_queue_timeout":0,
    "truncated_by_limit":false,
    "truncated_by_duration":false
  }
}"#;

const MANUAL_TRANSITION_LEGACY_RESPONSE: &str = r#"{
  "state":"completed",
  "mode":"enqueue_only",
  "job_id":null,
  "status_endpoint":null,
  "report":{
    "bucket":"photos",
    "prefix":"logs/",
    "tier":"COLDTIER",
    "dry_run":true,
    "lifecycle_config_found":true,
    "scanned":1,
    "eligible":1,
    "enqueued":0,
    "dry_run_eligible":1,
    "skipped_not_transition":0,
    "skipped_tier":0,
    "skipped_delete_marker":0,
    "skipped_directory":0,
    "skipped_replication":0,
    "skipped_already_in_flight":0,
    "skipped_queue_full":0,
    "skipped_queue_closed":0,
    "skipped_queue_timeout":0,
    "truncated_by_limit":false
  }
}"#;

#[test]
fn ilm_transition_run_json_calls_manual_transition_endpoint() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        MANUAL_TRANSITION_RESPONSE.to_string(),
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "ilm",
            "transition",
            "run",
            "myalias",
            "photos",
            "--prefix",
            "logs/",
            "--tier",
            "COLDTIER",
            "--dry-run",
            "--max-objects",
            "25",
            "--max-duration-seconds",
            "30",
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
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "manual_transition_run");
    assert_eq!(value["data"]["state"], "completed");
    assert_eq!(value["data"]["report"]["dry_run_eligible"], 2);
    assert_eq!(value["data"]["report"]["skipped_already_transitioned"], 1);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/ilm/transition/run?bucket=photos&prefix=logs%2F&dryRun=true&maxObjects=25&maxDurationSeconds=30&tier=COLDTIER"
    );
    assert!(request.body.is_empty());
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_run_human_output_exposes_counts_without_keys() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let response = MANUAL_TRANSITION_RESPONSE
        .replace(
            "\"state\":\"completed\"",
            "\"state\":\"completed\\u001b[31m\"",
        )
        .replace(
            "\"mode\":\"enqueue_only\"",
            "\"mode\":\"enqueue_only\\u001b[31m\"",
        )
        .replace("\"prefix\":\"logs/\"", "\"prefix\":\"logs/\\u001b[31m\"")
        .replace("\"tier\":\"COLDTIER\"", "\"tier\":\"COLDTIER\\u001b[31m\"");
    let (endpoint, _receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", response);

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "ilm",
            "transition",
            "run",
            "myalias",
            "photos",
            "--prefix",
            "logs/",
            "--dry-run",
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
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("Manual Transition Run"), "stdout: {stdout}");
    assert!(stdout.contains("Eligible:       2"), "stdout: {stdout}");
    assert!(stdout.contains("Already moved:  1"), "stdout: {stdout}");
    assert!(stdout.contains("Duration hit:   true"), "stdout: {stdout}");
    assert!(!stdout.contains("next_marker"), "stdout: {stdout}");
    assert!(!stdout.contains("\x1b["), "stdout: {stdout}");
    assert!(stdout.contains("\\u{1b}"), "stdout: {stdout}");
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_run_human_output_sanitizes_server_text() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        MANUAL_TRANSITION_CONTROL_RESPONSE.to_string(),
    );

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "ilm",
            "transition",
            "run",
            "myalias",
            "photos",
            "--dry-run",
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
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(
        stdout.contains("State:          completed\\nspoofed"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Prefix:         logs/\\nnext"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Tier:           COLD\\tTIER"),
        "stdout: {stdout}"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_run_accepts_legacy_response_without_duration_flag() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        MANUAL_TRANSITION_LEGACY_RESPONSE.to_string(),
    );

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "ilm",
            "transition",
            "run",
            "myalias",
            "photos",
            "--dry-run",
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
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("Already moved:  0"), "stdout: {stdout}");
    assert!(stdout.contains("Duration hit:   false"), "stdout: {stdout}");
    handle.join().expect("admin test server finished");
}
