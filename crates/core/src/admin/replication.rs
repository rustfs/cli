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

#[derive(Debug, Clone, PartialEq, Serialize)]
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

#[derive(Debug, Default)]
enum WireField<T> {
    #[default]
    Missing,
    Present(T),
}

impl<T> WireField<T> {
    fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    fn require(self, name: &str) -> std::result::Result<T, String> {
        match self {
            Self::Present(value) => Ok(value),
            Self::Missing => Err(format!("missing field `{name}`")),
        }
    }
}

impl<'de, T> Deserialize<'de> for WireField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Debug, Clone, Copy)]
struct WireCounter(u64);

impl<'de> Deserialize<'de> for WireCounter {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Keep the original token so decimal counters cannot be silently rounded
        // through f64 before their integer semantics are validated.
        let raw = <&serde_json::value::RawValue>::deserialize(deserializer)?;
        parse_wire_counter(raw.get())
            .map(WireCounter)
            .map_err(serde::de::Error::custom)
    }
}

fn parse_wire_counter(raw: &str) -> std::result::Result<u64, &'static str> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes[0] == b'-' {
        return Err("counter cannot be negative");
    }
    if !bytes[0].is_ascii_digit() {
        return Err("counter must be a JSON number");
    }

    let exponent_start = bytes
        .iter()
        .position(|byte| matches!(byte, b'e' | b'E'))
        .unwrap_or(bytes.len());
    let mantissa = &bytes[..exponent_start];
    let decimal = mantissa.iter().position(|byte| *byte == b'.');
    let (integer_digits, fraction_digits) = match decimal {
        Some(index) => (&mantissa[..index], &mantissa[index + 1..]),
        None => (mantissa, &[][..]),
    };
    if integer_digits.is_empty()
        || !integer_digits.iter().all(u8::is_ascii_digit)
        || !fraction_digits.iter().all(u8::is_ascii_digit)
    {
        return Err("counter must be a JSON number");
    }

    let digits = integer_digits.iter().chain(fraction_digits);
    if digits.clone().all(|digit| *digit == b'0') {
        return Ok(0);
    }

    let exponent = if exponent_start == bytes.len() {
        0_i64
    } else {
        parse_counter_exponent(&bytes[exponent_start + 1..])?
    };
    let fraction_len = i64::try_from(fraction_digits.len())
        .map_err(|_| "counter has too many fractional digits")?;
    let scale = exponent.saturating_sub(fraction_len);
    let total_digits = integer_digits.len() + fraction_digits.len();
    let (kept_digits, appended_zeros) = if scale < 0 {
        let removed_digits = usize::try_from(scale.unsigned_abs())
            .map_err(|_| "counter contains a fractional value")?;
        if removed_digits > total_digits {
            return Err("counter contains a fractional value");
        }
        let kept_digits = total_digits - removed_digits;
        if integer_digits
            .iter()
            .chain(fraction_digits)
            .skip(kept_digits)
            .any(|digit| *digit != b'0')
        {
            return Err("counter contains a fractional value");
        }
        (kept_digits, 0_usize)
    } else {
        let appended_zeros = usize::try_from(scale).map_err(|_| "counter exceeds the u64 range")?;
        if appended_zeros > 20 {
            return Err("counter exceeds the u64 range");
        }
        (total_digits, appended_zeros)
    };

    let mut counter = 0_u64;
    for digit in integer_digits
        .iter()
        .chain(fraction_digits)
        .take(kept_digits)
    {
        counter = counter
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or("counter exceeds the u64 range")?;
    }
    for _ in 0..appended_zeros {
        counter = counter
            .checked_mul(10)
            .ok_or("counter exceeds the u64 range")?;
    }
    Ok(counter)
}

