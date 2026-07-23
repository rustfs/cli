#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_response_test_server};
use jsonschema::Validator;
use serde_json::Value;

const HEALTHY_SCANNER: &str = r#"{
  "enabled":true,
  "disabled_reason":null,
  "freshness":{"state":"fresh","last_cycle_end_unix_secs":1784606400,"max_expected_age_seconds":120,"reason":null},
  "metrics":{
    "collected_at":"2026-07-21T04:00:00Z",
    "current_cycle":7,
    "current_started":"2026-07-21T03:59:00Z",
    "current_scan_mode":"normal",
    "active_scan_paths":1,
    "active_paths":["photos/2026"],
    "last_cycle_end_unix_secs":1784606400,
    "last_cycle_result":"success",
    "last_cycle_duration_seconds":4.5,
    "last_cycle_objects_scanned":42,
    "last_cycle_directories_scanned":5,
    "last_cycle_bucket_drive_failures":0
  },
  "cycle_schedule":{"effective_interval_seconds":60,"clean_idle_backoff_enabled":false,"clean_idle_backoff_multiplier":1},
  "runtime_config":{"speed":{"value":"default","source":"default"},"cycle_interval_seconds":{"value":60,"source":"config"}}
}"#;

fn output_v3_validator() -> Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

fn assert_valid_v3(value: &Value) {
    let errors = output_v3_validator()
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "invalid v3 output:\n{}",
        errors.join("\n")
    );
}

#[test]
fn scanner_status_json_is_typed_v3_output() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", HEALTHY_SCANNER.to_string());

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "scanner", "status", "myalias"])
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
    assert_eq!(value["type"], "scanner_status");
    assert_eq!(value["data"]["health"], "healthy");
    assert_eq!(value["data"]["metrics"]["last_cycle_objects_scanned"], 42);
    assert_valid_v3(&value);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/scanner/status");
    handle.join().expect("admin test server finished");
}

#[test]
fn scanner_status_human_output_makes_stale_empty_and_partial_states_explicit() {
    for (label, body, expected) in [
        (
            "stale",
            HEALTHY_SCANNER
                .replace("\"state\":\"fresh\"", "\"state\":\"stale\"")
                .replace("\"reason\":null", "\"reason\":\"last cycle is too old\""),
            "Health:         stale",
        ),
        (
            "empty",
            HEALTHY_SCANNER
                .replace("\"state\":\"fresh\"", "\"state\":\"unknown\"")
                .replace("\"current_cycle\":7", "\"current_cycle\":0")
                .replace(
                    "\"last_cycle_end_unix_secs\":1784606400",
                    "\"last_cycle_end_unix_secs\":0",
                )
                .replace(
                    "\"last_cycle_result\":\"success\"",
                    "\"last_cycle_result\":\"unknown\"",
                ),
            "Health:         empty",
        ),
        (
            "partial",
            HEALTHY_SCANNER.replace(
                "\"last_cycle_result\":\"success\"",
                "\"last_cycle_result\":\"partial\",\"last_cycle_partial_reason\":\"directories\"",
            ),
            "Health:         partial",
        ),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) =
            start_admin_response_test_server("200 OK", "application/json", body);
        let output = Command::new(rc_binary())
            .args(["admin", "scanner", "status", "myalias"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert!(
            output.status.success(),
            "{label} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
        assert!(stdout.contains(expected), "{label} stdout: {stdout}");
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn scanner_status_permission_denial_and_unsupported_have_distinct_exit_codes() {
    for (status, expected_code, expected_type) in [
        ("403 Forbidden", 4, "auth_error"),
        ("404 Not Found", 7, "unsupported_feature"),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) = start_admin_response_test_server(
            status,
            "application/json",
            r#"{"code":"AccessDenied","message":"denied or unavailable"}"#.to_string(),
        );
        let output = Command::new(rc_binary())
            .args(["--json", "admin", "scanner", "status", "myalias"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(output.status.code(), Some(expected_code));
        assert!(output.stdout.is_empty());
        let value: Value = serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
        assert_eq!(value["type"], "scanner_status");
        assert_eq!(value["error"]["type"], expected_type);
        assert_valid_v3(&value);
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn scanner_status_rejects_malformed_payload() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", "{not-json}".to_string());
    let output = Command::new(rc_binary())
        .args(["admin", "scanner", "status", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(1));
    handle.join().expect("admin test server finished");
}
