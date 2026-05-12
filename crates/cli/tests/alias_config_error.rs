//! Alias command error output contract tests for config-load failures.

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

fn run_rc_with_config(args: &[&str], config_dir: &Path) -> Output {
    let mut command = Command::new(rc_binary());

    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }

    command
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .output()
        .expect("failed to execute rc")
}

#[test]
fn alias_remove_json_error_includes_code_for_malformed_config() {
    let config_dir = tempfile::tempdir().expect("create temp config dir");
    std::fs::write(config_dir.path().join("config.toml"), "not valid toml")
        .expect("write malformed config");

    let output = run_rc_with_config(&["alias", "remove", "stale", "--json"], config_dir.path());

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "JSON errors should be emitted on stderr"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    let json: serde_json::Value = serde_json::from_str(&stderr).expect("stderr is valid JSON");
    let message = json["error"].as_str().expect("error message");
    assert!(!message.is_empty(), "payload: {json}");
    assert_eq!(json["code"], 1);
    assert_eq!(json["details"]["type"], "general_error");
    assert_eq!(json["details"]["message"], message);
    assert_eq!(json["details"]["retryable"], false);
}
