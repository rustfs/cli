#![cfg(not(windows))]

mod admin_support;

use std::process::Command;
use std::time::Duration;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};
use serde_json::Value;

const EXISTING_POLICY: &str = r#"{"Version":"2012-10-17","Id":"keep-id","Statement":[{"Sid":"ExistingPrefix","Effect":"Allow","Principal":"*","Action":["s3:GetObject"],"Resource":"arn:aws:s3:::bucket/music/*"},{"Sid":"KeepDeny","Effect":"Deny","Principal":"*","Action":"s3:DeleteObject","Resource":"arn:aws:s3:::bucket/*"}]}"#;

#[test]
fn prefix_set_reads_and_preserves_the_existing_bucket_policy() {
    let config = tempfile::tempdir().expect("config directory");
    let (endpoint, requests, server) =
        start_admin_sequence_test_server(vec![("200 OK", EXISTING_POLICY), ("204 No Content", "")]);
    let output = Command::new(rc_binary())
        .args(["anonymous", "set", "public", "test/bucket/images", "--json"])
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_test", rc_host_alias(&endpoint))
        .output()
        .expect("run prefix set");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let read = requests
        .recv_timeout(Duration::from_secs(5))
        .expect("policy read");
    assert_eq!(read.method, "GET");
    let url = url::Url::parse(&format!("http://localhost{}", read.target)).expect("request URL");
    assert_eq!(url.path().trim_end_matches('/'), "/bucket");
    assert!(url.query_pairs().any(|(name, _)| name == "policy"));
    let write = requests
        .recv_timeout(Duration::from_secs(5))
        .expect("policy write");
    assert_eq!(write.method, "PUT");
    let policy: Value = serde_json::from_slice(&write.body).expect("policy JSON");
    let existing: Value = serde_json::from_str(EXISTING_POLICY).expect("policy JSON");
    assert_eq!(policy["Id"], "keep-id");
    for statement in existing["Statement"].as_array().expect("statements") {
        assert!(
            policy["Statement"]
                .as_array()
                .expect("statements")
                .contains(statement)
        );
    }
    assert!(
        policy["Statement"]
            .as_array()
            .expect("statements")
            .iter()
            .any(|s| s["Resource"] == "arn:aws:s3:::bucket/images/*")
    );
    server.join().expect("policy server");
}

#[test]
fn prefix_set_does_not_write_when_reading_the_policy_is_denied() {
    let config = tempfile::tempdir().expect("config directory");
    let (endpoint, requests, server) =
        start_admin_sequence_test_server(vec![("403 Forbidden", "")]);
    let output = Command::new(rc_binary())
        .args(["anonymous", "set", "download", "test/bucket/images"])
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_test", rc_host_alias(&endpoint))
        .output()
        .expect("run prefix set");
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(5))
            .expect("policy read")
            .method,
        "GET"
    );
    server.join().expect("policy server");
    assert!(requests.try_recv().is_err());
}
