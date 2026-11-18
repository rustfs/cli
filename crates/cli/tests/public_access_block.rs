#[allow(dead_code)]
mod admin_support;

use std::process::{Command, Output};

use admin_support::{rc_host_alias, start_admin_response_test_server};
use serde_json::{Value, json};

fn run_with_s3_error(operation: &str, status: &str, code: &str) -> Output {
    let config = tempfile::tempdir().expect("create config directory");
    let (endpoint, _requests, server) = start_admin_response_test_server(
        status,
        "application/xml",
        format!("<Error><Code>{code}</Code><Message>Request failed</Message></Error>"),
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rc"));
    cmd.args([
        "--json",
        "bucket",
        "public-access-block",
        operation,
        "local/archive",
    ])
    .env("RC_CONFIG_DIR", config.path())
    .env("RC_HOST_local", rc_host_alias(&endpoint));
    match operation {
        "set" => {
            let source = config.path().join("block.json");
            std::fs::write(
                &source,
                r#"{"block_public_acls":true,"ignore_public_acls":true,"block_public_policy":true,"restrict_public_buckets":true}"#,
            )
            .expect("write Public Access Block configuration");
            cmd.arg(source);
        }
        "remove" => {
            cmd.arg("--yes");
        }
        "get" => {}
        _ => panic!("unexpected operation {operation}"),
    }
    let output = cmd.output().expect("run rc");
    server.join().expect("S3 test server finished");
    output
}

#[test]
fn missing_configuration_is_an_empty_get_and_an_idempotent_remove() {
    for operation in ["get", "remove"] {
        let output = run_with_s3_error(
            operation,
            "404 Not Found",
            "NoSuchPublicAccessBlockConfiguration",
        );
        assert_eq!(output.status.code(), Some(0), "{operation}: {output:?}");
        let payload: Value = serde_json::from_slice(&output.stdout).expect("JSON success");
        assert_eq!(payload["status"], "success");
        assert_eq!(
            payload["data"],
            json!({
                "bucket": "archive", "operation": operation, "configuration": null
            })
        );
    }
}

#[test]
fn missing_bucket_is_not_found_for_every_operation() {
    for operation in ["get", "set", "remove"] {
        let output = run_with_s3_error(operation, "404 Not Found", "NoSuchBucket");
        assert_eq!(output.status.code(), Some(5), "{operation}: {output:?}");
        let payload: Value = serde_json::from_slice(&output.stderr).expect("JSON error");
        assert_eq!(payload["error"]["type"], "not_found");
    }
}

#[test]
fn access_denied_is_an_auth_error_for_every_operation() {
    for operation in ["get", "set", "remove"] {
        let output = run_with_s3_error(operation, "403 Forbidden", "AccessDenied");
        assert_eq!(output.status.code(), Some(4), "{operation}: {output:?}");
        let payload: Value = serde_json::from_slice(&output.stderr).expect("JSON error");
        assert_eq!(payload["error"]["type"], "auth_error");
    }
}

#[test]
fn forbidden_response_cannot_be_interpreted_as_missing_configuration() {
    for operation in ["get", "remove"] {
        let output = run_with_s3_error(
            operation,
            "403 Forbidden",
            "NoSuchPublicAccessBlockConfiguration",
        );
        assert_eq!(output.status.code(), Some(4), "{operation}: {output:?}");
    }
}

#[test]
fn set_never_treats_missing_configuration_as_success() {
    let output = run_with_s3_error(
        "set",
        "404 Not Found",
        "NoSuchPublicAccessBlockConfiguration",
    );
    assert!(!output.status.success());
    let payload: Value = serde_json::from_slice(&output.stderr).expect("JSON error");
    assert_eq!(payload["status"], "error");
}
