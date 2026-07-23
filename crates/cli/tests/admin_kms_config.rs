#![cfg(not(windows))]

mod admin_support;

use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_response_test_server};
use jsonschema::Validator;
use serde_json::Value;

const LOCAL_CONFIG: &str = r#"{
  "backend_type":"Local",
  "key_dir":"/var/lib/rustfs/kms",
  "master_key":"LOCAL_MASTER_MUST_NOT_APPEAR",
  "file_permissions":384,
  "default_key_id":"archive-key"
}"#;

const VAULT_KV2_CONFIG: &str = r#"{
  "backend_type":"VaultKV2",
  "address":"https://vault.example:8200",
  "auth_method":{"Token":{"token":"VAULT_TOKEN_MUST_NOT_APPEAR"}},
  "namespace":null,
  "mount_path":"transit",
  "kv_mount":"secret",
  "key_path_prefix":"rustfs/kms/keys"
}"#;

const VAULT_TRANSIT_CONFIG: &str = r#"{
  "backend_type":"VaultTransit",
  "address":"https://vault.example:8200",
  "auth_method":{"AppRole":{"role_id":"role-id","secret_id":"APPROLE_MUST_NOT_APPEAR"}},
  "mount_path":"transit"
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

fn protected_config(contents: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("create config file");
    file.write_all(contents.as_bytes())
        .expect("write config file");
    file.as_file()
        .set_permissions(Permissions::from_mode(0o600))
        .expect("protect config file");
    file
}

