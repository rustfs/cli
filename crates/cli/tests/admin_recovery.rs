#![cfg(not(windows))]

mod admin_support;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use admin_support::{
    CapturedAdminRequest, rc_binary, rc_host_alias, start_admin_binary_sequence_test_server,
    start_admin_response_test_server,
};
use jsonschema::Validator;
use serde_json::{Value, json};

const ID: &str = "11111111-1111-4111-8111-111111111111";
const INCARNATION: &str = "22222222-2222-4222-8222-222222222222";
const NODE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST: &str = "q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s=";

fn validator() -> Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
    let schema: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

fn command(endpoint: &str, config: &Path, args: &[&str]) -> Output {
    Command::new(rc_binary())
        .arg("--json")
        .args(args)
        .env("RC_CONFIG_DIR", config)
        .env("RC_HOST_local", rc_host_alias(endpoint))
        .env("RC_HOST_second", rc_host_alias(endpoint))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .output()
        .expect("run recovery command")
}

fn record(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = if output.stdout.is_empty() {
        &output.stderr
    } else {
        assert!(output.stderr.is_empty(), "unexpected second JSON record");
        &output.stdout
    };
    let mut value: Value = serde_json::from_slice(bytes).expect("one JSON record");
    // Workspace feature unification may enable serde_json/preserve_order.
    // Object field order is not part of the JSON contract or these snapshots.
    value.sort_all_objects();
    let errors: Vec<_> = validator()
        .iter_errors(&value)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema errors: {errors:?}\n{value}");
    let encoded = value.to_string();
    assert!(!encoded.contains("ACCESS_KEY"));
    assert!(!encoded.contains("SECRET_KEY"));
    assert!(!encoded.contains("DO-NOT-ECHO"));
    value
}

fn run(args: &[&str], status: &str, body: Value, code: i32) -> (Value, CapturedAdminRequest) {
    let root = tempfile::tempdir().unwrap();
    let (endpoint, receiver, server) =
        start_admin_response_test_server(status, "application/json", body.to_string());
    let output = command(&endpoint, root.path(), args);
    let request = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    server.join().unwrap();
    assert!(
        request
            .headers
            .to_lowercase()
            .contains("authorization: aws4-hmac-sha256")
    );
    (record(&output, code), request)
}

fn observation(reply: Value) -> Value {
    json!({
        "contractVersion":1, "topologyFingerprint":"b".repeat(64),
        "observationStartedAtMs":10, "observationFinishedAtMs":20,
        "expectedNodes":1, "pageComplete":true, "nextCursor":null,
        "nodes":[{"nodeId":NODE,"host":"node-a","state":"ok","reply":reply}]
    })
}

fn summary() -> Value {
    json!({"result":"summary","summary":{
        "contractVersion":1,"sampledAtMs":9,"checkpointReadable":true,
        "checkpointOwner":ID,"checkpointSequence":1,"activeResponsibilityCount":0,
        "heldCount":2,"legacyGenerationUnknownCount":1,"bucketIncarnationChangedCount":0,
        "operatorAcceptedCount":1,"bestEffortQueueDepth":0,"responsibilityBytes":200
    }})
}

fn inventory() -> Value {
    json!({"observed_at":"2026-11-13T00:00:00Z","bucket_incarnation":INCARNATION,
        "items":[{"key":"object","version_id":"null","size":128,"protection":"legacy",
            "audit_unsupported":null,"source_fingerprint":"inventory-source","observation_error":null}],
        "is_truncated":false,"next_key_marker":null,"next_version_marker":null})
}

fn job(mode: &str, state: &str, item_state: &str) -> Value {
    let observed = item_state != "pending";
    json!({"format_version":1,"id":ID,"bucket":"bucket","bucket_incarnation":INCARNATION,
        "request":{"mode":mode,"items":[{"key":"object","version_id":"null",
            "expected_sha256":DIGEST,"target_key":(mode=="migrate").then_some("protected-copy")}],
            "bytes_per_second":65536,"max_object_bytes":1024},
        "state":state,"revision":1,
        "results":[{"state":item_state,"source_fingerprint":observed.then_some("executed-source"),
            "expected_sha256":observed.then_some(DIGEST),"evidence_source":observed.then_some("administrator_supplied_sha256"),
            "source_size":observed.then_some(128),"target_version_id":null,"detail":null}],"last_error":null})
}

