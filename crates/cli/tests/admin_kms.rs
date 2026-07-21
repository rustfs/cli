#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_response_test_server, start_admin_sequence_test_server,
};
use jsonschema::Validator;
use serde_json::Value;

const RUNNING_STATUS: &str = r#"{
  "status":"Running",
  "backend_type":"VaultKV2",
  "healthy":true,
  "config_summary":{
    "backend_type":"VaultKV2",
    "default_key_id":"archive/key",
    "timeout_seconds":30,
    "retry_attempts":3,
    "enable_cache":true,
    "max_cached_keys":1000,
    "cache_ttl_seconds":3600,
    "cache_summary":{"max_keys":500,"ttl_seconds":900,"enable_metrics":true},
    "backend_summary":{
      "backend_type":"vault-kv2",
      "address":"https://user:MUST_NOT_APPEAR@vault.example/path?token=MUST_NOT_APPEAR",
      "auth_method_type":"approle",
      "has_stored_credentials":true,
      "skip_tls_verify":false,
      "token":"MUST_NOT_APPEAR",
      "secret_id":"MUST_NOT_APPEAR"
    }
  }
}"#;

const KEY_LIST: &str = r#"{
  "success":true,
  "message":"keys listed successfully",
  "keys":[{
    "key_id":"archive/key",
    "description":"Archive key",
    "algorithm":"AES_256",
    "usage":"EncryptDecrypt",
    "status":"Active",
    "version":2,
    "metadata":{"owner":"storage"},
    "tags":{"environment":"prod"},
    "created_at":"2026-07-21T00:00:00Z",
    "rotated_at":null,
    "created_by":"admin"
  }],
  "truncated":true,
  "next_marker":"next/key"
}"#;

const KEY_STATUS: &str = r#"{
  "success":true,
  "message":"Key described successfully",
  "key_metadata":{
    "key_id":"archive/key",
    "key_state":"Enabled",
    "key_usage":"EncryptDecrypt",
    "description":"Archive key",
    "creation_date":"2026-07-21T00:00:00Z",
    "deletion_date":null,
    "origin":"AWS_KMS",
    "key_manager":"CUSTOMER",
    "tags":{"environment":"prod"}
  }
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
fn kms_status_json_is_typed_and_redacts_unrecognized_secret_fields() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", RUNNING_STATUS.to_string());

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "kms", "status", "myalias"])
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
    assert!(!stdout.contains("MUST_NOT_APPEAR"));
    let value: Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    assert_eq!(value["type"], "kms");
    assert_eq!(value["data"]["operation"], "status");
    assert_eq!(value["data"]["state"], "running");
    assert_eq!(value["data"]["config"]["default_key_id"], "archive/key");
    assert_valid_v3(&value);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/kms/service-status");
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_status_unconfigured_is_a_successful_human_result() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let body =
        r#"{"status":"NotConfigured","backend_type":null,"healthy":null,"config_summary":null}"#;
    let (endpoint, _receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", body.to_string());

    let output = Command::new(rc_binary())
        .args(["admin", "kms", "status", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("State:          not-configured"));
    assert!(stdout.contains("Backend:        not configured"));
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_key_list_json_preserves_pagination_and_encoded_marker() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", KEY_LIST.to_string());

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "kms",
            "key",
            "list",
            "myalias",
            "--limit",
            "25",
            "--marker",
            "marker/with space",
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
    assert_eq!(value["data"]["operation"], "key_list");
    assert_eq!(value["data"]["keys"][0]["state"], "active");
    assert_eq!(
        value["data"]["pagination"]["continuation_token"],
        "next/key"
    );
    assert_valid_v3(&value);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/kms/keys?limit=25&marker=marker%2Fwith%20space"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_key_status_resolves_default_key_without_exporting_key_material() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_sequence_test_server(vec![("200 OK", RUNNING_STATUS), ("200 OK", KEY_STATUS)]);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "kms", "key", "status", "myalias"])
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
    assert_eq!(value["data"]["operation"], "key_status");
    assert_eq!(value["data"]["key"]["key_id"], "archive/key");
    assert!(value["data"]["key"].get("plaintext_key").is_none());
    assert!(value["data"]["key"].get("ciphertext_blob").is_none());
    assert_valid_v3(&value);

    let first = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured status request");
    let second = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured key request");
    assert_eq!(first.target, "/rustfs/admin/v3/kms/service-status");
    assert_eq!(second.target, "/rustfs/admin/v3/kms/keys/archive%2Fkey");
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_status_and_key_list_map_permission_and_route_absence() {
    for (command, status, expected_code, expected_type) in [
        (
            vec!["admin", "kms", "status", "myalias"],
            "403 Forbidden",
            4,
            "auth_error",
        ),
        (
            vec!["admin", "kms", "status", "myalias"],
            "404 Not Found",
            7,
            "unsupported_feature",
        ),
        (
            vec!["admin", "kms", "key", "list", "myalias"],
            "403 Forbidden",
            4,
            "auth_error",
        ),
        (
            vec!["admin", "kms", "key", "list", "myalias"],
            "404 Not Found",
            7,
            "unsupported_feature",
        ),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) = start_admin_response_test_server(
            status,
            "application/json",
            r#"{"code":"AccessDenied","message":"denied or unavailable"}"#.to_string(),
        );
        let output = Command::new(rc_binary())
            .arg("--json")
            .args(command)
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(output.status.code(), Some(expected_code));
        let value: Value = serde_json::from_slice(&output.stderr).expect("stderr should be JSON");
        assert_eq!(value["type"], "kms");
        assert_eq!(value["error"]["type"], expected_type);
        assert_valid_v3(&value);
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn kms_key_status_preserves_missing_key_and_malformed_exit_codes() {
    for (status, body, expected_code) in [
        (
            "404 Not Found",
            r#"{"code":"NoSuchKey","message":"missing key"}"#,
            5,
        ),
        ("200 OK", "{not-json}", 1),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let (endpoint, _receiver, handle) =
            start_admin_response_test_server(status, "application/json", body.to_string());
        let output = Command::new(rc_binary())
            .args(["admin", "kms", "key", "status", "myalias", "missing-key"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");

        assert_eq!(output.status.code(), Some(expected_code));
        handle.join().expect("admin test server finished");
    }
}
