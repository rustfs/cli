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
fn ls_versions_json_network_error_reports_exit_code() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let endpoint = unused_local_endpoint();

    let output = Command::new(rc_binary())
        .args(["--json", "ls", "test/bucket", "--versions"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_test",
            format!(
                "http://accesskey:secretkey@{}",
                endpoint.trim_start_matches("http://")
            ),
        )
        .output()
        .expect("run rc command");
    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error output");
    assert_eq!(payload["code"], 3);
    assert_eq!(payload["details"]["type"], "network_error");
    assert_eq!(payload["details"]["retryable"], true);
    assert!(
        payload["error"]
            .as_str()
            .expect("error message")
            .contains("Failed to list versions")
    );
}
