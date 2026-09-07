//! Binary-level contract tests for `rc admin bucket migration`.
//!
//! Every server answer comes from the wire fixtures vendored under
//! `crates/core/tests/fixtures/on_demand_migration/`.

#![cfg(not(windows))]

mod admin_support;

use admin_support::{rc_binary, rc_host_alias, start_admin_sequence_test_server};
use serde_json::Value;
use std::process::{Command, Output};
use std::time::Duration;

const SET_REQUEST: &str =
    include_str!("../../core/tests/fixtures/on_demand_migration/set_request.json");
const SET_RESPONSE: &str =
    include_str!("../../core/tests/fixtures/on_demand_migration/set_response.json");
const GET_RESPONSE: &str =
    include_str!("../../core/tests/fixtures/on_demand_migration/get_response.json");
const STATUS: &str = include_str!("../../core/tests/fixtures/on_demand_migration/status.json");
const STATUS_WITH_BACKFILL: &str =
    include_str!("../../core/tests/fixtures/on_demand_migration/status_with_backfill.json");
const BACKFILL_JOB: &str =
    include_str!("../../core/tests/fixtures/on_demand_migration/backfill_job.json");
const BACKFILL_JOB_COMPLETED: &str = r#"{"bucket":"photos","job":{"format_version":1,"job_id":"11111111-1111-4111-8111-111111111111","state":"completed","config_updated_at":"2026-09-02T10:00:00Z","prefix":"photos/","skip_existing":"always","dry_run":false,"listed":2000,"enqueued":2000,"pulled":1500,"skipped_existing":500,"failed":0,"bytes":73400320,"started_at":"2026-09-02T10:00:30Z","updated_at":"2026-09-02T10:09:10Z"}}"#;

fn run(endpoint: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let config = tempfile::tempdir().unwrap();
    let mut command = Command::new(rc_binary());
    command
        .args(["admin", "bucket", "migration"])
        .args(args)
        .env("RC_CONFIG_DIR", config.path())
        .env("RC_HOST_myalias", rc_host_alias(endpoint))
        .env_remove("RC_ODM_SECRET_KEY")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn get_prints_a_redacted_table_and_json_envelope() {
    let (endpoint, requests, server) =
        start_admin_sequence_test_server(vec![("200 OK", GET_RESPONSE), ("200 OK", GET_RESPONSE)]);
    let human = run(&endpoint, &["get", "myalias/photos"], &[]);
    assert!(human.status.success(), "{}", stderr(&human));
    let text = stdout(&human);
    assert!(text.contains("On-Demand Migration: photos"));
    assert!(text.contains("legacy-photos"));
    assert!(text.contains("AKIASOURCE"));
    assert!(text.contains("REDACTED"));
    assert!(text.contains("Source prefix:        photos/"));
    assert!(text.contains("Max concurrent pulls: 8"));

    let json = run(&endpoint, &["get", "myalias/photos", "--json"], &[]);
    assert!(json.status.success(), "{}", stderr(&json));
    let data: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(data["schema_version"], 3);
    assert_eq!(data["type"], "on_demand_migration");
    assert_eq!(data["data"]["operation"], "get");
    assert_eq!(data["data"]["bucket"], "photos");
    assert_eq!(
        data["data"]["result"]["config"]["source"]["credentials"]["secret_key"],
        "REDACTED"
    );
    assert_eq!(data["data"]["result"]["updated_at"], "2026-09-02T10:00:00Z");

    let first = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.method, "GET");
    assert_eq!(first.target, "/rustfs/admin/v3/on-demand-migration/photos");
    assert!(
        first
            .headers
            .to_ascii_lowercase()
            .contains("authorization: aws4-hmac-sha256")
    );
    server.join().unwrap();
}

#[test]
fn status_renders_the_null_ratio_as_an_em_dash_and_keeps_it_null_in_json() {
    let (endpoint, requests, server) = start_admin_sequence_test_server(vec![
        ("200 OK", STATUS),
        ("200 OK", STATUS_WITH_BACKFILL),
    ]);
    let human = run(&endpoint, &["status", "myalias/photos"], &[]);
    assert!(human.status.success(), "{}", stderr(&human));
    let text = stdout(&human);
    assert!(text.contains("Source-hit ratio:     \u{2014}"), "{text}");
    assert!(!text.contains("Source-hit ratio:     0"), "{text}");
    assert!(text.contains("Migrated bytes:       4 KiB (4096)"));
    assert!(text.contains("In-flight pulls:      1"));
    assert!(text.contains("Queued pulls:         1"));
    assert!(text.contains("Breaker:              half_open"));
    assert!(text.contains("Last source error:    server_error at 2026-09-02T10:00:00Z"));
    assert!(text.contains("Requests (get):       source_hit 2"));

    let json = run(&endpoint, &["status", "myalias/photos", "--json"], &[]);
    assert!(json.status.success(), "{}", stderr(&json));
    let data: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(data["data"]["operation"], "status");
    assert!(data["data"]["result"]["served_by_source_ratio"].is_null());
    assert_eq!(data["data"]["result"]["backfill"]["state"], "running");

    let first = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        first.target,
        "/rustfs/admin/v3/on-demand-migration/photos/status"
    );
    server.join().unwrap();
}

