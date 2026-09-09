//! `rc ilm rule import` JSON dialect tests.
//!
//! Import accepts the rc camelCase dialect plus the common alternative
//! spellings people paste (snake_case, S3/PascalCase, and the S3 JSON shape
//! with a nested `filter`/`abortIncompleteMultipartUpload`). Unknown rule
//! fields are rejected instead of silently dropped: a dropped action field
//! used to import a degraded rule that the server then refused with a
//! confusing "rule must have an action" error.

use rc_core::lifecycle::LifecycleConfiguration;

fn parse(json: &str) -> LifecycleConfiguration {
    serde_json::from_str(json).expect("lifecycle import should parse")
}

fn parse_error(json: &str) -> String {
    serde_json::from_str::<LifecycleConfiguration>(json)
        .expect_err("lifecycle import should fail")
        .to_string()
}

#[test]
fn camel_case_flat_dialect_round_trips() {
    let config = parse(
        r#"{"rules":[{"id":"r","status":"Enabled","prefix":"v1/","abortIncompleteMultipartUploadDays":1}]}"#,
    );
    assert_eq!(config.rules[0].prefix.as_deref(), Some("v1/"));
    assert_eq!(
        config.rules[0].abort_incomplete_multipart_upload_days,
        Some(1)
    );
}

#[test]
fn snake_case_flat_dialect_is_accepted() {
    let config = parse(
        r#"{"rules":[{"id":"r","status":"Enabled","prefix":"v1/","abort_incomplete_multipart_upload_days":1}]}"#,
    );
    assert_eq!(config.rules[0].prefix.as_deref(), Some("v1/"));
    assert_eq!(
        config.rules[0].abort_incomplete_multipart_upload_days,
        Some(1)
    );
}

#[test]
fn pascal_case_flat_dialect_is_accepted() {
    let config = parse(
        r#"{"Rules":[{"ID":"r","Status":"Enabled","Prefix":"v1/","AbortIncompleteMultipartUploadDays":1}]}"#,
    );
    assert_eq!(config.rules[0].prefix.as_deref(), Some("v1/"));
    assert_eq!(
        config.rules[0].abort_incomplete_multipart_upload_days,
        Some(1)
    );
}

#[test]
fn s3_shaped_filter_and_abort_action_are_accepted() {
    let config = parse(
        r#"{"rules":[{"id":"r","status":"Enabled","filter":{"prefix":"v1/"},"abortIncompleteMultipartUpload":{"daysAfterInitiation":2}}]}"#,
    );
    assert_eq!(config.rules[0].prefix.as_deref(), Some("v1/"));
    assert_eq!(
        config.rules[0].abort_incomplete_multipart_upload_days,
        Some(2)
    );
}

#[test]
fn s3_filter_and_combination_is_flattened() {
    let config = parse(
        r#"{"rules":[{"id":"r","status":"Enabled","filter":{"And":{"Prefix":"logs/","Tags":[{"Key":"env","Value":"prod"}],"ObjectSizeGreaterThan":100,"ObjectSizeLessThan":1000}}}]}"#,
    );
    assert_eq!(config.rules[0].prefix.as_deref(), Some("logs/"));
    assert_eq!(
        config.rules[0]
            .tags
            .as_ref()
            .and_then(|tags| tags.get("env")),
        Some(&"prod".to_string())
    );
    assert_eq!(config.rules[0].object_size_greater_than, Some(100));
    assert_eq!(config.rules[0].object_size_less_than, Some(1000));
}

#[test]
fn s3_standalone_object_size_greater_than_filter_is_accepted() {
    let config = parse(
        r#"{"Rules":[{"ID":"size-filter","Status":"Enabled","Filter":{"ObjectSizeGreaterThan":1024},"Expiration":{"Days":30}}]}"#,
    );
    assert_eq!(config.rules[0].object_size_greater_than, Some(1024));
    assert_eq!(config.rules[0].expiration.clone().unwrap().days, Some(30));
}

#[test]
fn s3_standalone_object_size_less_than_filter_is_accepted() {
    let config = parse(
        r#"{"rules":[{"id":"r","status":"Enabled","filter":{"ObjectSizeLessThan":65536},"expiration":{"days":7}}]}"#,
    );
    assert_eq!(config.rules[0].object_size_less_than, Some(65536));
    assert_eq!(config.rules[0].expiration.clone().unwrap().days, Some(7));
}

#[test]
fn nested_expired_object_delete_marker_is_accepted() {
    let config = parse(
        r#"{"rules":[{"id":"r","status":"Enabled","expiration":{"ExpiredObjectDeleteMarker":true}}]}"#,
    );
    assert_eq!(config.rules[0].expired_object_delete_marker, Some(true));
}

#[test]
fn unknown_rule_field_is_rejected_instead_of_dropped() {
    let error = parse_error(
        r#"{"rules":[{"id":"r","status":"Enabled","prefix":"v1/","daysAfterInitiation":1}]}"#,
    );
    assert!(
        error.contains("unknown field") && error.contains("daysAfterInitiation"),
        "error should name the unknown field: {error}"
    );
}

#[test]
fn flat_and_nested_abort_days_conflict_is_rejected() {
    let error = parse_error(
        r#"{"rules":[{"id":"r","status":"Enabled","abortIncompleteMultipartUploadDays":1,"abortIncompleteMultipartUpload":{"daysAfterInitiation":2}}]}"#,
    );
    assert!(
        error.contains("abortIncompleteMultipartUploadDays"),
        "{error}"
    );
}

#[test]
fn flat_prefix_and_filter_conflict_is_rejected() {
    let error = parse_error(
        r#"{"rules":[{"id":"r","status":"Enabled","prefix":"a/","filter":{"prefix":"b/"}}]}"#,
    );
    assert!(
        error.contains("prefix") && error.contains("Filter.Prefix"),
        "{error}"
    );
}

#[test]
fn filter_with_two_top_level_predicates_is_rejected() {
    let error = parse_error(
        r#"{"rules":[{"id":"r","status":"Enabled","filter":{"prefix":"a/","tag":{"key":"env","value":"prod"}}}]}"#,
    );
    assert!(
        error.contains(
            "exactly one of Prefix, Tag, And, ObjectSizeGreaterThan, or ObjectSizeLessThan"
        ),
        "{error}"
    );
}