fn readiness() -> Value {
    json!({"local_reader_supported":true,"write_requested":true,"fleet_operator_attested":true,
        "new_writes_enabled_here":true,"fleet_capability_verified":false,
        "unverified_requirements":["peer_reader_writer_and_repair_capabilities",
            "old_process_storage_admission","deployment_failure_and_performance_qualification"]})
}

fn write_selection(root: &Path, mode: &str) {
    std::fs::write(
        root.join("inventory.json"),
        json!({"bucket":"bucket","inventory":inventory()}).to_string(),
    )
    .unwrap();
    let mut items = job(mode, "paused", "pending")["request"]["items"].clone();
    items[0]["expected_sha256"] = json!("ab".repeat(32));
    std::fs::write(root.join("manifest.json"), items.to_string()).unwrap();
}

fn create_args<'a>(
    mode: &'a str,
    manifest: &'a str,
    inventory: &'a str,
    receipt: &'a str,
) -> Vec<&'a str> {
    vec![
        "admin",
        "integrity",
        mode,
        "local",
        "bucket",
        "--manifest",
        manifest,
        "--inventory-file",
        inventory,
        "--receipt",
        receipt,
        "--bytes-per-second",
        "65536",
        "--max-object-bytes",
        "1024",
        "--digest-source-reference",
        "backup/original-3177",
    ]
}

#[test]
fn mrf_json_preserves_unverified_summary_partial_coverage_and_absence() {
    let (value, request) = run(
        &["admin", "heal", "mrf", "status", "local"],
        "200 OK",
        observation(summary()),
        0,
    );
    assert_eq!(request.target, "/rustfs/admin/v4/heal/mrf/cluster");
    insta::assert_json_snapshot!("recovery_mrf_summary", value);
    let (value, _) = run(
        &["admin", "heal", "mrf", "list", "local", "--limit", "1"],
        "200 OK",
        observation(json!({"result":"list","observedAtMs":15,"snapshot":{
        "contractVersion":2,"nodeLocal":true,"nextCursor":null,"responsibilities":[{
            "responsibilityId":ID,"bucket":"bucket","object":"object",
            "sourceBucketIncarnationId":null,"bucketIncarnationId":INCARNATION,
            "versionId":null,"status":"legacy_generation_unknown","accepted":null
        }]}})),
        0,
    );
    assert!(value["data"]["nodes"][0]["reply"]["snapshot"]["responsibilities"][0]["sourceBucketIncarnationId"].is_null());
    let mut partial = observation(summary());
    partial["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"nodeId":"c".repeat(64),"host":"node-b","state":"unavailable","reply":null}));
    let mut unreadable = summary();
    unreadable["summary"]["checkpointReadable"] = json!(false);
    partial["nodes"].as_array_mut().unwrap().push(json!({
        "nodeId":"d".repeat(64),"host":"node-c","state":"error","reply":unreadable
    }));
    partial["expectedNodes"] = json!(3);
    partial["pageComplete"] = json!(false);
    let (value, _) = run(
        &["admin", "heal", "mrf", "status", "local"],
        "200 OK",
        partial,
        1,
    );
    assert_eq!(value["status"], "partial");
    assert!(value["data"]["nodes"][1]["reply"].is_null());
    assert_eq!(
        value["data"]["nodes"][2]["reply"]["summary"]["checkpointReadable"],
        false
    );
    assert_eq!(
        value["data"]["nodes"][2]["reply"]["summary"]["heldCount"],
        2
    );
    let (value, _) = run(
        &[
            "admin",
            "heal",
            "mrf",
            "get",
            "local",
            "--node-id",
            NODE,
            "--responsibility-id",
            ID,
        ],
        "200 OK",
        observation(
            json!({"result":"get","observation":{"contractVersion":1,"observedAtMs":12,"responsibility":null}}),
        ),
        1,
    );
    assert_eq!(value["status"], "not_observed");
    let mut invalid = value.clone();
    invalid["status"] = json!("verified");
    assert!(!validator().is_valid(&invalid));
}

#[test]
fn mrf_actions_distinguish_retained_acknowledgment_unknown_and_authentication_errors() {
    let args = [
        "admin",
        "heal",
        "mrf",
        "recheck",
        "local",
        "--node-id",
        NODE,
        "--responsibility-id",
        ID,
        "--expected-bucket-incarnation",
        INCARNATION,
    ];
    let (value, request) = run(
        &args,
        "200 OK",
        observation(json!({"result":"action","action":{
        "responsibilityId":ID,"state":"active","dataVerified":false,"durableResponsibilityRetained":true}})),
        0,
    );
    assert_eq!(value["status"], "accepted");
    assert_eq!(request.method, "POST");
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).unwrap()["request"]["expectedBucketIncarnationId"],
        INCARNATION
    );
    let mut invalid = value;
    invalid["data"]["nodes"][0]["reply"]["action"]["dataVerified"] = json!(true);
    assert!(!validator().is_valid(&invalid));
    let (value, _) = run(
        &args,
        "503 Service Unavailable",
        json!({"Message":"DO-NOT-ECHO"}),
        1,
    );
    assert_eq!(value["status"], "unknown");
    assert_eq!(value["error"]["automatic_retry"], false);
    insta::assert_json_snapshot!("recovery_unknown_action", value);
    let (value, _) = run(
        &args,
        "403 Forbidden",
        json!({"Code":"AccessDenied","Message":"DO-NOT-ECHO"}),
        4,
    );
    assert_eq!(value["status"], "error");
    insta::assert_json_snapshot!("recovery_auth_error", value);
}

