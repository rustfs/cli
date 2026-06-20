#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_test_server};

#[test]
fn access_key_info_dispatches_to_access_key_info_json() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"{"accessKey":"svc-ldap","userType":"Service Account","userProvider":"ldap","parentUser":"ldap-parent","accountStatus":"on","ldapSpecificInfo":{"username":"alice"}}"#,
    );

    let output = Command::new(rc_binary())
        .args([
            "--json",
            "admin",
            "access-key",
            "info",
            "myalias",
            "svc-ldap",
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
    assert_eq!(payload["accessKey"], "svc-ldap");
    assert_eq!(payload["userType"], "Service Account");
    assert_eq!(payload["userProvider"], "ldap");
    assert_eq!(payload["parentUser"], "ldap-parent");
    assert_eq!(payload["accountStatus"], "on");
    assert_eq!(payload["ldapSpecificInfo"]["username"], "alice");

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured admin request");
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/info-access-key?accessKey=svc-ldap"
    );

    handle.join().expect("admin test server finished");
}
