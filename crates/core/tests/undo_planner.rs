use jiff::Timestamp;
use rc_core::{CopyObjectOptions, ObjectVersion, UndoAction, plan_object_undo};

fn version(
    key: &str,
    version_id: &str,
    modified: &str,
    is_latest: bool,
    is_delete_marker: bool,
) -> ObjectVersion {
    ObjectVersion {
        key: key.to_string(),
        version_id: version_id.to_string(),
        is_latest,
        is_delete_marker,
        last_modified: Some(modified.parse::<Timestamp>().expect("valid timestamp")),
        size_bytes: (!is_delete_marker).then_some(12),
        etag: (!is_delete_marker).then(|| format!("etag-{version_id}")),
    }
}

#[test]
fn latest_delete_marker_plans_exact_marker_removal() {
    let history = vec![
        version(
            "report.txt",
            "marker-v3",
            "2026-07-23T03:00:00Z",
            true,
            true,
        ),
        version(
            "report.txt",
            "data-v2",
            "2026-07-23T02:00:00Z",
            false,
            false,
        ),
    ];

    let plan = plan_object_undo("report.txt", &history, None).expect("reversible delete");

    assert_eq!(plan.expected_latest_version_id, "marker-v3");
    assert_eq!(
        plan.action,
        UndoAction::RemoveDeleteMarker {
            marker_version_id: "marker-v3".to_string(),
            revealed_version_id: "data-v2".to_string(),
        }
    );
}

#[test]
fn latest_overwrite_plans_previous_data_version_restore() {
    let history = vec![
        version("report.txt", "data-v3", "2026-07-23T03:00:00Z", true, false),
        version(
            "report.txt",
            "data-v2",
            "2026-07-23T02:00:00Z",
            false,
            false,
        ),
        version(
            "report.txt",
            "data-v1",
            "2026-07-23T01:00:00Z",
            false,
            false,
        ),
    ];

    let plan = plan_object_undo("report.txt", &history, None).expect("reversible overwrite");

    assert_eq!(plan.expected_latest_version_id, "data-v3");
    assert_eq!(
        plan.action,
        UndoAction::RestoreVersion {
            source_version_id: "data-v2".to_string(),
        }
    );
}

#[test]
fn explicit_historical_data_version_is_restored() {
    let history = vec![
        version("report.txt", "data-v3", "2026-07-23T03:00:00Z", true, false),
        version(
            "report.txt",
            "data-v2",
            "2026-07-23T02:00:00Z",
            false,
            false,
        ),
        version(
            "report.txt",
            "data-v1",
            "2026-07-23T01:00:00Z",
            false,
            false,
        ),
    ];

    let plan = plan_object_undo("report.txt", &history, Some("data-v1")).expect("explicit restore");

    assert_eq!(
        plan.action,
        UndoAction::RestoreVersion {
            source_version_id: "data-v1".to_string(),
        }
    );
}

#[test]
fn missing_or_ambiguous_history_is_rejected() {
    let first_put = vec![version(
        "report.txt",
        "data-v1",
        "2026-07-23T01:00:00Z",
        true,
        false,
    )];
    assert!(plan_object_undo("report.txt", &first_put, None).is_err());

    let two_latest = vec![
        version("report.txt", "data-v2", "2026-07-23T02:00:00Z", true, false),
        version("report.txt", "data-v1", "2026-07-23T01:00:00Z", true, false),
    ];
    assert!(plan_object_undo("report.txt", &two_latest, None).is_err());

    let tied_predecessors = vec![
        version("report.txt", "data-v3", "2026-07-23T03:00:00Z", true, false),
        version(
            "report.txt",
            "data-v2a",
            "2026-07-23T02:00:00Z",
            false,
            false,
        ),
        version(
            "report.txt",
            "data-v2b",
            "2026-07-23T02:00:00Z",
            false,
            false,
        ),
    ];
    assert!(plan_object_undo("report.txt", &tied_predecessors, None).is_err());
}

#[test]
fn overwrite_after_delete_marker_is_refused_instead_of_guessing() {
    let history = vec![
        version("report.txt", "data-v3", "2026-07-23T03:00:00Z", true, false),
        version(
            "report.txt",
            "marker-v2",
            "2026-07-23T02:00:00Z",
            false,
            true,
        ),
        version(
            "report.txt",
            "data-v1",
            "2026-07-23T01:00:00Z",
            false,
            false,
        ),
    ];

    let error = plan_object_undo("report.txt", &history, None).expect_err("ambiguous undo");
    assert!(error.to_string().contains("delete marker"));
}

#[test]
fn copy_options_reject_empty_source_version_ids() {
    assert!(CopyObjectOptions::for_source_version(Some(String::new())).is_err());
    assert_eq!(
        CopyObjectOptions::for_source_version(Some("data-v1".to_string()))
            .expect("valid source version")
            .source_version_id
            .as_deref(),
        Some("data-v1")
    );
}
