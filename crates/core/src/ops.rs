//! Typed operational APIs for health probes and storage usage.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::Result;

/// RustFS health probe kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthProbe {
    /// Process liveness through the public `/health` endpoint.
    Liveness,
    /// Dependency readiness through the public `/health/ready` endpoint.
    Readiness,
}

impl HealthProbe {
    /// Return the documented RustFS beta.10 endpoint for this probe.
    pub const fn path(self) -> &'static str {
        match self {
            Self::Liveness => "/health",
            Self::Readiness => "/health/ready",
        }
    }

    /// Return the stable machine-readable probe name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Liveness => "liveness",
            Self::Readiness => "readiness",
        }
    }
}

/// Result returned for a completed HTTP health probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    /// Probe kind.
    pub probe: HealthProbe,
    /// Configured service endpoint, without credentials.
    pub endpoint: String,
    /// Probe path used for the request.
    pub path: String,
    /// HTTP status code returned by the service.
    pub status_code: u16,
    /// Whether the endpoint reported the requested health state.
    pub healthy: bool,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// Optional server-provided state such as `ok` or `degraded`.
    pub status: Option<String>,
    /// Optional server-provided service identity.
    pub service: Option<String>,
    /// Optional server-provided version.
    pub server_version: Option<String>,
}

/// Adapter boundary for public health endpoints.
#[async_trait]
pub trait HealthApi: Send + Sync {
    /// Run one health probe with an explicit upper time bound.
    async fn check_health(&self, probe: HealthProbe, timeout: Duration) -> Result<HealthReport>;
}

/// Origin of a usage report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// RustFS background scanner snapshot.
    ServerSnapshot,
    /// Portable client-side S3 listing.
    ClientScan,
}

impl UsageSource {
    /// Return the stable machine-readable source name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServerSnapshot => "server_snapshot",
            Self::ClientScan => "client_scan",
        }
    }
}

/// Requested usage scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    /// All buckets visible to the configured alias.
    Cluster,
    /// A single bucket.
    Bucket,
    /// A prefix within a bucket.
    Prefix,
}

impl UsageScope {
    /// Return the stable machine-readable scope name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cluster => "cluster",
            Self::Bucket => "bucket",
            Self::Prefix => "prefix",
        }
    }
}

/// Per-bucket usage values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// Bucket name.
    pub name: String,
    /// Bytes represented by the selected scan dimensions.
    pub total_bytes: u64,
    /// Current object count.
    pub object_count: u64,
    /// Non-delete object version count, when collected.
    pub version_count: Option<u64>,
    /// Delete marker count, when collected.
    pub delete_marker_count: Option<u64>,
    /// Incomplete multipart upload count, when collected.
    pub incomplete_upload_count: Option<u64>,
    /// Uploaded part bytes belonging to incomplete uploads, when collected.
    pub incomplete_upload_bytes: Option<u64>,
}

/// A bucket that could not be included in a multi-bucket client scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFailure {
    /// Bucket name.
    pub bucket: String,
    /// Sanitizable diagnostic that never contains configured credentials.
    pub message: String,
}

/// Complete or partial usage result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Data source used for the report.
    pub source: UsageSource,
    /// Scope represented by the report.
    pub scope: UsageScope,
    /// Alias-relative path for bucket and prefix scopes.
    pub path: Option<String>,
    /// Server snapshot timestamp; absent for client scans.
    pub snapshot_at: Option<Timestamp>,
    /// Total bytes represented by the report.
    pub total_bytes: u64,
    /// Current object count.
    pub object_count: u64,
    /// Version count, when supplied by the selected source.
    pub version_count: Option<u64>,
    /// Delete marker count, when supplied by the selected source.
    pub delete_marker_count: Option<u64>,
    /// Incomplete upload count, when explicitly scanned.
    pub incomplete_upload_count: Option<u64>,
    /// Incomplete uploaded-part bytes, when explicitly scanned.
    pub incomplete_upload_bytes: Option<u64>,
    /// Deterministically sorted bucket rows.
    pub buckets: Vec<UsageBucket>,
    /// Whether one or more requested buckets could not be scanned.
    pub partial: bool,
    /// Per-bucket failures for partial multi-bucket scans.
    pub failures: Vec<UsageFailure>,
}

