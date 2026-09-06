#![cfg(not(windows))]

mod admin_support;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};
use serde_json::{Value, json};
use std::process::Command;
use std::time::Duration;

#[test]
fn catalog_binary_paginates_signed_requests_and_redacts_credentials() {
    let config = tempfile::tempdir().unwrap();
    let (endpoint, requests, server) = start_admin_sequence_test_server(vec![
        (
            "200 OK",
            r#"{"identifiers":[{"name":"one"}],"next-page-token":"a+/="}"#,
        ),
        (
            "200 OK",
            r#"{"identifiers":[{"name":"two"}],"storage-credentials":[{"secret-access-key":"hidden"}]}"#,
        ),
    ]);
    let output = Command::new(rc_binary())
        .args([
            "table",
            "list",
            "myalias/warehouse.bucket/sales.eu",
            "--json",
        ])
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["type"], "table_catalog");
    assert_eq!(data["data"]["operation"], "table_list");
    assert_eq!(
        data["data"]["result"]["identifiers"],
        json!([{"name":"one"},{"name":"two"}])
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("hidden"));
    let first = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        first.target,
        "/iceberg/v1/warehouse.bucket/namespaces/sales%1Feu/tables?pageSize=1000"
    );
    assert!(
        first
            .headers
            .to_ascii_lowercase()
            .contains("authorization: aws4-hmac-sha256")
    );
    assert!(second.target.ends_with("pageToken=a%2B%2F%3D"));
    server.join().unwrap();
}

#[test]
fn catalog_binary_conflict_preserves_pointer_guards_and_exit_code() {
    let config = tempfile::tempdir().unwrap();
    let file = config.path().join("commit.json");
    std::fs::write(&file, r#"{"new-metadata-location":"s3://warehouse/m2"}"#).unwrap();
    let (endpoint, requests, server) = start_admin_sequence_test_server(vec![(
        "409 Conflict",
        r#"{"error":{"message":"stale metadata"}}"#,
    )]);
    let output = Command::new(rc_binary())
        .args(["table", "commit", "myalias/warehouse/ns/table", "--file"])
        .arg(file)
        .args([
            "--expected-version-token",
            "v1",
            "--expected-metadata-location",
            "s3://warehouse/m1",
            "--commit-id",
            "attempt-1",
            "--json",
        ])
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_myalias", rc_host_alias(&endpoint))
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    let data: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(data["status"], "error");
    assert_eq!(data["error"]["retryable"], false);
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(request.method, "POST");
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["expected-version-token"], "v1");
    assert_eq!(body["expected-metadata-location"], "s3://warehouse/m1");
    assert_eq!(body["commit-id"], "attempt-1");
    server.join().unwrap();
    assert!(requests.try_recv().is_err());
}

#[test]
fn catalog_binary_missing_alias_uses_catalog_error_envelope() {
    let config = tempfile::tempdir().unwrap();
    let output = Command::new(rc_binary())
        .args([
            "table",
            "show",
            "missing-catalog-alias/warehouse/ns/table",
            "--json",
        ])
        .env("RC_CONFIG_DIR", config.path())
        .env_remove("RC_HOST_missing-catalog-alias")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    let data: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(data["type"], "table_catalog");
    assert_eq!(data["error"]["type"], "not_found");
}
