#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_response_test_server, start_admin_sequence_test_server,
};
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

const MANUAL_TRANSITION_ASYNC_RESPONSE: &str = r#"{
  "state":"accepted",
  "mode":"durable_job",
  "job_id":"11111111-1111-4111-8111-111111111111",
  "status_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "report":{
    "bucket":"photos",
    "prefix":"logs/",
    "tier":"COLDTIER",
    "dry_run":false,
    "lifecycle_config_found":true,
    "scanned":0,
    "eligible":0,
    "enqueued":0,
    "dry_run_eligible":0,
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

const MANUAL_TRANSITION_JOB_RUNNING_RESPONSE: &str = r#"{
  "status":"running",
  "mode":"durable_job",
  "job_id":"11111111-1111-4111-8111-111111111111",
  "status_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_requested":false,
  "bucket":"photos",
  "prefix":"logs/",
  "tier":"COLDTIER",
  "dry_run":false,
  "created_at_unix_nanos":100,
  "updated_at_unix_nanos":200,
  "completed_at_unix_nanos":null,
  "report":{
    "bucket":"photos",
    "prefix":"logs/",
    "tier":"COLDTIER",
    "dry_run":false,
    "lifecycle_config_found":true,
    "scanned":3,
    "eligible":2,
    "enqueued":2,
    "dry_run_eligible":0,
    "skipped_not_transition":1,
    "skipped_tier":0,
    "skipped_delete_marker":0,
    "skipped_directory":0,
    "skipped_replication":0,
    "skipped_already_transitioned":0,
    "skipped_already_in_flight":0,
    "skipped_queue_full":0,
    "skipped_queue_closed":0,
    "skipped_queue_timeout":0,
    "transition_completed":1,
    "transition_failed":0,
    "truncated_by_limit":false,
    "truncated_by_duration":false
  },
  "queue_snapshot":{
    "queue_capacity":1000,
    "queued":1,
    "active":1,
    "workers":4,
    "queue_full":0,
    "queue_send_timeout":0,
    "compensation_pending":0,
    "compensation_running":0
  },
  "failure_reason":null
}"#;

const MANUAL_TRANSITION_JOB_COMPLETED_RESPONSE: &str = r#"{
  "status":"completed",
  "mode":"durable_job",
  "job_id":"11111111-1111-4111-8111-111111111111",
  "status_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_requested":false,
  "bucket":"photos",
  "prefix":"logs/",
  "tier":"COLDTIER",
  "dry_run":false,
  "created_at_unix_nanos":100,
  "updated_at_unix_nanos":300,
  "completed_at_unix_nanos":300,
  "report":{
    "bucket":"photos",
    "prefix":"logs/",
    "tier":"COLDTIER",
    "dry_run":false,
    "lifecycle_config_found":true,
    "scanned":3,
    "eligible":2,
    "enqueued":2,
    "dry_run_eligible":0,
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
    "transition_completed":2,
    "transition_failed":0,
    "truncated_by_limit":false,
    "truncated_by_duration":false,
    "cancelled":false
  },
  "queue_snapshot":{
    "queue_capacity":1000,
    "queued":0,
    "active":0,
    "workers":4,
    "queue_full":0,
    "queue_send_timeout":0,
    "compensation_pending":0,
    "compensation_running":0
  },
  "failure_reason":null
}"#;

const MANUAL_TRANSITION_JOB_CANCELLED_RESPONSE: &str = r#"{
  "status":"cancelled",
  "mode":"durable_job",
  "job_id":"11111111-1111-4111-8111-111111111111",
  "status_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_endpoint":"/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111",
  "cancel_requested":true,
  "bucket":"photos",
  "prefix":"logs/\nspoof",
  "tier":"COLD\tTIER",
  "dry_run":false,
  "created_at_unix_nanos":100,
  "updated_at_unix_nanos":400,
  "completed_at_unix_nanos":400,
  "report":{
    "bucket":"photos",
    "prefix":"logs/\nspoof",
    "tier":"COLD\tTIER",
    "dry_run":false,
    "lifecycle_config_found":true,
    "scanned":1,
    "eligible":1,
    "enqueued":0,
    "dry_run_eligible":0,
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
    "transition_completed":0,
    "transition_failed":0,
    "truncated_by_limit":false,
    "truncated_by_duration":false,
    "cancelled":true
  },
  "queue_snapshot":{
    "queue_capacity":1000,
    "queued":0,
    "active":0,
    "workers":4,
    "queue_full":0,
    "queue_send_timeout":0,
    "compensation_pending":0,
    "compensation_running":0
  },
  "failure_reason":"operator requested cancellation\nnext"
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

