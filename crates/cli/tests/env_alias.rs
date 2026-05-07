#![cfg(not(windows))]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

fn rc_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return PathBuf::from(path);
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has parent directory")
        .parent()
        .expect("workspace root exists")
        .to_path_buf();

    let debug_binary = workspace_root.join("target/debug/rc");
    if debug_binary.exists() {
        return debug_binary;
    }

    workspace_root.join("target/release/rc")
}

fn unused_local_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local endpoint");
    let address = listener.local_addr().expect("local endpoint address");
    drop(listener);
    format!("http://{address}")
}

#[test]
fn alias_list_includes_rc_host_alias_without_credentials() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["alias", "list", "--json"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_myalias",
            "https://ACCESS_KEY:SECRET_KEY@rustfs.local:9000",
        )
        .output()
        .expect("run rc command");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(!stdout.contains("SECRET_KEY"));

    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(payload["aliases"][0]["name"], "myalias");
    assert_eq!(
        payload["aliases"][0]["endpoint"],
        "https://rustfs.local:9000"
    );
}

#[test]
fn ls_resolves_rc_host_alias_from_environment() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let endpoint = unused_local_endpoint();
    let (_, endpoint_authority) = endpoint.split_once("://").expect("endpoint has scheme");
    let env_alias = format!("http://ACCESS_KEY:SECRET_KEY@{endpoint_authority}");

    let output = Command::new(rc_binary())
        .args(["--json", "ls", "myalias"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", env_alias)
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error output");
    assert!(
        payload["error"]
            .as_str()
            .expect("error message")
            .contains("Failed to list buckets"),
        "payload: {payload}"
    );
    assert_eq!(payload["details"]["type"], "network_error");
}

#[test]
fn alias_list_rejects_invalid_rc_host_percent_encoding_without_credentials() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["alias", "list", "--json"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_badalias",
            "https://ACCESS_KEY:SECRET%ZZ@rustfs.local:9000",
        )
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(!stderr.contains("ACCESS_KEY"));
    assert!(!stderr.contains("SECRET"));

    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error output");
    assert!(
        payload["error"]
            .as_str()
            .expect("error message")
            .contains("invalid percent-encoding in secret key"),
        "payload: {payload}"
    );
    assert_eq!(payload["code"], 2);
    assert_eq!(payload["details"]["type"], "usage_error");
}

#[test]
fn alias_list_rejects_invalid_rc_host_scheme_without_credentials() {
    let config_dir = tempfile::tempdir().expect("create config dir");

    let output = Command::new(rc_binary())
        .args(["alias", "list", "--json"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_badalias",
            "ftp://ACCESS_KEY:SECRET_KEY@rustfs.local:9000",
        )
        .output()
        .expect("run rc command");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(!stderr.contains("ACCESS_KEY"));
    assert!(!stderr.contains("SECRET_KEY"));

    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error output");
    assert!(
        payload["error"]
            .as_str()
            .expect("error message")
            .contains("must use an http or https URL"),
        "payload: {payload}"
    );
    assert_eq!(payload["code"], 2);
    assert_eq!(payload["details"]["type"], "usage_error");
}
