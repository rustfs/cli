#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

#[test]
fn pool_list_dispatches_to_pool_list_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"[{"id":0,"cmdline":"/data/pool0/disk{1...4}","lastUpdate":"2026-05-10T00:00:00Z","decommissionInfo":null}]"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "pool", "list", "myalias"])
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
    let pools = payload["pools"].as_array().expect("pools array");
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0]["id"], 0);
    assert_eq!(pools[0]["cmdline"], "/data/pool0/disk{1...4}");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/pools/list");

    handle.join().expect("admin test server finished");
}

#[test]
fn pool_status_dispatches_by_id_pool_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"id":1,"cmdline":"/data/pool1/disk{1...4}","lastUpdate":"2026-05-10T00:00:00Z","decommissionInfo":null}"#,
    );

    let output = Command::new(rc_binary())
        .args([
            "--json", "admin", "pool", "status", "myalias", "1", "--by-id",
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
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(payload["id"], 1);
    assert_eq!(payload["cmdline"], "/data/pool1/disk{1...4}");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/pools/status?pool=1&by-id=true"
    );

    handle.join().expect("admin test server finished");
}