impl UsageReport {
    /// Create an empty report that can be populated with bucket rows.
    pub fn empty(source: UsageSource, scope: UsageScope, path: Option<String>) -> Self {
        Self {
            source,
            scope,
            path,
            snapshot_at: None,
            total_bytes: 0,
            object_count: 0,
            version_count: None,
            delete_marker_count: None,
            incomplete_upload_count: None,
            incomplete_upload_bytes: None,
            buckets: Vec::new(),
            partial: false,
            failures: Vec::new(),
        }
    }

    /// Add one bucket row before final aggregation.
    pub fn push_bucket(&mut self, bucket: UsageBucket) {
        self.buckets.push(bucket);
    }

    /// Add one recoverable bucket failure.
    pub fn push_failure(&mut self, failure: UsageFailure) {
        self.partial = true;
        self.failures.push(failure);
    }

    /// Sort rows and recompute report totals using saturating arithmetic.
    pub fn finish(&mut self) {
        let versions_were_requested = self.version_count.is_some();
        let delete_markers_were_requested = self.delete_marker_count.is_some();
        let incomplete_uploads_were_requested = self.incomplete_upload_count.is_some();
        let incomplete_bytes_were_requested = self.incomplete_upload_bytes.is_some();
        self.buckets
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.failures
            .sort_by(|left, right| left.bucket.cmp(&right.bucket));
        self.total_bytes = self.buckets.iter().fold(0_u64, |total, bucket| {
            total.saturating_add(bucket.total_bytes)
        });
        self.object_count = self.buckets.iter().fold(0_u64, |total, bucket| {
            total.saturating_add(bucket.object_count)
        });
        self.version_count = sum_optional(self.buckets.iter().map(|bucket| bucket.version_count))
            .or_else(|| versions_were_requested.then_some(0));
        self.delete_marker_count =
            sum_optional(self.buckets.iter().map(|bucket| bucket.delete_marker_count))
                .or_else(|| delete_markers_were_requested.then_some(0));
        self.incomplete_upload_count = sum_optional(
            self.buckets
                .iter()
                .map(|bucket| bucket.incomplete_upload_count),
        )
        .or_else(|| incomplete_uploads_were_requested.then_some(0));
        self.incomplete_upload_bytes = sum_optional(
            self.buckets
                .iter()
                .map(|bucket| bucket.incomplete_upload_bytes),
        )
        .or_else(|| incomplete_bytes_were_requested.then_some(0));
    }
}

fn sum_optional(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut values = values;
    let first = values.next()??;
    values.try_fold(first, |total, value| Some(total.saturating_add(value?)))
}

/// Options for an explicitly authorized client-side usage scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageScanRequest {
    /// Optional bucket; absent means all visible buckets.
    pub bucket: Option<String>,
    /// Optional prefix within `bucket`.
    pub prefix: Option<String>,
    /// Include all object versions and delete markers.
    pub include_versions: bool,
    /// Include incomplete multipart uploads and uploaded part bytes.
    pub include_incomplete_uploads: bool,
}

impl UsageScanRequest {
    /// Resolve the request scope.
    pub const fn scope(&self) -> UsageScope {
        if self.prefix.is_some() {
            UsageScope::Prefix
        } else if self.bucket.is_some() {
            UsageScope::Bucket
        } else {
            UsageScope::Cluster
        }
    }

    /// Whether the request asks for data unavailable in the cluster snapshot.
    pub const fn requires_client_scan(&self) -> bool {
        self.prefix.is_some() || self.include_incomplete_uploads
    }

    /// Build the canonical alias-relative output path.
    pub fn path(&self, alias: &str) -> Option<String> {
        let bucket = self.bucket.as_deref()?;
        Some(match self.prefix.as_deref() {
            Some(prefix) => format!("{alias}/{bucket}/{prefix}"),
            None => format!("{alias}/{bucket}"),
        })
    }
}

/// Adapter boundary for the RustFS background-scanner snapshot.
#[async_trait]
pub trait UsageSnapshotApi: Send + Sync {
    /// Fetch the cluster-wide server snapshot.
    async fn usage_snapshot(&self) -> Result<UsageReport>;
}

/// Adapter boundary for a portable S3 usage scan.
#[async_trait]
pub trait UsageScanApi: Send + Sync {
    /// Scan usage using paginated S3 APIs.
    async fn scan_usage(&self, request: &UsageScanRequest) -> Result<UsageReport>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_totals_remain_unknown_when_any_bucket_is_unknown() {
        assert_eq!(sum_optional(std::iter::empty()), None);
        assert_eq!(sum_optional([Some(1), None].into_iter()), None);
        assert_eq!(sum_optional([Some(1), Some(2)].into_iter()), Some(3));
    }
}