fn parse_counter_exponent(raw: &[u8]) -> std::result::Result<i64, &'static str> {
    let (negative, digits) = match raw.first() {
        Some(b'+') => (false, &raw[1..]),
        Some(b'-') => (true, &raw[1..]),
        Some(_) => (false, raw),
        None => return Err("counter exponent is missing"),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err("counter exponent is invalid");
    }
    let exponent = digits.iter().fold(0_i64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(i64::from(*digit - b'0'))
    });
    Ok(if negative {
        exponent.saturating_neg()
    } else {
        exponent
    })
}

#[derive(Debug, Deserialize)]
struct MinioCountSizeWire {
    count: WireCounter,
    #[serde(rename = "bytes")]
    size: WireCounter,
}

impl From<MinioCountSizeWire> for ReplicationCountSize {
    fn from(value: MinioCountSizeWire) -> Self {
        Self {
            count: value.count.0,
            size: value.size.0,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MinioTimedErrStatsWire {
    #[serde(rename = "lastMinute")]
    last_minute: MinioCountSizeWire,
    #[serde(rename = "lastHour")]
    last_hour: MinioCountSizeWire,
    totals: MinioCountSizeWire,
}

impl MinioTimedErrStatsWire {
    fn into_totals(self) -> ReplicationCountSize {
        let Self {
            last_minute,
            last_hour,
            totals,
        } = self;
        let _ = (last_minute, last_hour);
        totals.into()
    }
}

#[derive(Debug, Deserialize)]
struct MinioQueueMetricWire {
    curr: MinioCountSizeWire,
    avg: MinioCountSizeWire,
    #[serde(default)]
    max: WireField<MinioCountSizeWire>,
    #[serde(default)]
    peak: WireField<MinioCountSizeWire>,
}

impl MinioQueueMetricWire {
    fn try_into_metric(self) -> std::result::Result<ReplicationQueueMetric, String> {
        let maximum = match (self.max, self.peak) {
            (WireField::Present(max), WireField::Present(peak)) => {
                let max: ReplicationCountSize = max.into();
                let peak: ReplicationCountSize = peak.into();
                if max != peak {
                    return Err("fields `queued.max` and `queued.peak` conflict".into());
                }
                max
            }
            (WireField::Present(max), WireField::Missing) => max.into(),
            (WireField::Missing, WireField::Present(peak)) => peak.into(),
            (WireField::Missing, WireField::Missing) => {
                return Err(
                    "missing field `queued.peak` or compatibility field `queued.max`".into(),
                );
            }
        };

        Ok(ReplicationQueueMetric {
            curr: self.curr.into(),
            avg: self.avg.into(),
            max: maximum,
            last_minute: ReplicationCountSize::default(),
            extra: BTreeMap::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct MinioTargetMetricWire {
    #[serde(rename = "replicationCount", default)]
    replicated_count: WireField<WireCounter>,
    #[serde(rename = "completedReplicationSize", default)]
    replicated_size: WireField<WireCounter>,
    #[serde(rename = "limitInBits", default)]
    bandwidth_limit: WireField<WireCounter>,
    #[serde(rename = "currentBandwidth", default)]
    current_bandwidth: WireField<f64>,
    #[serde(default)]
    failed: WireField<MinioTimedErrStatsWire>,
    #[serde(rename = "pendingReplicationSize", default)]
    pending_size: WireField<WireCounter>,
    #[serde(rename = "replicaSize", default)]
    replica_size: WireField<WireCounter>,
    #[serde(rename = "failedReplicationSize", default)]
    failed_size: WireField<WireCounter>,
    #[serde(rename = "pendingReplicationCount", default)]
    pending_count: WireField<WireCounter>,
    #[serde(rename = "failedReplicationCount", default)]
    failed_count: WireField<WireCounter>,
    #[serde(flatten, default)]
    extra: BTreeMap<String, Value>,
}

impl MinioTargetMetricWire {
    fn try_into_metric(self) -> std::result::Result<ReplicationTargetMetric, String> {
        let Self {
            replicated_count,
            replicated_size,
            bandwidth_limit,
            current_bandwidth,
            failed,
            pending_size,
            replica_size,
            failed_size,
            pending_count,
            failed_count,
            extra,
        } = self;
        let failed = minio_failed_totals(failed);
        validate_redundant_failed(&failed, &failed_count, &failed_size)?;
        let current_bandwidth = match current_bandwidth {
            WireField::Missing => 0.0,
            WireField::Present(value) if value.is_finite() && value >= 0.0 => value,
            WireField::Present(_) => {
                return Err("field `currentBandwidth` must be finite and non-negative".into());
            }
        };
        let _ = (pending_size, replica_size, pending_count);

        Ok(ReplicationTargetMetric {
            replicated_size: wire_counter_or_zero(replicated_size),
            replicated_count: wire_counter_or_zero(replicated_count),
            failed,
            fail_stats: None,
            latency: ReplicationLatencyMetric::default(),
            xfer_rate_lrg: ReplicationTransferRate::default(),
            xfer_rate_sml: ReplicationTransferRate::default(),
            bandwidth_limit_bytes_per_sec: wire_counter_or_zero(bandwidth_limit),
            current_bandwidth_bytes_per_sec: current_bandwidth,
            latency_scope: Some(ReplicationMetricScope::Unavailable),
            bandwidth_scope: None,
            extra,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ReplicationMetricsWireEnvelope {
    #[serde(rename = "Stats", default)]
    minio_stats: WireField<Option<BTreeMap<String, MinioTargetMetricWire>>>,
    #[serde(rename = "completedReplicationSize", default)]
    minio_replicated_size: WireField<WireCounter>,
    #[serde(rename = "replicaSize", default)]
    minio_replica_size: WireField<WireCounter>,
    #[serde(rename = "replicaCount", default)]
    minio_replica_count: WireField<WireCounter>,
    #[serde(rename = "replicationCount", default)]
    minio_replicated_count: WireField<WireCounter>,
    #[serde(rename = "failed", default)]
    minio_failed: WireField<MinioTimedErrStatsWire>,
    #[serde(rename = "queued", default)]
    minio_queue: WireField<MinioQueueMetricWire>,
    #[serde(rename = "pendingReplicationSize", default)]
    minio_pending_size: WireField<WireCounter>,
    #[serde(rename = "failedReplicationSize", default)]
    minio_failed_size: WireField<WireCounter>,
    #[serde(rename = "pendingReplicationCount", default)]
    minio_pending_count: WireField<WireCounter>,
    #[serde(rename = "failedReplicationCount", default)]
    minio_failed_count: WireField<WireCounter>,
    #[serde(rename = "stats", default)]
    legacy_stats: WireField<BTreeMap<String, ReplicationTargetMetric>>,
    #[serde(rename = "replica_size", default)]
    legacy_replica_size: WireField<u64>,
    #[serde(rename = "replica_count", default)]
    legacy_replica_count: WireField<u64>,
    #[serde(rename = "replicated_size", default)]
    legacy_replicated_size: WireField<u64>,
    #[serde(rename = "replicated_count", default)]
    legacy_replicated_count: WireField<u64>,
    #[serde(rename = "q_stat", default)]
    legacy_queue: WireField<ReplicationQueueMetric>,
    #[serde(default)]
    provider_available: Option<bool>,
    #[serde(default)]
    cluster_complete: Option<bool>,
    #[serde(default)]
    observed_node_count: Option<u32>,
    #[serde(default)]
    expected_node_count: Option<u32>,
    #[serde(default)]
    queue_scope: Option<ReplicationMetricScope>,
    #[serde(flatten, default)]
    extra: BTreeMap<String, Value>,
}

impl ReplicationMetricsWireEnvelope {
    fn has_minio_fields(&self) -> bool {
        self.minio_stats.is_present()
            || self.minio_replicated_size.is_present()
            || self.minio_replica_size.is_present()
            || self.minio_replica_count.is_present()
            || self.minio_replicated_count.is_present()
            || self.minio_failed.is_present()
            || self.minio_queue.is_present()
            || self.minio_pending_size.is_present()
            || self.minio_failed_size.is_present()
            || self.minio_pending_count.is_present()
            || self.minio_failed_count.is_present()
    }

    fn has_legacy_fields(&self) -> bool {
        self.legacy_stats.is_present()
            || self.legacy_replica_size.is_present()
            || self.legacy_replica_count.is_present()
            || self.legacy_replicated_size.is_present()
            || self.legacy_replicated_count.is_present()
            || self.legacy_queue.is_present()
    }

    fn try_into_metrics(self) -> std::result::Result<ReplicationMetrics, String> {
        match (
            self.minio_stats.is_present(),
            self.legacy_stats.is_present(),
        ) {
            (true, true) => Err("replication metrics response mixes `Stats` and `stats`".into()),
            (false, false) => {
                Err("missing replication metrics discriminator `Stats` or `stats`".into())
            }
            (true, false) => {
                if self.has_legacy_fields() {
                    return Err("MinIO replication metrics contain legacy fields".into());
                }
                self.try_into_minio_metrics()
            }
            (false, true) => {
                if self.has_minio_fields() {
                    return Err("legacy replication metrics contain MinIO fields".into());
                }
                self.try_into_legacy_metrics()
            }
        }
    }

    fn try_into_minio_metrics(self) -> std::result::Result<ReplicationMetrics, String> {
        let stats = self
            .minio_stats
            .require("Stats")?
            .unwrap_or_default()
            .into_iter()
            .map(|(arn, target)| target.try_into_metric().map(|target| (arn, target)))
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
        let failed = minio_failed_totals(self.minio_failed);
        validate_redundant_failed(&failed, &self.minio_failed_count, &self.minio_failed_size)?;
        let queue = self.minio_queue.require("queued")?.try_into_metric()?;
        let _ = (self.minio_pending_size, self.minio_pending_count, failed);

        Ok(ReplicationMetrics {
            stats,
            replica_size: wire_counter_or_zero(self.minio_replica_size),
            replica_count: wire_counter_or_zero(self.minio_replica_count),
            replicated_size: wire_counter_or_zero(self.minio_replicated_size),
            replicated_count: wire_counter_or_zero(self.minio_replicated_count),
            q_stat: queue,
            provider_available: self.provider_available,
            cluster_complete: self.cluster_complete,
            observed_node_count: self.observed_node_count,
            expected_node_count: self.expected_node_count,
            queue_scope: self.queue_scope,
            extra: self.extra,
        })
    }

    fn try_into_legacy_metrics(self) -> std::result::Result<ReplicationMetrics, String> {
        Ok(ReplicationMetrics {
            stats: self.legacy_stats.require("stats")?,
            replica_size: self.legacy_replica_size.require("replica_size")?,
            replica_count: self.legacy_replica_count.require("replica_count")?,
            replicated_size: self.legacy_replicated_size.require("replicated_size")?,
            replicated_count: self.legacy_replicated_count.require("replicated_count")?,
            q_stat: self.legacy_queue.require("q_stat")?,
            provider_available: self.provider_available,
            cluster_complete: self.cluster_complete,
            observed_node_count: self.observed_node_count,
            expected_node_count: self.expected_node_count,
            queue_scope: self.queue_scope,
            extra: self.extra,
        })
    }
}

fn wire_counter_or_zero(field: WireField<WireCounter>) -> u64 {
    match field {
        WireField::Missing => 0,
        WireField::Present(value) => value.0,
    }
}

fn minio_failed_totals(field: WireField<MinioTimedErrStatsWire>) -> ReplicationCountSize {
    match field {
        WireField::Missing => ReplicationCountSize::default(),
        WireField::Present(value) => value.into_totals(),
    }
}

fn validate_redundant_failed(
    totals: &ReplicationCountSize,
    count: &WireField<WireCounter>,
    size: &WireField<WireCounter>,
) -> std::result::Result<(), String> {
    if matches!(count, WireField::Present(value) if value.0 != totals.count)
        || matches!(size, WireField::Present(value) if value.0 != totals.size)
    {
        return Err("replication metrics contain inconsistent failed totals".into());
    }
    Ok(())
}

impl<'de> Deserialize<'de> for ReplicationMetrics {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ReplicationMetricsWireEnvelope::deserialize(deserializer)?
            .try_into_metrics()
            .map_err(serde::de::Error::custom)
    }
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

    #[test]
    fn metrics_decode_captured_minio_v1_wire_response() {
        let metrics: ReplicationMetrics = serde_json::from_str(include_str!(
            "../../tests/fixtures/replication_metrics_minio_v1.json"
        ))
        .expect("captured MinIO-compatible metrics");

        let target = metrics
            .stats
            .get("arn:minio:replication:us-east-1:00000000-0000-0000-0000-000000000000:destination")
            .expect("captured target");
        assert_eq!(metrics.replicated_count, 1);
        assert_eq!(metrics.replicated_size, 20);
        assert_eq!(target.replicated_count, 1);
        assert_eq!(target.replicated_size, 20);
        assert_eq!(
            target.latency_scope,
            Some(ReplicationMetricScope::Unavailable)
        );
        assert_eq!(metrics.provider_available, Some(true));
        assert_eq!(metrics.cluster_complete, Some(true));
        assert_eq!(metrics.observed_node_count, Some(1));
        assert_eq!(metrics.expected_node_count, Some(1));
    }

    #[test]
    fn metrics_use_timed_totals_and_preserve_minio_extensions() {
        let metrics: ReplicationMetrics = serde_json::from_str(
            r#"{
                "Stats":{"arn:target":{
                    "replicationCount":2,"completedReplicationSize":30,
                    "limitInBits":8000,"currentBandwidth":125.5,
                    "failed":{
                        "lastMinute":{"count":1.0,"bytes":10.0},
                        "lastHour":{"count":4.0,"bytes":40.0},
                        "totals":{"count":9.0,"bytes":90.0}},
                    "failedReplicationCount":9,"failedReplicationSize":90,
                    "TargetFuture":{"token":"value"}}},
                "completedReplicationSize":30,"replicaSize":4,
                "replicaCount":3,"replicationCount":2,
                "failed":{
                    "lastMinute":{"count":1.0,"bytes":10.0},
                    "lastHour":{"count":4.0,"bytes":40.0},
                    "totals":{"count":9.0,"bytes":90.0}},
                "queued":{
                    "curr":{"count":1.0,"bytes":10.0},
                    "avg":{"count":2.0,"bytes":20.0},
                    "peak":{"count":3.0,"bytes":30.0}},
                "TopFuture":{"revision":7}}
            "#,
        )
        .expect("MinIO metrics");

        let target = &metrics.stats["arn:target"];
        assert_eq!(target.failed.count, 9);
        assert_eq!(target.failed.size, 90);
        assert_eq!(target.bandwidth_limit_bytes_per_sec, 8000);
        assert_eq!(target.current_bandwidth_bytes_per_sec, 125.5);
        assert_eq!(target.extra["TargetFuture"]["token"], "value");
        assert_eq!(metrics.q_stat.max.count, 3);
        assert_eq!(metrics.extra["TopFuture"]["revision"], 7);
    }

    #[test]
    fn metrics_accept_queue_peak_or_max_but_reject_conflicts() {
        fn payload(queue_tail: &str) -> String {
            format!(
                r#"{{"Stats":null,"queued":{{
                    "curr":{{"count":0,"bytes":0}},
                    "avg":{{"count":0,"bytes":0}},
                    {queue_tail}}}}}"#
            )
        }

        for tail in [
            r#""peak":{"count":2,"bytes":20}"#,
            r#""max":{"count":2,"bytes":20}"#,
            r#""max":{"count":2,"bytes":20},"peak":{"count":2,"bytes":20}"#,
        ] {
            let metrics: ReplicationMetrics =
                serde_json::from_str(&payload(tail)).expect("compatible queue peak");
            assert_eq!(metrics.q_stat.max.count, 2);
            assert_eq!(metrics.q_stat.max.size, 20);
        }

        assert!(
            serde_json::from_str::<ReplicationMetrics>(&payload(
                r#""max":{"count":2,"bytes":20},"peak":{"count":3,"bytes":20}"#,
            ))
            .is_err()
        );
    }

    #[test]
    fn metrics_accept_omitempty_and_lossless_numeric_encodings() {
        for (encoded, expected) in [
            ("1e3", 1_000_u64),
            ("1.5e1", 15),
            ("1.2300e2", 123),
            ("0e-400", 0),
            ("1000000000000000.0", 1_000_000_000_000_000),
            ("9007199254740991.0", 9_007_199_254_740_991),
            ("9007199254740992.0", 9_007_199_254_740_992),
            ("9223372036854775808.0", 9_223_372_036_854_775_808),
            ("1e19", 10_000_000_000_000_000_000),
            ("18446744073709551615", u64::MAX),
            ("18446744073709551615.0", u64::MAX),
        ] {
            let payload = format!(
                r#"{{"Stats":{{"arn:target":{{"replicationCount":1e3}}}},
                    "queued":{{"curr":{{"count":0.0,"bytes":0.0}},
                    "avg":{{"count":0,"bytes":0}},
                    "peak":{{"count":{encoded},"bytes":42.0}}}}}}"#
            );
            let metrics: ReplicationMetrics =
                serde_json::from_str(&payload).expect("lossless counter encoding");
            assert_eq!(metrics.replicated_count, 0);
            assert_eq!(metrics.stats["arn:target"].replicated_count, 1000);
            assert_eq!(metrics.stats["arn:target"].failed.count, 0);
            assert_eq!(metrics.q_stat.max.count, expected, "encoded: {encoded}");
        }

        for invalid in [
            "-1",
            "-0.0",
            "1.5",
            "999999999999999.01",
            "1e-400",
            "18446744073709551616.0",
            "1e400",
            "NaN",
            "Infinity",
            "\"1\"",
            "null",
        ] {
            let payload = format!(
                r#"{{"Stats":null,"queued":{{
                    "curr":{{"count":{invalid},"bytes":0}},
                    "avg":{{"count":0,"bytes":0}},
                    "peak":{{"count":0,"bytes":0}}}}}}"#
            );
            assert!(
                serde_json::from_str::<ReplicationMetrics>(&payload).is_err(),
                "accepted invalid counter {invalid}"
            );
        }
    }

    #[test]
    fn metrics_reject_missing_core_mixed_and_inconsistent_minio_fields() {
        for payload in [
            r#"{"queued":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},"peak":{"count":0,"bytes":0}}}"#,
            r#"{"Stats":null}"#,
            r#"{"Stats":null,"stats":{},"queued":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},"peak":{"count":0,"bytes":0}}}"#,
            r#"{"Stats":null,"replica_size":0,"queued":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},"peak":{"count":0,"bytes":0}}}"#,
            r#"{"Stats":null,"queued":{"avg":{"count":0,"bytes":0},"peak":{"count":0,"bytes":0}}}"#,
            r#"{"Stats":null,"queued":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},"peak":{"count":0,"bytes":0}},"failed":{"lastMinute":{"count":0,"bytes":0},"lastHour":{"count":0,"bytes":0}}}"#,
            r#"{"Stats":{"arn:target":{"failed":{"lastMinute":{"count":0,"bytes":0},"lastHour":{"count":0,"bytes":0},"totals":{"count":2,"bytes":20}},"failedReplicationCount":1,"failedReplicationSize":20}},"queued":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},"peak":{"count":0,"bytes":0}}}"#,
        ] {
            assert!(
                serde_json::from_str::<ReplicationMetrics>(payload).is_err(),
                "accepted malformed metrics: {payload}"
            );
        }
    }
}
