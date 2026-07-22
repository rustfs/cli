//! Contract tests for the versioned JSON output schemas and v3 golden fixtures.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::Validator;
use serde_json::Value;

const V3_FAMILIES: &[&str] = &[
    "capabilities",
    "versioned_objects",
    "locks",
    "multipart_uploads",
    "watch_event",
    "health",
    "usage",
    "metrics",
    "admin_operations",
    "replication",
    "replication_operations",
];

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CLI crate must be inside the workspace")
        .to_path_buf()
}

fn load_json(path: &Path) -> Value {
    let contents = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn load_validator(version: u8) -> Validator {
    let path = repository_root()
        .join("schemas")
        .join(format!("output_v{version}.json"));
    let schema = load_json(&path);
    jsonschema::validator_for(&schema)
        .unwrap_or_else(|error| panic!("failed to compile {}: {error}", path.display()))
}

fn assert_valid(validator: &Validator, value: &Value, label: &str) {
    let errors = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "{label} does not satisfy its output contract:\n{}",
        errors.join("\n")
    );
}

fn fixture_path(family: &str, case: &str) -> PathBuf {
    let extension = if family == "watch_event" {
        "jsonl"
    } else {
        "json"
    };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/output_v3")
        .join(family)
        .join(format!("{case}.{extension}"))
}

fn version_operation_fixture_path(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/output_v3/version_operations")
        .join(format!("{case}.json"))
}

fn snapshot_payload(contents: &str) -> Option<&str> {
    contents
        .split_once("\n---\n")
        .or_else(|| contents.split_once("\r\n---\r\n"))
        .map(|(_, payload)| payload)
}

#[test]
fn snapshot_payload_accepts_lf_and_crlf_delimiters() {
    assert_eq!(
        snapshot_payload("header\n---\n{\"schema_version\":1}\n"),
        Some("{\"schema_version\":1}\n")
    );
    assert_eq!(
        snapshot_payload("header\r\n---\r\n{\"schema_version\":1}\r\n"),
        Some("{\"schema_version\":1}\r\n")
    );
}

#[test]
fn every_v3_family_has_valid_success_empty_and_error_fixtures() {
    let validator = load_validator(3);

    for family in V3_FAMILIES {
        for case in ["success", "empty", "error"] {
            let path = fixture_path(family, case);
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));

            if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                let mut record_count = 0;
                for (line_index, line) in contents.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    record_count += 1;
                    let value: Value = serde_json::from_str(line).unwrap_or_else(|error| {
                        panic!(
                            "failed to parse {} line {}: {error}",
                            path.display(),
                            line_index + 1
                        )
                    });
                    assert_valid(
                        &validator,
                        &value,
                        &format!("{} line {}", path.display(), line_index + 1),
                    );
                }
                assert!(record_count > 0, "{} must contain a record", path.display());
            } else {
                let value: Value = serde_json::from_str(&contents)
                    .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
                assert_valid(&validator, &value, &path.display().to_string());
            }
        }
    }
}

#[test]
fn version_operation_success_empty_error_and_dry_run_fixtures_are_valid() {
    let validator = load_validator(3);

    for case in ["success", "empty", "error", "dry_run", "stat"] {
        let path = version_operation_fixture_path(case);
        let value = load_json(&path);
        assert_valid(&validator, &value, &path.display().to_string());
    }
}

#[test]
fn version_remove_contract_requires_version_aware_partial_results() {
    let validator = load_validator(3);
    let fixture = load_json(&version_operation_fixture_path("error"));

    assert_eq!(fixture["status"], "error");
    assert_eq!(fixture["data"]["outcome"], "partial");
    assert_eq!(fixture["data"]["removed"][0]["version_id"], "v1");
    assert_eq!(fixture["data"]["failed"][0]["version_id"], "v2");

    let mut missing_version = fixture;
    missing_version["data"]["failed"][0]
        .as_object_mut()
        .expect("failure must be an object")
        .remove("version_id");
    assert!(
        !validator.is_valid(&missing_version),
        "version removal failures must preserve the selected version ID field"
    );
}

