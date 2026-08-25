#![cfg(not(windows))]

mod admin_support;

use std::fs;
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

#[test]
fn service_account_create_accepts_inline_policy_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"credentials":{"accessKey":"service-key","secretKey":"service-secret"}}"#,
    );
    let policy = r#"{"Version":"2012-10-17","Statement":[]}"#;

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "service-account",
            "create",
            "myalias",
            "service-key",
            "service-secret",
            "--policy-json",
            policy,
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
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/rustfs/admin/v3/add-service-accounts");
    handle.join().expect("admin test server finished");
}

#[test]
fn service_account_create_sends_target_user_for_another_parent() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"credentials":{"accessKey":"service-key","secretKey":"service-secret"}}"#,
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "service-account",
            "create",
            "myalias",
            "service-key",
            "service-secret",
            "--user",
            "test-user",
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
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/rustfs/admin/v3/add-service-accounts");
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("create body should be JSON");
    assert_eq!(body["targetUser"], "test-user");
    assert_eq!(body["accessKey"], "service-key");
    assert!(body.get("secretKey").is_some());
    handle.join().expect("admin test server finished");
}

#[test]
fn service_account_create_omits_target_user_when_parent_is_the_caller() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"credentials":{"accessKey":"service-key","secretKey":"service-secret"}}"#,
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "service-account",
            "create",
            "myalias",
            "service-key",
            "service-secret",
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
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("create body should be JSON");
    assert!(body.get("targetUser").is_none());
    handle.join().expect("admin test server finished");
}

#[test]
fn service_account_create_omits_empty_user_flag_from_request_body() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"credentials":{"accessKey":"service-key","secretKey":"service-secret"}}"#,
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "service-account",
            "create",
            "myalias",
            "service-key",
            "service-secret",
            "--user",
            "",
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
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("create body should be JSON");
    assert!(body.get("targetUser").is_none());
    handle.join().expect("admin test server finished");
}

#[test]
fn service_account_create_rejects_invalid_inline_policy_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args([
            "admin",
            "service-account",
            "create",
            "myalias",
            "service-key",
            "service-secret",
            "--policy-json",
            "{not-json}",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Policy JSON is not valid JSON"));
}

#[test]
fn service_account_update_dispatches_to_update_endpoint() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let policy_dir = tempfile::tempdir().expect("create policy dir");
    let policy_path = policy_dir.path().join("policy.json");
    fs::write(&policy_path, r#"{"Version":"2012-10-17","Statement":[]}"#)
        .expect("write policy file");
    let (endpoint, receiver, handle) = start_admin_test_server("");

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "service-account",
            "update",
            "myalias",
            "service-key",
            "--policy",
            policy_path.to_str().expect("UTF-8 policy path"),
            "--description",
            "Updated description",
            "--secret-key",
            "replacement-secret",
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
    assert!(!stdout.contains("replacement-secret"));
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(payload["success"], true);
    assert_eq!(payload["access_key"], "service-key");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/update-service-account?accessKey=service-key"
    );
    handle.join().expect("admin test server finished");
}

#[test]
fn service_account_update_requires_at_least_one_field() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args([
            "admin",
            "service-account",
            "update",
            "myalias",
            "service-key",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("at least one field"));
}

#[test]
fn service_account_update_rejects_missing_policy_file() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args([
            "admin",
            "service-account",
            "update",
            "myalias",
            "service-key",
            "--policy",
            "/definitely/not/a/policy.json",
        ])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Failed to read policy file"));
}
