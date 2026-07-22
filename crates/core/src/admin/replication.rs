//! Typed contracts for read-only replication diff inspection.

use std::collections::BTreeMap;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;

/// Maximum encoded size accepted for one replication diff response.
pub const MAX_REPLICATION_DIFF_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A bounded, on-demand scan of object versions that have not replicated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationDiff {
    #[serde(rename = "Entries")]
    pub entries: Vec<ReplicationDiffEntry>,
    #[serde(rename = "IsTruncated")]
    pub is_truncated: bool,
    #[serde(rename = "ScannedVersions")]
    pub scanned_versions: usize,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

/// One pending or failed object version returned by a replication diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationDiffEntry {
    #[serde(rename = "Object")]
    pub object: String,
    #[serde(rename = "VersionID")]
    pub version_id: Option<String>,
    #[serde(rename = "Size")]
    pub size_bytes: u64,
    #[serde(rename = "IsDeleteMarker")]
    pub delete_marker: bool,
    #[serde(rename = "ReplicationStatus")]
    pub replication_status: String,
    #[serde(rename = "LastModified")]
    pub last_modified: Option<Timestamp>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

/// Read-only RustFS replication diff operations.
#[async_trait]
pub trait ReplicationDiffApi: Send + Sync {
    /// Scan a bucket, optionally below a prefix, for pending or failed versions.
    async fn replication_diff(&self, bucket: &str, prefix: Option<&str>)
    -> Result<ReplicationDiff>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_preserves_unknown_fields_and_typed_entries() {
        let response: ReplicationDiff = serde_json::from_str(
            r#"{
                "Entries": [{
                    "Object": "reports/a.json",
                    "VersionID": "v1",
                    "Size": 42,
                    "IsDeleteMarker": false,
                    "ReplicationStatus": "FAILED",
                    "LastModified": "2026-07-21T04:00:00Z",
                    "TargetDetail": {"attempts": 2}
                }],
                "IsTruncated": true,
                "ScannedVersions": 10000,
                "ServerRevision": 7
            }"#,
        )
        .expect("typed replication diff");

        assert_eq!(response.entries[0].size_bytes, 42);
        assert_eq!(response.entries[0].version_id.as_deref(), Some("v1"));
        assert_eq!(response.entries[0].extra["TargetDetail"]["attempts"], 2);
        assert_eq!(response.extra["ServerRevision"], 7);
    }

    #[test]
    fn response_accepts_delete_marker_without_version_or_timestamp() {
        let response: ReplicationDiff = serde_json::from_str(
            r#"{
                "Entries": [{
                    "Object": "removed.txt",
                    "VersionID": null,
                    "Size": 0,
                    "IsDeleteMarker": true,
                    "ReplicationStatus": "PENDING",
                    "LastModified": null
                }],
                "IsTruncated": false,
                "ScannedVersions": 1
            }"#,
        )
        .expect("delete marker diff");

        assert!(response.entries[0].delete_marker);
        assert!(response.entries[0].version_id.is_none());
        assert!(response.entries[0].last_modified.is_none());
    }

    #[test]
    fn response_rejects_negative_sizes_and_malformed_timestamps() {
        for payload in [
            r#"{"Entries":[{"Object":"a","VersionID":null,"Size":-1,"IsDeleteMarker":false,"ReplicationStatus":"FAILED","LastModified":null}],"IsTruncated":false,"ScannedVersions":1}"#,
            r#"{"Entries":[{"Object":"a","VersionID":null,"Size":1,"IsDeleteMarker":false,"ReplicationStatus":"FAILED","LastModified":"yesterday"}],"IsTruncated":false,"ScannedVersions":1}"#,
        ] {
            assert!(serde_json::from_str::<ReplicationDiff>(payload).is_err());
        }
    }

    #[test]
    fn response_requires_scan_completeness_fields() {
        let payload = r#"{"Entries":[]}"#;
        assert!(serde_json::from_str::<ReplicationDiff>(payload).is_err());
    }
}
