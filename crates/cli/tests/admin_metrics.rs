#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_response_test_server};
use jsonschema::Validator;
use serde_json::Value;

const METRICS_RESPONSE: &str = concat!(
    "{\"errors\":[],\"hosts\":[\"node-1\"],\"aggregated\":{\"scanner\":{\"collected\":\"2026-07-21T04:00:00Z\",\"current_cycle\":7,\"life_time_ops\":{\"scan_object\":42}}},\"by_host\":{\"node-1\":{\"rpc\":{\"collectedAt\":\"2026-07-21T04:00:01Z\",\"incomingBytes\":1024}}},\"by_disk\":{\"/data1\":{\"collected\":\"2026-07-21T04:00:02Z\",\"n_disks\":1,\"offline\":0}},\"final\":false}\n",
    "{\"errors\":[\"node-2 unavailable\"],\"hosts\":[\"node-1\"],\"aggregated\":{\"scanner\":{\"collected\":\"2026-07-21T04:00:03Z\",\"current_cycle\":8}},\"by_host\":{},\"by_disk\":{},\"final\":true}\n"
);

fn output_v3_validator() -> Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .expect("output v3 schema should parse");
    jsonschema::validator_for(&schema).expect("output v3 schema should compile")
}

#[test]
fn metrics_normalized_jsonl_preserves_numbers_labels_timestamps_and_partial_state() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/x-ndjson",
        METRICS_RESPONSE.to_string(),
    );
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "metrics",
            "myalias",
            "--scope",
            "scanner,disk",
            "--samples",
            "2",
            "--interval",
            "3s",
            "--host",
            "node-1",
            "--disk",
            "/data1",
            "--by-host",
            "--by-disk",
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
    let records = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).expect("JSON Lines record"))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["schema_version"], 3);
    assert_eq!(records[0]["type"], "metrics");
    assert_eq!(records[0]["data"]["scope"], "disk,scanner");
    assert_eq!(records[0]["data"]["collected_at"], "2026-07-21T04:00:02Z");
    assert_eq!(
        records[0]["data"]["raw"]["aggregated"]["scanner"]["current_cycle"],
        7
    );
    assert_eq!(records[1]["data"]["partial"], true);
    assert_eq!(records[1]["data"]["errors"][0], "node-2 unavailable");

    let samples = records[0]["data"]["samples"]
        .as_array()
        .expect("samples should be an array");
    let operation = samples
        .iter()
        .find(|sample| sample["labels"]["operation"] == "scan_object")
        .expect("operation label should be preserved");
    assert_eq!(operation["value"], 42);
    assert_eq!(operation["collected_at"], "2026-07-21T04:00:00Z");
    let host = samples
        .iter()
        .find(|sample| sample["labels"]["host"] == "node-1")
        .expect("host label should be preserved");
    assert_eq!(host["value"], 1024);
    let disk = samples
        .iter()
        .find(|sample| {
            sample["labels"]["disk"] == "/data1"
                && sample["name"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("n_disks"))
        })
        .expect("disk label should be preserved");
    assert_eq!(disk["value"], 1);

    let validator = output_v3_validator();
    for record in &records {
        let errors = validator
            .iter_errors(record)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "invalid v3 output:\n{}",
            errors.join("\n")
        );
    }

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/metrics?disks=%2Fdata1&hosts=node-1&interval=3s&n=2&types=3&by-disk=true&by-host=true"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn metrics_raw_format_emits_bounded_server_records_without_v3_wrapping() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let first_record = METRICS_RESPONSE
        .lines()
        .next()
        .expect("first metrics record");
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "200 OK",
        "application/x-ndjson",
        format!("{first_record}\n"),
    );
    let output = Command::new(rc_binary())
        .args([
            "admin",
            "metrics",
            "myalias",
            "--metrics-format",
            "raw",
            "--scope",
            "scanner",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("raw stdout should be JSON");
    assert!(value.get("schema_version").is_none());
    assert_eq!(value["aggregated"]["scanner"]["current_cycle"], 7);
    handle.join().expect("admin test server finished");
}

#[test]
fn metrics_empty_response_is_explicit() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) =
        start_admin_response_test_server("200 OK", "application/x-ndjson", String::new());
    let output = Command::new(rc_binary())
        .args(["admin", "metrics", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("No metric snapshots returned"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn metrics_permission_denial_and_unsupported_have_distinct_exit_codes() {
    for (status, expected_code) in [("403 Forbidden", 4), ("404 Not Found", 7)] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) = start_admin_response_test_server(
            status,
            "application/json",
            r#"{"code":"AccessDenied","message":"denied or unavailable"}"#.to_string(),
        );
        let output = Command::new(rc_binary())
            .args(["admin", "metrics", "myalias"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(output.status.code(), Some(expected_code));
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn metrics_rejects_malformed_and_oversized_records() {
    for (label, body) in [
        ("malformed", "{not-json}\n".to_string()),
        (
            "oversized",
            format!("{{\"padding\":\"{}\"}}\n", "x".repeat(1024 * 1024)),
        ),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) =
            start_admin_response_test_server("200 OK", "application/x-ndjson", body);
        let output = Command::new(rc_binary())
            .args(["admin", "metrics", "myalias"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(output.status.code(), Some(1), "case: {label}");
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn metrics_samples_above_server_limit_are_usage_errors_before_network_access() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args(["admin", "metrics", "missing", "--samples", "121"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("120"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