#[test]
fn inventory_and_readiness_are_observations_not_integrity_certificates() {
    let (value, _) = run(
        &["admin", "integrity", "readiness", "local"],
        "200 OK",
        readiness(),
        0,
    );
    assert_eq!(value["type"], "integrity_readiness");
    assert_eq!(value["data"]["fleet_capability_verified"], false);
    let mut page = inventory();
    page["is_truncated"] = json!(true);
    page["next_key_marker"] = json!("object");
    page["next_version_marker"] = json!("null");
    let (value, request) = run(
        &[
            "admin",
            "integrity",
            "inventory",
            "local",
            "bucket",
            "--limit",
            "1",
        ],
        "200 OK",
        page,
        1,
    );
    assert_eq!(value["status"], "partial");
    assert_eq!(value["data"]["inventory"]["items"][0]["version_id"], "null");
    assert!(request.target.ends_with("?prefix=&limit=1"));
    let (value, _) = run(
        &["admin", "integrity", "job", "local", "bucket", ID],
        "403 Forbidden",
        json!({"Code":"AccessDenied","Message":"DO-NOT-ECHO"}),
        4,
    );
    assert_eq!(value["type"], "integrity");
    assert_eq!(value["status"], "error");
}

#[test]
fn audit_receipt_survives_a_new_client_and_is_required_before_explicit_resume() {
    let root = tempfile::tempdir().unwrap();
    write_selection(root.path(), "audit");
    let manifest = root.path().join("manifest.json");
    let inventory = root.path().join("inventory.json");
    let receipt = root.path().join("receipt.json");
    let export = root.path().join("result.json");
    let responses = [
        ("paused", "pending"),
        ("paused", "pending"),
        ("running", "pending"),
        ("complete", "verified"),
    ]
    .into_iter()
    .map(|(state, item)| {
        (
            "200 OK",
            "application/json",
            job("audit", state, item).to_string().into_bytes(),
        )
    })
    .collect();
    let (endpoint, receiver, server) = start_admin_binary_sequence_test_server(responses);
    let args = create_args(
        "audit",
        manifest.to_str().unwrap(),
        inventory.to_str().unwrap(),
        receipt.to_str().unwrap(),
    );
    let created = record(&command(&endpoint, root.path(), &args), 0);
    assert_eq!(created["status"], "paused_receipt_saved");
    let saved: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(created["data"], saved);
    assert_eq!(
        saved["job"]["request"]["items"][0]["expected_sha256"],
        DIGEST
    );
    assert_eq!(saved["estimate"]["stream_passes"], 1);
    assert_eq!(saved["estimate"]["minimum_stream_seconds"], 0.001953125);
    assert_eq!(saved["estimate"]["peak_payload_staging_budget"], 1024);
    let mut snapshot = created.clone();
    snapshot["data"]["endpoint"] = json!("http://server:9000/");
    snapshot["data"]["resume_command"][8] = json!("receipt.json");
    insta::assert_json_snapshot!("recovery_paused_receipt", snapshot);
    let resumed = record(
        &command(
            &endpoint,
            root.path(),
            &[
                "admin",
                "integrity",
                "resume",
                "second",
                "bucket",
                ID,
                "--receipt",
                receipt.to_str().unwrap(),
            ],
        ),
        0,
    );
    assert_eq!(resumed["status"], "accepted");
    let completed = record(
        &command(
            &endpoint,
            root.path(),
            &[
                "admin",
                "integrity",
                "job",
                "second",
                "bucket",
                ID,
                "--output",
                export.to_str().unwrap(),
            ],
        ),
        0,
    );
    assert!(completed["data"]["checked_at"].is_null());
    assert!(completed["data"]["retrieved_at"].is_string());
    assert_eq!(completed["data"]["source_responsibility_retained"], true);
    let exported: Value = serde_json::from_slice(&std::fs::read(export).unwrap()).unwrap();
    assert_eq!(exported, completed["data"]);
    let mut snapshot = completed.clone();
    snapshot["data"]["retrieved_at"] = json!("2026-11-13T00:00:01Z");
    insta::assert_json_snapshot!("recovery_verified_job", snapshot);
    server.join().unwrap();
    let requests: Vec<_> = receiver.try_iter().collect();
    assert_eq!(
        requests.len(),
        4,
        "create must not automatically resume or poll"
    );
    assert_eq!(requests[0].target, "/rustfs/admin/v3/integrity/bucket/jobs");
    assert_eq!(requests[1].method, "GET");
    assert!(requests[2].target.ends_with("/control"));
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[2].body).unwrap(),
        json!({"operation":"resume"})
    );
}

