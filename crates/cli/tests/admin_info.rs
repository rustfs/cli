#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

const BETA9_INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","deploymentID":"deployment-123","servers":[{"endpoint":"http://node1:9000","state":"online","version":"1.0.0-beta.9","drives":[{"endpoint":"http://node1:9000/data1","path":"/data1","state":"ok","totalspace":100,"usedspace":40,"availspace":60,"pool_index":1,"set_index":2,"disk_index":3}]}]},"admin_discovery":{"runtimeCapabilities":"/rustfs/admin/v4/runtime/capabilities","clusterSnapshot":"/rustfs/admin/v4/cluster/snapshot","extensionsCatalog":"/rustfs/admin/v4/extensions/catalog"}}"#;
const UNAVAILABLE_STATS_INFO_RESPONSE: &str = r#"{"info":{"mode":"distributed","deploymentID":"deployment-123","buckets":{"count":0,"error":"data usage snapshot unavailable"},"objects":{"count":0,"error":"data usage snapshot unavailable"},"servers":[]}}"#;

#[test]
fn cluster_info_displays_disk_location_indexes_from_snake_case_fields() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(BETA9_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args(["admin", "info", "cluster", "myalias"])
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
        stdout.contains("Location: pool:1 set:2 disk:3"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Deployment ID: deployment-123"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("Servers:       1"), "stdout: {stdout}");
    assert!(
        stdout.contains("Disks:         1 (1 online)"),
        "stdout: {stdout}"
    );

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/info");

    handle.join().expect("admin test server finished");
}

#[test]
fn cluster_info_marks_unavailable_counts_instead_of_reporting_zero() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(UNAVAILABLE_STATS_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args(["admin", "info", "cluster", "myalias"])
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
        stdout.contains("Buckets:       unavailable"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Objects:       unavailable"),
        "stdout: {stdout}"
    );
    assert!(!stdout.contains("Buckets:       0"), "stdout: {stdout}");
    assert!(!stdout.contains("Objects:       0"), "stdout: {stdout}");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/info");

    handle.join().expect("admin test server finished");
}

#[test]
fn server_info_reads_servers_from_beta9_info_response() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(BETA9_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args(["admin", "info", "server", "myalias"])
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
    assert!(stdout.contains("http://node1:9000"), "stdout: {stdout}");
    assert!(stdout.contains("1.0.0-beta.9"), "stdout: {stdout}");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/info");

    handle.join().expect("admin test server finished");
}

#[test]
fn disk_info_reads_disks_from_beta9_info_response() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(BETA9_INFO_RESPONSE);

    let output = Command::new(rc_binary())
        .args(["admin", "info", "disk", "myalias"])
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
    assert!(stdout.contains("/data1"), "stdout: {stdout}");
    assert!(stdout.contains("(pool:1 set:2 disk:3)"), "stdout: {stdout}");
    assert!(stdout.contains("40 B / 100 B (40%)"), "stdout: {stdout}");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rustfs/admin/v3/info");

    handle.join().expect("admin test server finished");
}
