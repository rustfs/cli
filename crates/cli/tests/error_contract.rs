//! CLI error output contract tests.
//!
//! These tests cover local validation paths that do not require a running S3
//! backend but still need stable JSON error metadata.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

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

fn rc_command() -> Command {
    let mut command = Command::new(rc_binary());

    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }

    command
}

fn run_rc(args: &[&str]) -> Output {
    rc_command()
        .args(args)
        .output()
        .expect("failed to execute rc")
}

fn run_rc_with_config(args: &[&str], config_dir: &Path) -> Output {
    rc_command()
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .output()
        .expect("failed to execute rc")
}

fn spawn_s3_error_server(code: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock S3 server");
    let address = listener.local_addr().expect("mock S3 server address");

    thread::spawn(move || {
        for _ in 0..3 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let body = format!("<Error><Code>{code}</Code><Message>test</Message></Error>");
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    format!("http://{address}")
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

#[test]
fn alias_set_json_error_rejects_invalid_signature() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");

    let output = run_rc_with_config(
        &[
            "alias",
            "set",
            "bad",
            "http://localhost:9000",
            "accesskey",
            "secretkey",
            "--signature",
            "v5",
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
    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");
    assert_eq!(json["error"], "Signature must be 'v4' or 'v2'");
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

#[test]
#[cfg(not(windows))]
fn alias_set_json_error_rejects_invalid_credentials_without_saving_alias() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");
    let endpoint = spawn_s3_error_server("InvalidAccessKeyId");

    let output = run_rc_with_config(
        &[
            "alias",
            "set",
            "bad",
            &endpoint,
            "accesskey",
            "secretkey",
            "--json",
        ],
        config_dir.path(),
    );

    assert_eq!(
        output.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "auth JSON errors should be emitted on stderr"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");
    assert_eq!(
        json["error"],
        "Authentication failed: Invalid access key or secret key"
    );
    assert_eq!(json["code"], 4);
    assert_eq!(json["details"]["type"], "auth_error");
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

#[test]
fn alias_set_anonymous_accepts_missing_credentials_and_lists_auth_mode() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");

    let output = run_rc_with_config(
        &[
            "alias",
            "set",
            "public",
            "https://public.example.com",
            "--anonymous",
            "--client-cert",
            "/tmp/client.pem",
            "--client-key",
            "/tmp/client.key",
            "--json",
        ],
        config_dir.path(),
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let list_output = run_rc_with_config(&["alias", "list", "--json"], config_dir.path());
    assert!(
        list_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );

    let stdout = String::from_utf8(list_output.stdout).expect("stdout should be UTF-8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");
    let alias = &payload["aliases"][0];
    assert_eq!(alias["name"], "public");
    assert_eq!(alias["auth_mode"], "anonymous");
    assert_eq!(alias["mtls"], true);
}

#[test]
fn alias_set_anonymous_rejects_supplied_credentials() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");

    let output = run_rc_with_config(
        &[
            "alias",
            "set",
            "bad",
            "https://public.example.com",
            "accesskey",
            "secretkey",
            "--anonymous",
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

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");
    assert_eq!(
        json["error"],
        "Anonymous aliases must not include access key or secret key credentials"
    );
}

#[test]
fn alias_set_rejects_partial_client_identity() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");

    let output = run_rc_with_config(
        &[
            "alias",
            "set",
            "bad",
            "https://public.example.com",
            "accesskey",
            "secretkey",
            "--client-cert",
            "/tmp/client.pem",
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

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");
    assert_eq!(
        json["error"],
        "--client-cert and --client-key must be supplied together"
    );
}