#[test]
fn completed_job_without_evidence_and_pending_control_have_nonzero_exits() {
    let mut result = job("audit", "complete", "verified");
    result["results"][0]["evidence_source"] = Value::Null;
    let (value, _) = run(
        &["admin", "integrity", "job", "local", "bucket", ID],
        "200 OK",
        result,
        1,
    );
    assert_eq!(value["status"], "not_verified");
    let (value, _) = run(
        &["admin", "integrity", "pause", "local", "bucket", ID],
        "200 OK",
        job("audit", "running", "pending"),
        1,
    );
    assert_eq!(value["status"], "incomplete");
}

#[test]
fn qualified_migration_receipt_keeps_external_attestation_separate_from_local_readiness() {
    let root = tempfile::tempdir().unwrap();
    write_selection(root.path(), "migrate");
    let manifest = root.path().join("manifest.json");
    let inventory = root.path().join("inventory.json");
    let receipt = root.path().join("receipt.json");
    let (endpoint, receiver, server) = start_admin_binary_sequence_test_server(vec![
        (
            "200 OK",
            "application/json",
            readiness().to_string().into_bytes(),
        ),
        (
            "200 OK",
            "application/json",
            job("migrate", "paused", "pending").to_string().into_bytes(),
        ),
    ]);
    let mut args = create_args(
        "migrate",
        manifest.to_str().unwrap(),
        inventory.to_str().unwrap(),
        receipt.to_str().unwrap(),
    );
    args.extend([
        "--deployment-id",
        "deployment-a",
        "--qualification-reference",
        "qualification-3177",
        "--acknowledge-qualified-deployment",
    ]);
    let created = record(&command(&endpoint, root.path(), &args), 0);
    assert_eq!(created["data"]["qualification"]["operator_attested"], true);
    assert_eq!(
        created["data"]["readiness"]["fleet_capability_verified"],
        false
    );
    assert_eq!(created["data"]["estimate"]["stream_passes"], 3);
    assert_eq!(
        created["data"]["estimate"]["minimum_stream_seconds"],
        0.005859375
    );
    assert_eq!(
        created["data"]["estimate"]["peak_payload_staging_budget"],
        1024
    );
    server.join().unwrap();
    assert_eq!(receiver.try_iter().count(), 2);
}
