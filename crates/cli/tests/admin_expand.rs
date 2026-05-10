#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

#[test]
fn scale_start_dispatches_to_rebalance_start_with_expansion_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"id":"rebalance-123"}"#);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "scale", "start", "myalias"])
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
    assert_eq!(
        payload["message"],
        "Expansion rebalance started successfully"
    );
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
fn expand_status_dispatches_to_rebalance_status_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"id":"rebalance-123","pools":[],"stoppedAt":"2026-05-06T00:00:00Z"}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "expand", "status", "myalias"])
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
    assert_eq!(payload["stoppedAt"], "2026-05-06T00:00:00Z");
    assert_eq!(
        payload["pools"]
            .as_array()
            .expect("pools should be an array")
            .len(),
        0
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/rebalance/status");

    handle.join().expect("admin test server finished");
}

#[test]
fn expand_stop_dispatches_to_rebalance_stop_with_expansion_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server("");

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "expand", "stop", "myalias"])
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
    assert_eq!(
        payload["message"],
        "Expansion rebalance stopped successfully"
    );
    assert_eq!(payload["target"], "myalias");
    assert!(payload.get("id").is_none());

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/rustfs/admin/v3/rebalance/stop");

    handle.join().expect("admin test server finished");
}
