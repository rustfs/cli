//! Typed contracts for read-only replication inspection.

use std::collections::BTreeMap;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;

/// Maximum encoded size accepted for one replication diff response.
pub const MAX_REPLICATION_DIFF_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum encoded size accepted for metrics and MRF responses.
pub const MAX_REPLICATION_INSPECTION_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Scope of a replication observation. Unknown values are retained for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicationMetricScope {
    Unavailable,
    NodeLocal,
    ClusterAggregated,
    PartialCluster,
    Unknown(String),
}

impl ReplicationMetricScope {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unavailable => "unavailable",
            Self::NodeLocal => "node_local",
            Self::ClusterAggregated => "cluster_aggregated",
            Self::PartialCluster => "partial_cluster",
            Self::Unknown(value) => value,
        }
    }
}

impl Serialize for ReplicationMetricScope {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReplicationMetricScope {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "unavailable" => Self::Unavailable,
            "node_local" => Self::NodeLocal,
            "cluster_aggregated" => Self::ClusterAggregated,
            "partial_cluster" => Self::PartialCluster,
            _ => Self::Unknown(value),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplicationCountSize {
    pub count: u64,
    #[serde(rename = "bytes", alias = "size")]
    pub size: u64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplicationQueueMetric {
    pub curr: ReplicationCountSize,
    pub avg: ReplicationCountSize,
    pub max: ReplicationCountSize,
    pub last_minute: ReplicationCountSize,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplicationLatencyMetric {
    pub avg: f64,
    pub curr: f64,
    pub max: f64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplicationTransferRate {
    pub avg: f64,
    pub curr: f64,
    pub peak: f64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationTargetMetric {
    pub replicated_size: u64,
    pub replicated_count: u64,
    pub failed: ReplicationCountSize,
    #[serde(default)]
    pub fail_stats: Option<ReplicationCountSize>,
    pub latency: ReplicationLatencyMetric,
    pub xfer_rate_lrg: ReplicationTransferRate,
    pub xfer_rate_sml: ReplicationTransferRate,
    pub bandwidth_limit_bytes_per_sec: u64,
    pub current_bandwidth_bytes_per_sec: f64,
    #[serde(default)]
    pub latency_scope: Option<ReplicationMetricScope>,
    #[serde(default)]
    pub bandwidth_scope: Option<ReplicationMetricScope>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationMetrics {
    pub stats: BTreeMap<String, ReplicationTargetMetric>,
    pub replica_size: u64,
    pub replica_count: u64,
    pub replicated_size: u64,
    pub replicated_count: u64,
    pub q_stat: ReplicationQueueMetric,
    #[serde(default)]
    pub provider_available: Option<bool>,
    #[serde(default)]
    pub cluster_complete: Option<bool>,
    #[serde(default)]
    pub observed_node_count: Option<u32>,
    #[serde(default)]
    pub expected_node_count: Option<u32>,
    #[serde(default)]
    pub queue_scope: Option<ReplicationMetricScope>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationMrfTarget {
    #[serde(rename = "ARN")]
    pub arn: String,
    #[serde(rename = "FailedCount")]
    pub failed_count: u64,
    #[serde(rename = "FailedSize")]
    pub failed_size: u64,
    #[serde(rename = "ObservationScope")]
    pub observation_scope: ReplicationMetricScope,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationMrf {
    #[serde(rename = "Bucket")]
    pub bucket: String,
    #[serde(rename = "Targets")]
    pub targets: Vec<ReplicationMrfTarget>,
    #[serde(rename = "TotalFailedCount")]
    pub total_failed_count: u64,
    #[serde(rename = "TotalFailedSize")]
    pub total_failed_size: u64,
    #[serde(rename = "QueuedCount")]
    pub queued_count: u64,
    #[serde(rename = "QueuedSize")]
    pub queued_size: u64,
    #[serde(rename = "PerObjectEntriesAvailable")]
    pub per_object_entries_available: bool,
    #[serde(rename = "RuntimeStatsAvailable")]
    pub runtime_stats_available: bool,
    #[serde(rename = "ClusterComplete")]
    pub cluster_complete: bool,
    #[serde(rename = "ObservedNodeCount")]
    pub observed_node_count: u32,
    #[serde(rename = "ExpectedNodeCount")]
    pub expected_node_count: u32,
    #[serde(rename = "DurableBacklogAvailable")]
    pub durable_backlog_available: bool,
    #[serde(rename = "DurableCount")]
    pub durable_count: u64,
    #[serde(rename = "DurableSize")]
    pub durable_size: u64,
    #[serde(rename = "PerTargetDurableEntriesAvailable")]
    pub per_target_durable_entries_available: bool,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[async_trait]
pub trait ReplicationInspectionApi: Send + Sync {
    async fn replication_metrics(&self, bucket: &str) -> Result<ReplicationMetrics>;
    async fn replication_mrf(&self, bucket: &str) -> Result<ReplicationMrf>;
}

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

    #[test]
    fn metrics_distinguish_legacy_metadata_and_preserve_unknown_scope() {
        let legacy: ReplicationMetrics = serde_json::from_str(r#"{"stats":{},"replica_size":0,"replica_count":0,"replicated_size":0,"replicated_count":0,"q_stat":{"curr":{"count":0,"size":0},"avg":{"count":0,"size":0},"max":{"count":0,"size":0},"last_minute":{"count":0,"size":0}}}"#).expect("legacy metrics");
        assert_eq!(legacy.provider_available, None);
        assert_eq!(legacy.cluster_complete, None);

        let current: ReplicationMetrics = serde_json::from_str(r#"{"stats":{},"replica_size":0,"replica_count":0,"replicated_size":0,"replicated_count":0,"q_stat":{"curr":{"count":0,"size":0},"avg":{"count":0,"size":0},"max":{"count":0,"size":0},"last_minute":{"count":0,"size":0}},"provider_available":true,"cluster_complete":false,"observed_node_count":1,"expected_node_count":2,"queue_scope":"future_scope"}"#).expect("current metrics");
        assert_eq!(current.provider_available, Some(true));
        assert_eq!(
            current.queue_scope,
            Some(ReplicationMetricScope::Unknown("future_scope".into()))
        );
    }

    #[test]
    fn metrics_and_mrf_reject_negative_counters() {
        let metrics = r#"{"stats":{},"replica_size":-1,"replica_count":0,"replicated_size":0,"replicated_count":0,"q_stat":{"curr":{"count":0,"size":0},"avg":{"count":0,"size":0},"max":{"count":0,"size":0},"last_minute":{"count":0,"size":0}}}"#;
        assert!(serde_json::from_str::<ReplicationMetrics>(metrics).is_err());
        let mrf = r#"{"Bucket":"b","Targets":[],"TotalFailedCount":-1,"TotalFailedSize":0,"QueuedCount":0,"QueuedSize":0,"PerObjectEntriesAvailable":false,"RuntimeStatsAvailable":true,"ClusterComplete":false,"ObservedNodeCount":1,"ExpectedNodeCount":2,"DurableBacklogAvailable":false,"DurableCount":0,"DurableSize":0,"PerTargetDurableEntriesAvailable":false}"#;
        assert!(serde_json::from_str::<ReplicationMrf>(mrf).is_err());
    }
}