#[test]
fn ilm_transition_run_async_json_exposes_job_endpoints() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "202 Accepted",
        "application/json",
        MANUAL_TRANSITION_ASYNC_RESPONSE.to_string(),
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
            "--async",
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
    assert_eq!(value["type"], "manual_transition_run");
    assert_eq!(value["data"]["state"], "accepted");
    assert_eq!(value["data"]["mode"], "durable_job");
    assert_eq!(
        value["data"]["job_id"],
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(
        value["data"]["cancel_endpoint"],
        "/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111"
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/ilm/transition/run?bucket=photos&prefix=logs%2F&dryRun=false&maxObjects=10000&tier=COLDTIER&async=true"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_status_json_calls_job_endpoint() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        MANUAL_TRANSITION_JOB_RUNNING_RESPONSE.to_string(),
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "ilm",
            "transition",
            "status",
            "myalias",
            "11111111-1111-4111-8111-111111111111",
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
    assert_eq!(value["type"], "manual_transition_job_status");
    assert_eq!(value["data"]["status"], "running");
    assert_eq!(value["data"]["queue_snapshot"]["queued"], 1);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111"
    );
    assert!(request.body.is_empty());
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_cancel_human_sanitizes_job_text() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/json",
        MANUAL_TRANSITION_JOB_CANCELLED_RESPONSE.to_string(),
    );

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "ilm",
            "transition",
            "cancel",
            "myalias",
            "11111111-1111-4111-8111-111111111111",
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
        stdout.contains("Status:         cancelled"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("Cancel asked:   true"), "stdout: {stdout}");
    assert!(
        stdout.contains("Prefix:         logs/\\nspoof"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Tier:           COLD\\tTIER"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Failure:        operator requested cancellation\\nnext"),
        "stdout: {stdout}"
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "DELETE");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/ilm/transition/jobs/11111111-1111-4111-8111-111111111111"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_wait_polls_until_terminal_state() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", MANUAL_TRANSITION_JOB_RUNNING_RESPONSE),
        ("200 OK", MANUAL_TRANSITION_JOB_COMPLETED_RESPONSE),
    ]);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "ilm",
            "transition",
            "wait",
            "myalias",
            "11111111-1111-4111-8111-111111111111",
            "--poll-interval-seconds",
            "1",
            "--timeout-seconds",
            "10",
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
    assert_eq!(value["type"], "manual_transition_job_wait");
    assert_eq!(value["data"]["status"], "completed");

    let first = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured first status request");
    let second = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured second status request");
    assert_eq!(first.method, "GET");
    assert_eq!(second.method, "GET");
    assert_eq!(first.target, second.target);
    handle.join().expect("admin test server finished");
}

#[test]
fn ilm_transition_job_errors_preserve_exit_classes() {
    for (status, expected_code) in [
        ("403 Forbidden", 4),
        ("404 Not Found", 5),
        ("409 Conflict", 6),
        ("501 Not Implemented", 7),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) = start_admin_response_test_server(
            status,
            "application/xml",
            "<Error><Code>NoSuchKey</Code><Message>job unavailable</Message></Error>".to_string(),
        );

        let output = Command::new(rc_binary())
            .args([
                "--json",
                "admin",
                "ilm",
                "transition",
                "status",
                "myalias",
                "11111111-1111-4111-8111-111111111111",
            ])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "status {status}, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        handle.join().expect("admin test server finished");
    }
}
