use std::time::Duration;

use rc_core::ops::{
    HealthProbe, UsageBucket, UsageReport, UsageScanRequest, UsageScope, UsageSource,
};

#[test]
fn health_probe_paths_match_rustfs_beta_10_contract() {
    assert_eq!(HealthProbe::Liveness.path(), "/health");
    assert_eq!(HealthProbe::Readiness.path(), "/health/ready");
}

#[test]
fn usage_report_totals_are_deterministic_and_saturating() {
    let mut report = UsageReport::empty(UsageSource::ClientScan, UsageScope::Cluster, None);
    report.push_bucket(UsageBucket {
        name: "zeta".to_string(),
        total_bytes: 7,
        object_count: 2,
        version_count: Some(3),
        delete_marker_count: Some(1),
        incomplete_upload_count: Some(1),
        incomplete_upload_bytes: Some(5),
    });
    report.push_bucket(UsageBucket {
        name: "alpha".to_string(),
        total_bytes: 11,
        object_count: 4,
        version_count: Some(6),
        delete_marker_count: Some(2),
        incomplete_upload_count: Some(0),
        incomplete_upload_bytes: Some(0),
    });
    report.finish();

    assert_eq!(report.total_bytes, 18);
    assert_eq!(report.object_count, 6);
    assert_eq!(report.version_count, Some(9));
    assert_eq!(report.delete_marker_count, Some(3));
    assert_eq!(report.incomplete_upload_count, Some(1));
    assert_eq!(report.incomplete_upload_bytes, Some(5));
    assert_eq!(report.buckets[0].name, "alpha");
    assert_eq!(report.buckets[1].name, "zeta");
}

#[test]
fn usage_scan_request_exposes_expensive_dimensions_explicitly() {
    let request = UsageScanRequest {
        bucket: Some("photos".to_string()),
        prefix: Some("2026/".to_string()),
        include_versions: true,
        include_incomplete_uploads: true,
    };

    assert_eq!(request.scope(), UsageScope::Prefix);
    assert!(request.requires_client_scan());
    assert_eq!(
        request.path("local"),
        Some("local/photos/2026/".to_string())
    );
    assert_eq!(Duration::from_secs(5).as_millis(), 5_000);
}
