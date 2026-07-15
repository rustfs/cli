#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

fn run_service_action(action: &str, response_body: &'static str) -> (serde_json::Value, String) {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(response_body);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "service", action, "myalias"])
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
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    handle.join().expect("admin test server finished");

    (payload, request.target)
}

#[test]
fn service_stop_dispatches_to_service_action_stop() {
    let (payload, target) = run_service_action(
        "stop",
        r#"{"action":"stop","accepted":true,"effective":true,"message":"graceful shutdown initiated"}"#,
    );

    assert_eq!(target, "/rustfs/admin/v3/service?action=stop");
    assert_eq!(payload["action"], "stop");
    assert_eq!(payload["accepted"], true);
    assert_eq!(payload["effective"], true);
    assert_eq!(payload["message"], "graceful shutdown initiated");
}

#[test]
fn service_restart_dispatches_to_service_action_restart() {
    let (payload, target) = run_service_action(
        "restart",
        r#"{"action":"restart","accepted":true,"effective":true,"message":"graceful shutdown initiated; the supervising process manager is responsible for relaunch"}"#,
    );

    assert_eq!(target, "/rustfs/admin/v3/service?action=restart");
    assert_eq!(payload["action"], "restart");
    assert_eq!(payload["accepted"], true);
}

#[test]
fn service_freeze_reports_advisory_effectiveness() {
    let (payload, target) = run_service_action(
        "freeze",
        r#"{"action":"freeze","accepted":true,"effective":false,"message":"freeze flag recorded, but RustFS does not yet gate request admission on it (advisory only)"}"#,
    );

    assert_eq!(target, "/rustfs/admin/v3/service?action=freeze");
    assert_eq!(payload["accepted"], true);
    assert_eq!(payload["effective"], false);
}

#[test]
fn service_unfreeze_dispatches_to_service_action_unfreeze() {
    let (payload, target) = run_service_action(
        "unfreeze",
        r#"{"action":"unfreeze","accepted":true,"effective":false,"message":"freeze flag cleared (advisory only)"}"#,
    );

    assert_eq!(target, "/rustfs/admin/v3/service?action=unfreeze");
    assert_eq!(payload["action"], "unfreeze");
}

#[test]
fn service_stop_human_output_reports_success() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"action":"stop","accepted":true,"effective":true,"message":"graceful shutdown initiated"}"#,
    );

    let output = Command::new(rc_binary())
        .args(["admin", "service", "stop", "myalias"])
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
        stdout.contains("graceful shutdown initiated"),
        "stdout: {stdout}"
    );

    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}
