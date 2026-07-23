//! Local-only exit-code coverage for bulk transfer preflight behavior.

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
    workspace_root.join("target/debug/rc")
}

fn run_rc(args: &[&str], config_dir: &Path) -> Output {
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
        .expect("execute rc")
}

#[test]
fn multi_source_ambiguous_destination_fails_before_remote_access() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let sources = tempfile::tempdir().expect("create source dir");
    let first = sources.path().join("first.txt");
    let second = sources.path().join("second.txt");
    std::fs::write(&first, b"first").expect("write first source");
    std::fs::write(&second, b"second").expect("write second source");

    let output = run_rc(
        &[
            "cp",
            first.to_str().expect("first path is UTF-8"),
            second.to_str().expect("second path is UTF-8"),
            "missing-alias/bucket/object.txt",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Multiple copy sources require"), "{stderr}");
    assert!(
        !stderr.contains("Alias 'missing-alias' not found"),
        "{stderr}"
    );
}

#[test]
fn empty_recursive_source_can_fail_with_not_found_exit_code() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let source = tempfile::tempdir().expect("create empty source");

    let output = run_rc(
        &[
            "cp",
            source.path().to_str().expect("source path is UTF-8"),
            "missing-alias/bucket/",
            "--recursive",
            "--fail-empty",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No copy sources matched"), "{stderr}");
}

#[test]
fn empty_recursive_source_succeeds_and_reports_zero_plan() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let source = tempfile::tempdir().expect("create empty source");

    let output = run_rc(
        &[
            "cp",
            source.path().to_str().expect("source path is UTF-8"),
            "missing-alias/bucket/",
            "--recursive",
            "--summary",
        ],
        config_dir.path(),
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 planned"), "{stdout}");
    assert!(stdout.contains("0 B transferred"), "{stdout}");
}

#[test]
fn aggregate_failure_keeps_summary_and_returns_non_success_exit_code() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let source = tempfile::NamedTempFile::new().expect("create source file");
    std::fs::write(source.path(), b"payload").expect("write source file");

    let output = run_rc(
        &[
            "cp",
            source.path().to_str().expect("source path is UTF-8"),
            "missing-alias/bucket/object.txt",
            "--summary",
            "--continue-on-error",
            "--retry-attempts",
            "1",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(5));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("1 planned"), "{stdout}");
    assert!(stdout.contains("1 failed"), "{stdout}");
    assert!(
        stderr.contains("Alias not found: missing-alias"),
        "{stderr}"
    );
}

#[test]
fn planned_json_output_is_explicitly_deferred_to_versioned_contract() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let source = tempfile::tempdir().expect("create empty source");

    let output = run_rc(
        &[
            "cp",
            source.path().to_str().expect("source path is UTF-8"),
            "missing-alias/bucket/",
            "--recursive",
            "--json",
        ],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    let payload: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr is valid JSON");
    assert_eq!(payload["code"], 7);
    assert!(
        payload["error"]
            .as_str()
            .is_some_and(|message| message.contains("versioned output contract"))
    );
}
