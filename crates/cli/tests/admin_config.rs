#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{
    rc_binary, rc_host_alias, start_admin_sequence_test_server, start_admin_test_server,
};

const SCANNER_HELP: &str = r#"{"subSys":"scanner","description":"scanner settings","multipleTargets":false,"keysHelp":[{"key":"speed","type":"string","description":"scanner speed","optional":true,"multipleTargets":false}]}"#;

fn run(args: &[&str], endpoint: &str, config_dir: &tempfile::TempDir) -> std::process::Output {
    Command::new(rc_binary())
        .args(args)
        .env("RC_CONFIG_DIR", config_dir.path())
        .env("RC_HOST_myalias", rc_host_alias(endpoint))
        .output()
        .expect("run rc command")
}

fn assert_valid_v3(value: &serde_json::Value) {
    let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("schemas/output_v3.json");
    let schema = std::fs::read_to_string(&schema_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", schema_path.display()));
    let schema: serde_json::Value =
        serde_json::from_str(&schema).expect("output v3 schema should parse");
    let validator = jsonschema::validator_for(&schema).expect("output v3 schema should compile");
    let errors = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "config output must satisfy output v3:\n{}",
        errors.join("\n")
    );
}

#[test]
fn config_get_defensively_redacts_server_output() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) =
        start_admin_test_server("identity_openid client_id=console client_secret=server-secret");

    let output = run(
        &[
            "--json",
            "admin",
            "config",
            "get",
            "myalias",
            "identity_openid",
        ],
        &endpoint,
        &config_dir,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("*redacted*"));
    assert!(!stdout.contains("server-secret"));
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["type"], "admin_operations");
    assert_eq!(value["data"]["operations"][0]["changed"], false);
    assert_valid_v3(&value);

    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/get-config-kv?key=identity_openid"
    );
    handle.join().expect("server finished");
}

#[test]
fn config_history_redacts_secret_bearing_data() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_test_server(
        r#"[{"RestoreID":"restore-1","CreateTime":"2026-07-21T00:00:00Z","Data":"identity_openid client_secret=history-secret"}]"#,
    );

    let output = run(
        &[
            "--json", "admin", "config", "history", "myalias", "--count", "5",
        ],
        &endpoint,
        &config_dir,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("*redacted*"));
    assert!(!stdout.contains("history-secret"));
    let request = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/list-config-history-kv?count=5"
    );
    handle.join().expect("server finished");
}

#[test]
fn config_set_dry_run_never_sends_mutation() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", SCANNER_HELP),
        ("200 OK", "scanner speed=default"),
    ]);

    let output = run(
        &[
            "--json",
            "admin",
            "config",
            "set",
            "myalias",
            "scanner",
            "speed=fast",
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
    let first = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("help request");
    let second = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("config request");
    assert_eq!(first.method, "GET");
    assert!(first.target.starts_with("/rustfs/admin/v3/help-config-kv?"));
    assert_eq!(second.method, "GET");
    assert_eq!(second.target, "/rustfs/admin/v3/get-config-kv?key=scanner");
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    handle.join().expect("server finished");
}

#[test]
fn config_set_success_has_read_preflight_before_mutation() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, receiver, handle) = start_admin_sequence_test_server(vec![
        ("200 OK", SCANNER_HELP),
        ("200 OK", "scanner speed=default"),
        ("200 OK", ""),
    ]);

    let output = run(
        &["admin", "config", "set", "myalias", "scanner", "speed=fast"],
        &endpoint,
        &config_dir,
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = (0..3)
        .map(|_| {
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("captured request")
        })
        .collect::<Vec<_>>();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[2].method, "PUT");
    assert_eq!(requests[2].target, "/rustfs/admin/v3/set-config-kv");
    handle.join().expect("server finished");
}

#[test]
fn config_access_denial_never_echoes_submitted_secret() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) = start_admin_sequence_test_server(vec![(
        "403 Forbidden",
        r#"{"message":"server echoed submitted-secret in a forbidden response"}"#,
    )]);

    let output = run(
        &[
            "admin",
            "config",
            "set",
            "myalias",
            "identity_openid",
            "client_secret=submitted-secret",
        ],
        &endpoint,
        &config_dir,
    );

    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(!stderr.contains("submitted-secret"));
    assert!(stderr.contains("denied configuration access"));
    handle.join().expect("server finished");
}

#[test]
fn unsupported_module_switch_has_deterministic_exit_code() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let (endpoint, _receiver, handle) =
        start_admin_sequence_test_server(vec![("404 Not Found", r#"{"message":"missing route"}"#)]);

    let output = run(
        &[
            "--json",
            "admin",
            "config",
            "module-switch",
            "get",
            "myalias",
        ],
        &endpoint,
        &config_dir,
    );

    assert_eq!(output.status.code(), Some(7));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("does not advertise module switch operations"));
    let value: serde_json::Value = serde_json::from_str(&stderr).expect("JSON error output");
    assert_valid_v3(&value);
    handle.join().expect("server finished");
}

#[test]
fn restore_requires_confirmation_without_contacting_server() {
    let config_dir = tempfile::tempdir().expect("create config dir");
    let output = Command::new(rc_binary())
        .args(["admin", "config", "restore", "myalias", "restore-1"])
        .env("RC_CONFIG_DIR", config_dir.path())
        .env(
            "RC_HOST_myalias",
            "http://ACCESS_KEY:SECRET_KEY@127.0.0.1:1",
        )
        .output()
        .expect("run rc command");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("pass --yes"));
}
