//! CLI error output contract tests.
//!
//! These tests cover local validation paths that do not require a running S3
//! backend but still need stable JSON error metadata.

use std::path::{Path, PathBuf};
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

fn run_rc(args: &[&str]) -> Output {
    Command::new(rc_binary())
        .args(args)
        .output()
        .expect("failed to execute rc")
}

fn run_rc_with_config(args: &[&str], config_dir: &Path) -> Output {
    Command::new(rc_binary())
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .output()
        .expect("failed to execute rc")
}

#[test]
fn cp_local_to_local_json_error_reports_usage_metadata() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let source = temp_dir.path().join("source.txt");
    let target = temp_dir.path().join("target.txt");
    std::fs::write(&source, b"source").expect("write source file");
    std::fs::write(&target, b"target").expect("write target file");

    let output = run_rc(&[
        "cp",
        source.to_str().expect("source path is valid utf-8"),
        target.to_str().expect("target path is valid utf-8"),
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "usage JSON errors should be emitted on stderr"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");

    assert_eq!(
        json["error"],
        "Cannot copy between two local paths. Use system cp command."
    );
    assert_eq!(json["code"], 2);
    assert_eq!(json["details"]["type"], "usage_error");
    assert_eq!(
        json["details"]["message"],
        "Cannot copy between two local paths. Use system cp command."
    );
    assert_eq!(json["details"]["retryable"], false);
    assert_eq!(
        json["details"]["suggestion"],
        "Use your local shell cp command when both paths are on the filesystem."
    );
}

#[test]
fn alias_set_json_error_rejects_embedded_endpoint_credentials() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");

    let output = run_rc_with_config(
        &[
            "alias",
            "set",
            "bad",
            "http://ACCESS_KEY:SECRET_KEY@localhost:9000",
            "accesskey",
            "secretkey",
            "--json",
        ],
        config_dir.path(),
    );

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "usage JSON errors should be emitted on stderr"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(!stderr.contains("SECRET_KEY"));

    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");
    assert_eq!(
        json["error"],
        "Endpoint must not include credentials; pass access key and secret key as separate arguments"
    );
    assert_eq!(json["code"], 2);
    assert_eq!(json["details"]["type"], "usage_error");
    assert_eq!(json["details"]["retryable"], false);

    let list_output = run_rc_with_config(&["alias", "list", "--json"], config_dir.path());
    assert!(
        list_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );

    let stdout = String::from_utf8(list_output.stdout).expect("stdout should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");
    assert_eq!(
        payload["aliases"].as_array().expect("aliases array").len(),
        0
    );
}
