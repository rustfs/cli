#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

#[test]
fn rebalance_start_dispatches_to_rebalance_start_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"id":"rebalance-123"}"#);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "rebalance", "start", "myalias"])
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
    assert_eq!(payload["success"], true);
    assert_eq!(payload["message"], "Rebalance started successfully");
    assert_eq!(payload["target"], "myalias");
    assert_eq!(payload["id"], "rebalance-123");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rustfs/admin/v3/rebalance/start");

    handle.join().expect("admin test server finished");
}

#[test]
fn rebalance_status_dispatches_to_rebalance_status_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"id":"rebalance-123","pools":[{"id":0,"status":"Completed","used":0.5,"lastError":null,"cleanupWarnings":{"count":1,"lastMsg":"cleanup warning","lastBucket":"test-bucket","lastObject":"object-a","lastAt":"2026-06-12T00:00:00Z"},"progress":null}],"stoppedAt":"2026-05-07T00:00:00Z"}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "rebalance", "status", "myalias"])
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
    assert_eq!(payload["id"], "rebalance-123");
    assert_eq!(payload["stoppedAt"], "2026-05-07T00:00:00Z");
    assert_eq!(payload["pools"].as_array().expect("pools array").len(), 1);
    assert_eq!(payload["pools"][0]["cleanupWarnings"]["count"], 1);
    assert_eq!(
        payload["pools"][0]["cleanupWarnings"]["lastMsg"],
        "cleanup warning"
    );
    assert_eq!(
        payload["pools"][0]["cleanupWarnings"]["lastBucket"],
        "test-bucket"
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/rebalance/status");

    handle.join().expect("admin test server finished");
}

#[test]
fn rebalance_stop_dispatches_to_rebalance_stop_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server("");

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "rebalance", "stop", "myalias"])
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
    assert_eq!(payload["success"], true);
    assert_eq!(payload["message"], "Rebalance stopped successfully");
    assert_eq!(payload["target"], "myalias");
    assert!(payload.get("id").is_none());

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rustfs/admin/v3/rebalance/stop");

    handle.join().expect("admin test server finished");
}
