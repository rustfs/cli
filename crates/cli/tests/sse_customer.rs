//! SSE-C CLI security and exit-code contract tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rc_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return PathBuf::from(path);
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has parent directory")
        .parent()
        .expect("workspace root exists")
        .join("target/debug/rc")
}

fn run_rc(args: &[&str], config_dir: &Path, environment: Option<(&str, &str)>) -> Output {
    let mut command = Command::new(rc_binary());
    command
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .env_remove("RC_TEST_SSE_C_KEY");
    if let Some((name, value)) = environment {
        command.env(name, value);
    }
    command.output().expect("execute rc")
}

fn protect_key_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("protect key file");
    }
}

#[test]
fn pipe_rejects_wrong_length_key_without_disclosing_file_contents() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let key_file = tempfile::NamedTempFile::new().expect("create key file");
    let secret = "customer-secret-that-is-too-short";
    std::fs::write(key_file.path(), secret).expect("write key file");
    protect_key_file(key_file.path());

    let output = run_rc(
        &[
            "pipe",
            "missing/bucket/object",
            "--enc-c-key-file",
            key_file.path().to_str().expect("key path is UTF-8"),
            "--json",
        ],
        config_dir.path(),
        None,
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("SSE-C keys must contain exactly 32 bytes"));
    assert!(!stderr.contains(secret));
}

#[test]
fn cp_rejects_unproven_server_side_sse_c_copy_before_key_or_network_access() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let raw_key = "0123456789abcdef0123456789abcdef";

    let output = run_rc(
        &[
            "cp",
            "missing/source/object",
            "missing/destination/object",
            "--enc-c-source-key-env",
            "RC_TEST_SSE_C_KEY",
            "--json",
        ],
        config_dir.path(),
        Some(("RC_TEST_SSE_C_KEY", raw_key)),
    );

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("server-side SSE-C copy is not compatibility-proven"));
    assert!(!stderr.contains(raw_key));
    assert!(!stderr.contains("MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="));
    assert!(!stderr.contains("hRasmdxgYDKV3nvbahU1MA=="));
}

#[test]
fn conflicting_encryption_flags_fail_before_loading_sse_c_input() {
    let config_dir = tempfile::tempdir().expect("create config directory");

    for args in [
        vec![
            "pipe",
            "missing/bucket/object",
            "--enc-s3",
            "--enc-c-key-env",
            "RC_TEST_SSE_C_KEY",
            "--json",
        ],
        vec![
            "cp",
            "missing/source/object",
            "missing/destination/object",
            "--enc-s3",
            "missing/destination/object",
            "--enc-c-destination-key-env",
            "RC_TEST_SSE_C_KEY",
            "--json",
        ],
    ] {
        let output = run_rc(&args, config_dir.path(), None);
        assert_eq!(
            output.status.code(),
            Some(2),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be combined"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn missing_key_file_returns_usage_without_exposing_the_path() {
    let config_dir = tempfile::tempdir().expect("create config directory");
    let missing = config_dir.path().join("customer-material-does-not-exist");

    let output = run_rc(
        &[
            "pipe",
            "missing/bucket/object",
            "--enc-c-key-file",
            missing.to_str().expect("missing path is UTF-8"),
            "--json",
        ],
        config_dir.path(),
        None,
    );

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("Failed to inspect SSE-C key file"));
    assert!(!stderr.contains("customer-material-does-not-exist"));
}
