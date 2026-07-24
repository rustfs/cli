#![cfg(not(windows))]

mod admin_support;

use std::io::{Cursor, Write};
use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_binary_sequence_test_server};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(*name, SimpleFileOptions::default())
            .expect("start ZIP entry");
        writer.write_all(bytes).expect("write ZIP entry");
    }
    writer.finish().expect("finish ZIP").into_inner()
}

fn protected_archive(directory: &tempfile::TempDir, bytes: &[u8]) -> String {
    let path = directory.path().join("metadata.zip");
    std::fs::write(&path, bytes).expect("write archive");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("protect archive");
    }
    path.display().to_string()
}

fn run(args: &[&str], endpoint: &str, config_dir: &tempfile::TempDir) -> std::process::Output {
    Command::new(rc_binary())
        .args(args)
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(endpoint))
        .output()
        .expect("run rc command")
}

fn assert_v3(stdout: &[u8]) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_slice(stdout).expect("JSON output");
    let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("schemas/output_v3.json");
    let schema: serde_json::Value =
        serde_json::from_slice(&std::fs::read(schema_path).expect("read schema"))
            .expect("parse schema");
    let validator = jsonschema::validator_for(&schema).expect("compile schema");
    let errors = validator
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema errors: {}", errors.join("\n"));
    value
}

#[test]
fn export_is_selected_deterministic_atomic_and_v3() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let response = archive(&[
        ("alpha/quota.json", br#"{"quota":10}"#),
        ("alpha/policy.json", br#"{"Version":"2012-10-17"}"#),
    ]);
    let (endpoint, receiver, handle) = start_admin_binary_sequence_test_server(vec![
        ("200 OK", "application/zip", response.clone()),
        ("200 OK", "application/zip", response),
    ]);
    let first = config_dir.path().join("first.zip");
    let second = config_dir.path().join("second.zip");

    let output = run(
        &[
            "--json",
            "admin",
            "bucket-metadata",
            "export",
            "myalias",
            "--bucket",
            "alpha",
            "--file",
            first.to_str().expect("UTF-8 path"),
        ],
        &endpoint,
        &config_dir,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = assert_v3(&output.stdout);
    assert_eq!(value["data"]["operations"][0]["result"]["bucket"], "alpha");

    let output = run(
        &[
            "--json",
            "admin",
            "bucket-metadata",
            "export",
            "myalias",
            "--bucket",
            "alpha",
            "--file",
            second.to_str().expect("UTF-8 path"),
        ],
        &endpoint,
        &config_dir,
    );
    assert!(output.status.success());
    assert_eq!(
        std::fs::read(first).expect("first"),
        std::fs::read(second).expect("second")
    );
    for _ in 0..2 {
        let request = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("export request");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/export-bucket-metadata?bucket=alpha"
        );
    }
    handle.join().expect("server finished");
}

#[test]
fn dry_run_reports_conflicts_without_mutation() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let source = protected_archive(
        &config_dir,
        &archive(&[("alpha/policy.json", b"new-policy")]),
    );
    let current = archive(&[("alpha/policy.json", b"old-policy")]);
    let (endpoint, receiver, handle) =
        start_admin_binary_sequence_test_server(vec![("200 OK", "application/zip", current)]);

    let output = run(
        &[
            "--json",
            "admin",
            "bucket-metadata",
            "import",
            "myalias",
            "--file",
            &source,
            "--conflict",
            "overwrite",
            "--dry-run",
        ],
        &endpoint,
        &config_dir,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value = assert_v3(&output.stdout);
    assert_eq!(value["data"]["operations"][0]["changed"], false);
    assert_eq!(value["data"]["operations"][0]["result"]["conflicts"], 1);
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("preflight export");
    assert_eq!(request.method, "GET");
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    handle.join().expect("server finished");
}

#[test]
fn confirmed_import_sends_one_bounded_zip_and_never_retries() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let source_bytes = archive(&[("alpha/policy.json", b"new-policy")]);
    let source = protected_archive(&config_dir, &source_bytes);
    let current = archive(&[("alpha/policy.json", b"old-policy")]);
    let (endpoint, receiver, handle) = start_admin_binary_sequence_test_server(vec![
        ("200 OK", "application/zip", current),
        (
            "500 Internal Server Error",
            "application/json",
            b"{}".to_vec(),
        ),
    ]);

    let output = run(
        &[
            "--json",
            "admin",
            "bucket-metadata",
            "import",
            "myalias",
            "--file",
            &source,
            "--conflict",
            "overwrite",
            "--yes",
        ],
        &endpoint,
        &config_dir,
    );
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("partially applied"));
    let error = assert_v3(stderr.as_bytes());
    assert_eq!(error["error"]["type"], "network_error");
    assert_eq!(error["error"]["outcome"], "unknown_partial");
    assert_eq!(error["error"]["retryable"], false);
    let preflight = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("preflight");
    let mutation = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("mutation");
    assert_eq!(preflight.method, "GET");
    assert_eq!(mutation.method, "PUT");
    assert_eq!(mutation.target, "/rustfs/admin/v3/import-bucket-metadata");
    assert!(
        mutation
            .headers
            .to_ascii_lowercase()
            .contains("application/zip")
    );
    let imported = ZipArchive::new(Cursor::new(mutation.body)).expect("valid uploaded ZIP");
    assert_eq!(imported.len(), 1);
    assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
    handle.join().expect("server finished");
}

#[test]
fn malformed_archive_and_redacted_target_fail_before_network() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let malformed = protected_archive(&config_dir, b"not-a-zip");
    let output = run(
        &[
            "admin",
            "bucket-metadata",
            "import",
            "myalias",
            "--file",
            &malformed,
            "--conflict",
            "fail",
            "--dry-run",
        ],
        "http://127.0.0.1:9",
        &config_dir,
    );
    assert_eq!(output.status.code(), Some(2));

    let redacted = archive(&[(
        "alpha/bucket-targets.json",
        br#"{"targets":[{"secretKey":"*redacted*"}]}"#,
    )]);
    let redacted = protected_archive(&config_dir, &redacted);
    let output = run(
        &[
            "admin",
            "bucket-metadata",
            "import",
            "myalias",
            "--file",
            &redacted,
            "--conflict",
            "fail",
            "--dry-run",
        ],
        "http://127.0.0.1:9",
        &config_dir,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secretKey"));
}

#[test]
fn access_denial_and_unsupported_route_are_distinct() {
    for (status, expected) in [("403 Forbidden", 4), ("404 Not Found", 7)] {
        let config_dir = tempfile::tempdir().expect("config dir");
        let destination = config_dir.path().join("archive.zip");
        let (endpoint, receiver, handle) = start_admin_binary_sequence_test_server(vec![(
            status,
            "application/json",
            b"{}".to_vec(),
        )]);
        let output = run(
            &[
                "--json",
                "admin",
                "bucket-metadata",
                "export",
                "myalias",
                "--file",
                destination.to_str().expect("UTF-8 path"),
            ],
            &endpoint,
            &config_dir,
        );
        assert_eq!(output.status.code(), Some(expected));
        assert_v3(&output.stderr);
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("request");
        handle.join().expect("server finished");
    }
}