#[test]
fn version_dry_run_contract_distinguishes_planned_from_removed_items() {
    let validator = load_validator(3);
    let fixture = load_json(&version_operation_fixture_path("dry_run"));

    assert_eq!(fixture["data"]["dry_run"], true);
    assert_eq!(fixture["data"]["outcome"], "planned");
    assert_eq!(fixture["data"]["planned"].as_array().map(Vec::len), Some(1));
    assert_eq!(fixture["data"]["removed"].as_array().map(Vec::len), Some(0));
    assert_valid(&validator, &fixture, "version removal dry-run fixture");

    let mut contradictory = fixture;
    contradictory["data"]["removed"] = serde_json::json!([{
        "path": "local/photos/image.jpg",
        "version_id": "v1",
        "delete_marker": false
    }]);
    assert!(
        !validator.is_valid(&contradictory),
        "dry-run records must never claim that an object version was removed"
    );
}

#[test]
fn legacy_schemas_compile_and_existing_v1_golden_snapshots_remain_valid() {
    let v1_validator = load_validator(1);
    // Compiling v2 guards its existing references even though this repository does not yet
    // contain v2 golden snapshots. Applying v2 to v1 alias records is invalid because some
    // historical v2 `oneOf` operation envelopes overlap; v3 must not reinterpret that contract.
    let _v2_validator = load_validator(2);
    let snapshot_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");

    for entry in fs::read_dir(&snapshot_dir).expect("snapshot directory must be readable") {
        let path = entry.expect("snapshot entry must be readable").path();
        if path.extension().and_then(|value| value.to_str()) != Some("snap") {
            continue;
        }

        let contents = fs::read_to_string(&path).expect("snapshot must be readable");
        let payload =
            snapshot_payload(&contents).expect("snapshot must contain a payload delimiter");
        let value: Value = serde_json::from_str(payload).expect("snapshot payload must be JSON");

        assert_valid(
            &v1_validator,
            &value,
            &format!("{} against output v1", path.display()),
        );
    }
}

#[test]
fn v3_allows_unknown_server_fields() {
    let validator = load_validator(3);
    let mut value = load_json(&fixture_path("capabilities", "success"));
    value["data"]["server_extension"] = serde_json::json!({ "revision": 7 });
    value["data"]["capabilities"][0]["server_detail"] =
        serde_json::json!({ "route": "/minio/admin/v4/runtime/capabilities" });

    assert_valid(&validator, &value, "extended capabilities output");
}

#[test]
fn replication_truncated_fixture_is_explicitly_non_resumable() {
    let validator = load_validator(3);
    let value = load_json(&fixture_path("replication", "truncated"));

    assert_eq!(value["data"]["scan"]["truncated"], true);
    assert_eq!(value["data"]["scan"]["resumable"], false);
    assert_valid(&validator, &value, "truncated replication diff");
}

#[test]
fn replication_status_allows_empty_reset_id_but_start_requires_exactly_one_target() {
    let validator = load_validator(3);
    let status = load_json(&fixture_path("replication_operations", "success"));
    assert_eq!(status["data"]["targets"][0]["reset_id"], "");
    assert_valid(&validator, &status, "status with persisted empty reset ID");

    let mut start = status;
    start["data"]["operation"] = Value::String("resync_start".to_string());
    assert!(
        !validator.is_valid(&start),
        "start output must retain a nonempty server reset ID"
    );
    start["data"]["targets"][0]["reset_id"] = Value::String("server-id".to_string());
    assert_valid(&validator, &start, "start with nonempty server reset ID");

    let mut empty_targets = start.clone();
    empty_targets["data"]["targets"] = serde_json::json!([]);
    assert!(
        !validator.is_valid(&empty_targets),
        "start output must contain exactly one target"
    );

    let mut multiple_targets = start;
    let target = multiple_targets["data"]["targets"][0].clone();
    multiple_targets["data"]["targets"] = serde_json::json!([target.clone(), target]);
    assert!(
        !validator.is_valid(&multiple_targets),
        "start output must not contain multiple targets"
    );
}

#[test]
fn v3_rejects_field_renames_and_type_changes() {
    let validator = load_validator(3);
    let fixture = load_json(&fixture_path("versioned_objects", "success"));

    let mut renamed = fixture.clone();
    let status = renamed
        .as_object_mut()
        .expect("fixture must be an object")
        .remove("status")
        .expect("fixture must contain status");
    renamed["state"] = status;
    assert!(!validator.is_valid(&renamed), "renamed fields must fail");

    let mut wrong_type = fixture;
    wrong_type["data"]["items"][0]["size_bytes"] = Value::String("1024".to_string());
    assert!(
        !validator.is_valid(&wrong_type),
        "byte counts encoded as strings must fail"
    );
}