#[test]
fn kms_configure_accepts_all_native_backend_shapes_from_protected_files() {
    for (config, expected_backend) in [
        (LOCAL_CONFIG, "Local"),
        (VAULT_KV2_CONFIG, "VaultKV2"),
        (VAULT_TRANSIT_CONFIG, "VaultTransit"),
    ] {
        let config_dir = tempfile::tempdir().expect("create config dir");
        let config_file = protected_config(config);
        let response = r#"{"success":true,"message":"configured with secret MUST_NOT_APPEAR","status":"Configured"}"#;
        let (endpoint, receiver, handle) =
            start_admin_response_test_server("200 OK", "application/json", response.to_string());

        let output = Command::new(rc_binary())
            .args([
                "--json",
                "admin",
                "kms",
                "configure",
                "myalias",
                "--config-file",
                config_file.path().to_str().expect("UTF-8 config path"),
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
        assert!(!stdout.contains("MUST_NOT_APPEAR"));
        let value: Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
        assert_eq!(value["data"]["operation"], "configure");
        assert_eq!(value["data"]["state"], "configured");
        assert_valid_v3(&value);

        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("captured configure request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, "/rustfs/admin/v3/kms/configure");
        let body: Value =
            serde_json::from_slice(&request.body).expect("configure body should be JSON");
        assert_eq!(body["backend_type"], expected_backend);
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn kms_configure_accepts_stdin_without_echoing_sensitive_input() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let response = r#"{"success":true,"message":"configured","status":"Configured"}"#;
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", response.to_string());

    let mut child = Command::new(rc_binary())
        .args(["--json", "admin", "kms", "configure", "myalias", "--stdin"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rc command");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(VAULT_KV2_CONFIG.as_bytes())
        .expect("write KMS config to stdin");
    let output = child.wait_with_output().expect("wait for rc command");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(!stdout.contains("VAULT_TOKEN_MUST_NOT_APPEAR"));
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured configure request");
    assert_eq!(request.target, "/rustfs/admin/v3/kms/configure");
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_configure_rejects_insecure_oversized_and_malformed_inputs_before_network() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let insecure = protected_config(LOCAL_CONFIG);
    insecure
        .as_file()
        .set_permissions(Permissions::from_mode(0o644))
        .expect("make config insecure");
    let malformed = protected_config(r#"{"backend_type":"Local","master_key":"SECRET"}"#);
    let oversized = protected_config(&"x".repeat(1024 * 1024 + 1));

    for file in [&insecure, &malformed, &oversized] {
        let output = Command::new(rc_binary())
            .args([
                "--json",
                "admin",
                "kms",
                "configure",
                "myalias",
                "--config-file",
                file.path().to_str().expect("UTF-8 config path"),
            ])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env(
                "RC_HOST_myalias",
                "http://ACCESS_KEY:SECRET_KEY@127.0.0.1:9",
            )
            .output()
            .expect("run rc command");
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
        assert!(!stderr.contains("SECRET"));
        let value: Value = serde_json::from_str(&stderr).expect("stderr should be JSON");
        assert_eq!(value["error"]["type"], "usage_error");
        assert_valid_v3(&value);
    }
}

#[test]
fn kms_reconfigure_uses_native_route_and_sanitizes_rejected_response() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let config_file = protected_config(VAULT_TRANSIT_CONFIG);
    let success = r#"{"success":true,"message":"reconfigured","status":"Running"}"#;
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", success.to_string());
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "kms",
            "reconfigure",
            "myalias",
            "--config-file",
            config_file.path().to_str().expect("UTF-8 config path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["data"]["operation"], "reconfigure");
    assert_eq!(value["data"]["state"], "running");
    assert_valid_v3(&value);
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured reconfigure request");
    assert_eq!(request.target, "/rustfs/admin/v3/kms/reconfigure");
    handle.join().expect("admin test server finished");

    let rejected =
        r#"{"success":false,"message":"invalid token APPROLE_MUST_NOT_APPEAR","status":"Error"}"#;
    let (endpoint, _receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", rejected.to_string());
    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "kms",
            "reconfigure",
            "myalias",
            "--config-file",
            config_file.path().to_str().expect("UTF-8 config path"),
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(!stderr.contains("APPROLE_MUST_NOT_APPEAR"));
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_start_maps_success_unconfigured_and_malformed_states() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    for (body, expected_code, expected_type) in [
        (
            r#"{"success":true,"message":"started","status":"Running"}"#,
            0,
            None,
        ),
        (
            r#"{"success":false,"message":"KMS service is not configured: MUST_NOT_APPEAR","status":"NotConfigured"}"#,
            5,
            Some("not_found"),
        ),
        ("{not-json}", 1, Some("general_error")),
    ] {
        let (endpoint, _receiver, handle) =
            start_admin_response_test_server("200 OK", "application/json", body.to_string());
        let output = Command::new(rc_binary())
            .args(["--json", "admin", "kms", "start", "myalias"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");
        assert_eq!(output.status.code(), Some(expected_code));
        let bytes = if expected_code == 0 {
            &output.stdout
        } else {
            &output.stderr
        };
        let text = String::from_utf8(bytes.clone()).expect("output should be UTF-8");
        assert!(!text.contains("MUST_NOT_APPEAR"));
        let value: Value = serde_json::from_str(&text).expect("output should be JSON");
        if let Some(expected_type) = expected_type {
            assert_eq!(value["error"]["type"], expected_type);
        } else {
            assert_eq!(value["data"]["operation"], "start");
        }
        assert_valid_v3(&value);
        handle.join().expect("admin test server finished");
    }
}

#[test]
fn kms_restart_requires_confirmation_and_posts_force_start() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let refused = Command::new(rc_binary())
        .args(["admin", "kms", "restart", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_myalias",
            "http://ACCESS_KEY:SECRET_KEY@127.0.0.1:9",
        )
        .output()
        .expect("run rc command");
    assert_eq!(refused.status.code(), Some(2));

    let response = r#"{"success":true,"message":"restarted","status":"Running"}"#;
    let (endpoint, receiver, handle) =
        start_admin_response_test_server("200 OK", "application/json", response.to_string());
    let output = Command::new(rc_binary())
        .args(["--json", "admin", "kms", "restart", "myalias", "--yes"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(value["data"]["operation"], "restart");
    assert_valid_v3(&value);
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured restart request");
    assert_eq!(request.target, "/rustfs/admin/v3/kms/start");
    let body: Value = serde_json::from_slice(&request.body).expect("restart body should be JSON");
    assert_eq!(body["force"], true);
    handle.join().expect("admin test server finished");

    let unavailable =
        r#"{"success":false,"message":"storage unavailable MUST_NOT_APPEAR","status":"Error"}"#;
    let (endpoint, _receiver, handle) = start_admin_response_test_server(
        "503 Service Unavailable",
        "application/json",
        unavailable.to_string(),
    );
    let output = Command::new(rc_binary())
        .args(["--json", "admin", "kms", "restart", "myalias", "--yes"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .output()
        .expect("run rc command");
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(!stderr.contains("MUST_NOT_APPEAR"));
    let value: Value = serde_json::from_str(&stderr).expect("stderr should be JSON");
    assert_eq!(value["error"]["type"], "network_error");
    assert_valid_v3(&value);
    handle.join().expect("admin test server finished");
}

#[test]
fn kms_stop_requires_confirmation_and_sanitizes_permission_denial() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let refused = Command::new(rc_binary())
        .args(["admin", "kms", "stop", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_myalias",
            "http://ACCESS_KEY:SECRET_KEY@127.0.0.1:9",
        )
        .output()
        .expect("run rc command");
    assert_eq!(refused.status.code(), Some(2));

    for (status, body, expected_code, expected_type) in [
        (
            "200 OK",
            r#"{"success":true,"message":"stopped","status":"Configured"}"#,
            0,
            None,
        ),
        (
            "403 Forbidden",
            r#"{"message":"denied VAULT_TOKEN_MUST_NOT_APPEAR"}"#,
            4,
            Some("auth_error"),
        ),
    ] {
        let (endpoint, _receiver, handle) =
            start_admin_response_test_server(status, "application/json", body.to_string());
        let output = Command::new(rc_binary())
            .args(["--json", "admin", "kms", "stop", "myalias", "--yes"])
            .env("RC_CONFIG_DIR", config_dir.path())
            .env("RC_HOST_myalias", rc_host_alias(&endpoint))
            .output()
            .expect("run rc command");
        assert_eq!(output.status.code(), Some(expected_code));
        let bytes = if expected_code == 0 {
            &output.stdout
        } else {
            &output.stderr
        };
        let text = String::from_utf8(bytes.clone()).expect("output should be UTF-8");
        assert!(!text.contains("VAULT_TOKEN_MUST_NOT_APPEAR"));
        let value: Value = serde_json::from_str(&text).expect("output should be JSON");
        if let Some(expected_type) = expected_type {
            assert_eq!(value["error"]["type"], expected_type);
        } else {
            assert_eq!(value["data"]["operation"], "stop");
        }
        assert_valid_v3(&value);
        handle.join().expect("admin test server finished");
    }
}
