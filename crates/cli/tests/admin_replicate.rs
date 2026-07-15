#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

#[test]
fn replicate_info_dispatches_to_site_replication_info() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"enabled":false}"#);

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "info", "myalias"])
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
    assert_eq!(payload["enabled"], false);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/site-replication/info");

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_info_human_output_lists_sites() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"enabled":true,"name":"site1","sites":[{"name":"site1","endpoint":"http://10.0.0.5:9000"},{"name":"site2","endpoint":"http://10.0.0.6:9000"}]}"#,
    );

    let output = Command::new(rc_binary())
        .args(["admin", "replicate", "info", "myalias"])
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
    assert!(stdout.contains("site1"), "stdout: {stdout}");
    assert!(stdout.contains("http://10.0.0.6:9000"), "stdout: {stdout}");

    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_status_requests_default_summary_sections() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"enabled":true,"MaxBuckets":2,"MaxUsers":1,"MaxGroups":0,"MaxPolicies":5,"Sites":{"dep-1":{"name":"site1","endpoint":"http://10.0.0.5:9000"}}}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "status", "myalias"])
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
    assert_eq!(payload["enabled"], true);
    assert_eq!(payload["MaxBuckets"], 2);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/site-replication/status?buckets=true&users=true&groups=true&policies=true"
    );

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_status_forwards_selected_section_flags() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(r#"{"enabled":true}"#);

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "replicate",
            "status",
            "myalias",
            "--buckets",
            "--metrics",
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
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/site-replication/status?buckets=true&metrics=true"
    );

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_add_dispatches_with_resolved_alias_sites() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"success":true,"status":"Requested sites were configured for replication successfully."}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "add", "sitea", "siteb"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_sitea", rc_host_alias(&endpoint))
        .env("RC_HOST_siteb", rc_host_alias(&endpoint))
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

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/rustfs/admin/v3/site-replication/add");

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_add_rejects_single_alias() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["admin", "replicate", "add", "onlyone"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert!(!output.status.success());
}

#[test]
fn replicate_remove_all_dispatches_to_site_replication_remove() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"status":"Requested site(s) were removed from cluster replication successfully."}"#,
    );

    let output = Command::new(rc_binary())
        .args(["--json", "admin", "replicate", "remove", "myalias", "--all"])
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
    assert_eq!(request.target, "/rustfs/admin/v3/site-replication/remove");

    handle.join().expect("admin test server finished");
}

#[test]
fn replicate_remove_requires_site_or_all() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["admin", "replicate", "remove", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .output()
        .expect("run rc command");

    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
}