#[test]
fn set_reads_the_secret_from_the_environment_and_sends_the_fixture_body() {
    let (endpoint, requests, server) =
        start_admin_sequence_test_server(vec![("200 OK", SET_RESPONSE)]);
    let output = run(
        &endpoint,
        &[
            "set",
            "myalias/photos",
            "--provider",
            "minio",
            "--endpoint",
            "https://source.example.com:9000",
            "--region",
            "us-east-1",
            "--source-bucket",
            "legacy-photos",
            "--source-prefix",
            "photos/",
            "--access-key",
            "AKIASOURCE",
            "--dry-run",
            "--json",
        ],
        &[("RC_ODM_SECRET_KEY", "sourceSecretKey123")],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("sourceSecretKey123"));
    assert!(!stderr(&output).contains("sourceSecretKey123"));
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["data"]["operation"], "set");
    assert_eq!(data["data"]["result"]["probe"]["reachable"], true);
    assert_eq!(
        data["data"]["result"]["config"]["source"]["credentials"]["secret_key"],
        "REDACTED"
    );

    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(request.method, "PUT");
    assert_eq!(
        request.target,
        "/rustfs/admin/v3/on-demand-migration/photos?dry-run=true"
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let expected: Value = serde_json::from_str(SET_REQUEST).unwrap();
    assert_eq!(body, expected);
    server.join().unwrap();
}

