//! Bounded read-only diagnostic snapshot contracts.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{Error, Result};

use super::RuntimeCapabilityStatus;

/// Maximum encoded size accepted for one diagnostic response.
pub const MAX_DIAGNOSTIC_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Detailed authenticated RustFS health snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailedHealthSnapshot {
    pub version: String,
    pub deployment_id: Option<String>,
    pub region: Option<String>,
    pub timestamp: Option<String>,
    pub cpu: HealthCpuSnapshot,
    pub memory: HealthMemorySnapshot,
    pub os: HealthOsSnapshot,
    pub process: HealthProcessSnapshot,
    pub drives: Vec<HealthDriveSnapshot>,
    pub unsupported_probes: Vec<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthCpuSnapshot {
    pub logical_cores: usize,
    pub brand: String,
    pub frequency_mhz: u64,
    pub usage_percent: f64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthMemorySnapshot {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub total_swap_bytes: u64,
    pub used_swap_bytes: u64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthOsSnapshot {
    pub os: String,
    pub kernel_version: Option<String>,
    pub os_version: Option<String>,
    pub hostname: Option<String>,
    pub arch: String,
    pub uptime_secs: u64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthProcessSnapshot {
    pub pid: u32,
    pub cpu_usage_percent: f64,
    pub memory_bytes: u64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthDriveSnapshot {
    pub endpoint: String,
    pub drive_path: String,
    pub state: String,
    pub total_space: u64,
    pub used_space: u64,
    pub available_space: u64,
    pub read_throughput: f64,
    pub write_throughput: f64,
    pub read_latency: f64,
    pub write_latency: f64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

/// Envelope returned by `/v4/cluster/snapshot`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterSnapshotDocument {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub snapshot: Option<DiagnosticClusterSnapshot>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// Full read-only cluster snapshot with unstable subtrees preserved verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticClusterSnapshot {
    pub summary: DiagnosticClusterSummary,
    pub runtime_capabilities_path: String,
    pub extensions_catalog_path: String,
    #[serde(default)]
    pub components: Option<ClusterComponentSnapshots>,
    #[serde(default)]
    pub topology: Value,
    #[serde(default)]
    pub membership: Value,
    #[serde(default)]
    pub pool_state: Value,
    #[serde(default)]
    pub local_storage: Value,
    #[serde(default)]
    pub peer_health: Value,
    #[serde(default)]
    pub rpc_boundary: Value,
    #[serde(default)]
    pub observability: Value,
    #[serde(default)]
    pub workload_admission: Value,
    #[serde(default)]
    pub runtime_status: Value,
    #[serde(default)]
    pub actionable_pressure: bool,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticClusterSummary {
    pub runtime: RuntimeCapabilityStatus,
    pub topology: RuntimeCapabilityStatus,
    pub membership: RuntimeCapabilityStatus,
    #[serde(default)]
    pub storage: Option<RuntimeCapabilityStatus>,
    pub peer_health: RuntimeCapabilityStatus,
    #[serde(default)]
    pub listing: Option<RuntimeCapabilityStatus>,
    #[serde(default)]
    pub usage: Option<RuntimeCapabilityStatus>,
    pub rpc_boundary: RuntimeCapabilityStatus,
    pub observability: RuntimeCapabilityStatus,
    pub workload_admission: RuntimeCapabilityStatus,
    pub actionable_pressure: RuntimeCapabilityStatus,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterComponentSnapshots {
    #[serde(default)]
    pub storage: Option<ClusterComponentSnapshot>,
    #[serde(default)]
    pub peer_health: Option<ClusterComponentSnapshot>,
    #[serde(default)]
    pub listing: Option<ClusterListingSnapshot>,
    #[serde(default)]
    pub usage: Option<ClusterUsageSnapshot>,
    #[serde(default)]
    pub workload_admission: Option<ClusterComponentSnapshot>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterComponentSnapshot {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub condition: String,
    #[serde(default)]
    pub status: Option<RuntimeCapabilityStatus>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterListingSnapshot {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub condition: String,
    #[serde(default)]
    pub status: Option<RuntimeCapabilityStatus>,
    #[serde(default)]
    pub internode_stall_timeouts_total: u64,
    #[serde(default)]
    pub hint: String,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterUsageSnapshot {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub condition: String,
    #[serde(default)]
    pub status: Option<RuntimeCapabilityStatus>,
    #[serde(default)]
    pub dirty_pending_buckets: u64,
    #[serde(default)]
    pub last_dirty_mark_unix_secs: u64,
    #[serde(default)]
    pub last_dirty_clear_unix_secs: u64,
    #[serde(default)]
    pub last_cycle_dirty_buckets: u64,
    #[serde(default)]
    pub last_cycle_cleared_dirty_buckets: u64,
    #[serde(default)]
    pub last_usage_save_unix_secs: u64,
    #[serde(default)]
    pub last_usage_save_result: String,
    #[serde(default)]
    pub last_success_unix_secs: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

/// Default bytes uploaded by each client-devnull request.
pub const DEFAULT_CLIENT_DEVNULL_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum bytes uploaded across all concurrent client-devnull requests.
pub const MAX_CLIENT_DEVNULL_AGGREGATE_BYTES: u64 = 64 * 1024 * 1024;
/// Default number of concurrent client-devnull requests.
pub const DEFAULT_CLIENT_DEVNULL_CONCURRENCY: u8 = 1;
/// Maximum number of concurrent client-devnull requests.
pub const MAX_CLIENT_DEVNULL_CONCURRENCY: u8 = 4;
/// Default active probe timeout.
pub const DEFAULT_CLIENT_DEVNULL_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum active probe timeout.
pub const MAX_CLIENT_DEVNULL_TIMEOUT: Duration = Duration::from_secs(60);

/// Validated limits for one bounded client-to-server devnull probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientDevnullRequest {
    bytes_per_request: u64,
    total_bytes: u64,
    concurrency: u8,
    timeout: Duration,
}

impl ClientDevnullRequest {
    /// Validate probe limits before any server request is made.
    pub fn new(bytes_per_request: u64, concurrency: u8, timeout: Duration) -> Result<Self> {
        if bytes_per_request == 0 {
            return Err(Error::Config(
                "Client devnull size must be greater than zero".to_string(),
            ));
        }
        if !(1..=MAX_CLIENT_DEVNULL_CONCURRENCY).contains(&concurrency) {
            return Err(Error::Config(format!(
                "Client devnull concurrency must be between 1 and {MAX_CLIENT_DEVNULL_CONCURRENCY}"
            )));
        }
        let total_bytes = bytes_per_request
            .checked_mul(u64::from(concurrency))
            .ok_or_else(|| {
                Error::Config("Client devnull aggregate size is too large".to_string())
            })?;
        if total_bytes > MAX_CLIENT_DEVNULL_AGGREGATE_BYTES {
            return Err(Error::Config(format!(
                "Client devnull aggregate size must not exceed {MAX_CLIENT_DEVNULL_AGGREGATE_BYTES} bytes"
            )));
        }
        if timeout < Duration::from_secs(1) || timeout > MAX_CLIENT_DEVNULL_TIMEOUT {
            return Err(Error::Config(format!(
                "Client devnull timeout must be between 1 and {} seconds",
                MAX_CLIENT_DEVNULL_TIMEOUT.as_secs()
            )));
        }

        Ok(Self {
            bytes_per_request,
            total_bytes,
            concurrency,
            timeout,
        })
    }

    /// Bytes sent by each concurrent request.
    pub const fn bytes_per_request(self) -> u64 {
        self.bytes_per_request
    }

    /// Aggregate bytes sent by all requests.
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    /// Number of concurrent requests.
    pub const fn concurrency(self) -> u8 {
        self.concurrency
    }

    /// Maximum duration of the active probe.
    pub const fn timeout(self) -> Duration {
        self.timeout
    }
}

/// Validated aggregate result from a client-to-server devnull probe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientDevnullResult {
    pub requested_bytes: u64,
    pub received_bytes: u64,
    pub concurrency: u8,
    pub elapsed_seconds: f64,
    pub aggregate_throughput_bytes_per_second: f64,
}

/// Read-only diagnostic adapter boundary.
#[async_trait]
pub trait DiagnosticReadApi: Send + Sync {
    async fn health_snapshot(&self) -> Result<DetailedHealthSnapshot>;
    async fn cluster_snapshot(&self) -> Result<ClusterSnapshotDocument>;
    async fn extensions_catalog(&self) -> Result<super::ExtensionsCatalog>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_devnull_defaults_match_the_bounded_probe_contract() {
        assert_eq!(DEFAULT_CLIENT_DEVNULL_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_CLIENT_DEVNULL_AGGREGATE_BYTES, 64 * 1024 * 1024);
        assert_eq!(DEFAULT_CLIENT_DEVNULL_TIMEOUT, Duration::from_secs(30));
        assert_eq!(DEFAULT_CLIENT_DEVNULL_CONCURRENCY, 1);
    }

    #[test]
    fn client_devnull_request_accepts_the_maximum_aggregate_payload() {
        let request = ClientDevnullRequest::new(16 * 1024 * 1024, 4, Duration::from_secs(60))
            .expect("maximum aggregate payload should be valid");

        assert_eq!(request.bytes_per_request(), 16 * 1024 * 1024);
        assert_eq!(request.total_bytes(), 64 * 1024 * 1024);
        assert_eq!(request.concurrency(), 4);
        assert_eq!(request.timeout(), Duration::from_secs(60));
    }

    #[test]
    fn client_devnull_request_rejects_invalid_limits() {
        for (bytes, concurrency, timeout) in [
            (0, 1, Duration::from_secs(30)),
            (8 * 1024 * 1024, 0, Duration::from_secs(30)),
            (8 * 1024 * 1024, 5, Duration::from_secs(30)),
            (32 * 1024 * 1024, 3, Duration::from_secs(30)),
            (8 * 1024 * 1024, 1, Duration::ZERO),
            (8 * 1024 * 1024, 1, Duration::from_millis(999)),
            (8 * 1024 * 1024, 1, Duration::from_secs(61)),
        ] {
            assert!(
                ClientDevnullRequest::new(bytes, concurrency, timeout).is_err(),
                "bytes={bytes}, concurrency={concurrency}, timeout={timeout:?}"
            );
        }
    }
}
