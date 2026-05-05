#![cfg(not(windows))]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};

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

fn run_rc(args: &[&str], config_dir: &std::path::Path) -> Output {
    Command::new(rc_binary())
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .output()
        .expect("run rc command")
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

    let alias_output = run_rc(
        &[
            "alias",
            "set",
            "test",
            &endpoint,
            "accesskey",
            "secretkey",
            "--bucket-lookup",
            "path",
        ],
        config_dir.path(),
    );
    assert!(
        alias_output.status.success(),
        "alias setup failed: {}",
        String::from_utf8_lossy(&alias_output.stderr)
    );

    let output = run_rc(
        &["--json", "ls", "test/bucket", "--versions"],
        config_dir.path(),
    );
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