#[test]
fn set_without_a_secret_source_is_a_usage_error_before_any_request() {
    let (endpoint, requests, server) = start_admin_sequence_test_server(vec![]);
    let output = run(
        &endpoint,
        &[
            "set",
            "myalias/photos",
            "--provider",
            "minio",
            "--endpoint",
            "https://source.example.com:9000",
            "--region",
            "us-east-1",
            "--source-bucket",
            "legacy-photos",
            "--access-key",
            "AKIASOURCE",
            "--json",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["type"], "on_demand_migration");
    assert_eq!(error["error"]["type"], "usage_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("RC_ODM_SECRET_KEY")
    );
    server.join().unwrap();
    assert!(requests.try_recv().is_err());
}

#[test]
fn a_server_without_the_route_family_is_reported_as_unsupported() {
    let (endpoint, _requests, server) =
        start_admin_sequence_test_server(vec![("404 Not Found", ""), ("404 Not Found", "")]);
    let human = run(&endpoint, &["get", "myalias/photos"], &[]);
    assert_eq!(human.status.code(), Some(7));
    assert!(stderr(&human).contains("server does not support on-demand migration"));
    assert!(human.stdout.is_empty());

    let json = run(&endpoint, &["status", "myalias/photos", "--json"], &[]);
    assert_eq!(json.status.code(), Some(7));
    let error: Value = serde_json::from_slice(&json.stderr).unwrap();
    assert_eq!(error["error"]["type"], "unsupported_feature");
    assert_eq!(error["error"]["capability"], "admin.on-demand-migration");
    server.join().unwrap();
}

#[test]
fn exit_codes_follow_the_server_answer() {
    let (endpoint, _requests, server) = start_admin_sequence_test_server(vec![
        (
            "404 Not Found",
            r#"{"Code":"NoSuchConfiguration","Message":"on-demand migration is not configured for bucket photos"}"#,
        ),
        (
            "409 Conflict",
            r#"{"Code":"OnDemandMigrationBackfillRunning","Message":"a backfill job is already running for bucket photos"}"#,
        ),
        (
            "400 Bad Request",
            r#"{"Code":"OnDemandMigrationSourceUnreachable","Message":"connect"}"#,
        ),
        (
            "403 Forbidden",
            r#"{"Code":"AccessDenied","Message":"licence does not include on-demand migration"}"#,
        ),
        (
            "400 Bad Request",
            r#"{"Code":"InvalidArgument","Message":"source_timeout.connect_ms out of range"}"#,
        ),
    ]);
    let not_found = run(&endpoint, &["get", "myalias/photos", "--json"], &[]);
    assert_eq!(not_found.status.code(), Some(5));

    let conflict = run(
        &endpoint,
        &["backfill", "start", "myalias/photos", "--json"],
        &[],
    );
    assert_eq!(conflict.status.code(), Some(6));
    let error: Value = serde_json::from_slice(&conflict.stderr).unwrap();
    assert_eq!(error["error"]["type"], "conflict");
    assert_eq!(error["error"]["retryable"], false);

    let set_args = [
        "set",
        "myalias/photos",
        "--provider",
        "s3",
        "--endpoint",
        "https://source.example.com",
        "--region",
        "us-east-1",
        "--source-bucket",
        "legacy",
        "--access-key",
        "AK",
        "--json",
    ];
    let secret = [("RC_ODM_SECRET_KEY", "sk")];
    let unreachable = run(&endpoint, &set_args, &secret);
    assert_eq!(unreachable.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&unreachable.stderr).unwrap();
    assert_eq!(error["error"]["type"], "network_error");
    assert!(
        error["error"]["suggestion"]
            .as_str()
            .unwrap()
            .contains("--dry-run")
    );

    let licence = run(&endpoint, &set_args, &secret);
    assert_eq!(licence.status.code(), Some(4));

    let invalid = run(&endpoint, &set_args, &secret);
    assert_eq!(invalid.status.code(), Some(2));
    server.join().unwrap();
}

#[test]
fn remove_and_backfill_control_use_their_routes() {
    let (endpoint, requests, server) = start_admin_sequence_test_server(vec![
        ("204 No Content", ""),
        ("200 OK", BACKFILL_JOB),
        ("200 OK", BACKFILL_JOB),
    ]);
    let removed = run(&endpoint, &["rm", "myalias/photos", "--json"], &[]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    let data: Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(data["data"]["operation"], "remove");
    assert_eq!(data["data"]["result"]["removed"], true);

    let started = run(
        &endpoint,
        &[
            "backfill",
            "start",
            "myalias/photos",
            "--prefix",
            "photos/",
            "--skip-existing",
            "etag_or_size",
            "--dry-run",
        ],
        &[],
    );
    assert!(started.status.success(), "{}", stderr(&started));
    let text = stdout(&started);
    assert!(text.contains("Backfill dry run started for 'photos'"));
    assert!(text.contains("State:                running"));
    assert!(text.contains("Owner:                node-a:9000 (lease until 2026-09-02T10:06:10Z)"));

    let cancelled = run(
        &endpoint,
        &["backfill", "cancel", "myalias/photos", "--json"],
        &[],
    );
    assert!(cancelled.status.success(), "{}", stderr(&cancelled));

    let delete = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(delete.method, "DELETE");
    assert_eq!(delete.target, "/rustfs/admin/v3/on-demand-migration/photos");
    let start = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(start.method, "POST");
    assert_eq!(
        start.target,
        "/rustfs/admin/v3/on-demand-migration/photos/backfill?op=start"
    );
    let body: Value = serde_json::from_slice(&start.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"prefix": "photos/", "skip_existing": "etag_or_size", "dry_run": true})
    );
    let cancel = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        cancel.target,
        "/rustfs/admin/v3/on-demand-migration/photos/backfill?op=cancel"
    );
    assert!(cancel.body.is_empty());
    server.join().unwrap();
}

#[test]
fn backfill_status_watch_streams_until_the_job_finishes() {
    let (endpoint, requests, server) = start_admin_sequence_test_server(vec![
        ("200 OK", BACKFILL_JOB),
        ("200 OK", BACKFILL_JOB_COMPLETED),
    ]);
    let output = run(
        &endpoint,
        &[
            "backfill",
            "status",
            "myalias/photos",
            "--watch",
            "--interval",
            "1",
            "--json",
        ],
        &[],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let lines = stdout(&output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["data"]["operation"], "backfill_status");
    assert_eq!(lines[0]["data"]["result"]["job"]["state"], "running");
    assert_eq!(lines[1]["data"]["result"]["job"]["state"], "completed");
    for _ in 0..2 {
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            request.target,
            "/rustfs/admin/v3/on-demand-migration/photos/backfill"
        );
    }
    server.join().unwrap();
}

#[test]
fn malformed_targets_and_missing_aliases_never_reach_the_server() {
    let config = tempfile::tempdir().unwrap();
    let output = Command::new(rc_binary())
        .args(["admin", "bucket", "migration", "get", "myalias", "--json"])
        .env("RC_CONFIG_DIR", config.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["type"], "usage_error");

    let output = Command::new(rc_binary())
        .args([
            "admin",
            "bucket",
            "migration",
            "status",
            "missing-odm-alias/photos",
            "--json",
        ])
        .env("RC_CONFIG_DIR", config.path())
        .env_remove("RC_HOST_missing-odm-alias")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["type"], "on_demand_migration");
    assert_eq!(error["error"]["type"], "not_found");
}
