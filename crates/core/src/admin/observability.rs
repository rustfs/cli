//! Typed contracts for RustFS scanner, storage, and realtime metrics APIs.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

/// Maximum number of snapshots accepted from one metrics request.
pub const MAX_METRICS_SAMPLES: u16 = 120;
/// Maximum encoded size of one NDJSON metrics record.
pub const MAX_METRICS_LINE_BYTES: usize = 1024 * 1024;
/// Maximum encoded size accepted for an entire metrics response.
pub const MAX_METRICS_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// RustFS realtime metrics selector bits from beta.10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricsScope {
    Scanner,
    Disk,
    Os,
    BatchJobs,
    SiteResync,
    Network,
    Memory,
    Cpu,
    Rpc,
    All,
}

impl MetricsScope {
    /// Return the exact `types` bit accepted by RustFS Admin API v3.
    pub const fn bit(self) -> u32 {
        match self {
            Self::Scanner => 1 << 0,
            Self::Disk => 1 << 1,
            Self::Os => 1 << 2,
            Self::BatchJobs => 1 << 3,
            Self::SiteResync => 1 << 4,
            Self::Network => 1 << 5,
            Self::Memory => 1 << 6,
            Self::Cpu => 1 << 7,
            Self::Rpc => 1 << 8,
            Self::All => (1 << 9) - 1,
        }
    }
}

impl fmt::Display for MetricsScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Scanner => "scanner",
            Self::Disk => "disk",
            Self::Os => "os",
            Self::BatchJobs => "batch-jobs",
            Self::SiteResync => "site-resync",
            Self::Network => "network",
            Self::Memory => "memory",
            Self::Cpu => "cpu",
            Self::Rpc => "rpc",
            Self::All => "all",
        };
        formatter.write_str(value)
    }
}

/// Bounded realtime metrics request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsQuery {
    pub scopes: Vec<MetricsScope>,
    pub hosts: Vec<String>,
    pub disks: Vec<String>,
    pub interval: Option<String>,
    pub samples: u16,
    pub by_host: bool,
    pub by_disk: bool,
    pub job_id: Option<String>,
    pub deployment_id: Option<String>,
}

impl Default for MetricsQuery {
    fn default() -> Self {
        Self {
            scopes: vec![MetricsScope::All],
            hosts: Vec::new(),
            disks: Vec::new(),
            interval: None,
            samples: 1,
            by_host: false,
            by_disk: false,
            job_id: None,
            deployment_id: None,
        }
    }
}

impl MetricsQuery {
    /// Combine requested selector bits without allowing duplicate scopes to alter the mask.
    pub fn types_mask(&self) -> u32 {
        if self.scopes.is_empty() || self.scopes.contains(&MetricsScope::All) {
            return MetricsScope::All.bit();
        }
        self.scopes.iter().fold(0, |mask, scope| mask | scope.bit())
    }

    /// Stable label used by human and machine output.
    pub fn scope_label(&self) -> String {
        if self.scopes.is_empty() || self.scopes.contains(&MetricsScope::All) {
            return "all".to_string();
        }
        let mut scopes = self
            .scopes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        scopes.sort();
        scopes.dedup();
        scopes.join(",")
    }
}

/// One scope-specific metrics object kept in its native JSON shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricGroup(pub BTreeMap<String, serde_json::Value>);

/// Scope groups carried by a RustFS realtime metrics snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricGroups {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanner: Option<MetricGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<MetricGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<MetricGroup>,
    #[serde(rename = "batchJobs", default, skip_serializing_if = "Option::is_none")]
    pub batch_jobs: Option<MetricGroup>,
    #[serde(
        rename = "siteResync",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub site_resync: Option<MetricGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<MetricGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<MetricGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<MetricGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc: Option<MetricGroup>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl MetricGroups {
    /// Iterate current and future metric groups in deterministic name order.
    pub fn groups(&self) -> Vec<(&str, &MetricGroup)> {
        let mut groups = Vec::new();
        for (name, group) in [
            ("batch-jobs", self.batch_jobs.as_ref()),
            ("cpu", self.cpu.as_ref()),
            ("disk", self.disk.as_ref()),
            ("memory", self.mem.as_ref()),
            ("network", self.net.as_ref()),
            ("os", self.os.as_ref()),
            ("rpc", self.rpc.as_ref()),
            ("scanner", self.scanner.as_ref()),
            ("site-resync", self.site_resync.as_ref()),
        ] {
            if let Some(group) = group {
                groups.push((name, group));
            }
        }
        groups
    }
}

/// One JSON Lines record from `/rustfs/admin/v3/metrics`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RealtimeMetrics {
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub aggregated: MetricGroups,
    #[serde(rename = "by_host", default)]
    pub by_host: BTreeMap<String, MetricGroups>,
    #[serde(rename = "by_disk", default)]
    pub by_disk: BTreeMap<String, MetricGroup>,
    #[serde(rename = "final", default)]
    pub final_sample: bool,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Fully bounded result of a RustFS metrics query.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsBatch {
    pub snapshots: Vec<RealtimeMetrics>,
    pub encoded_bytes: usize,
}

impl MetricsBatch {
    /// Whether RustFS reported incomplete data or omitted its final marker.
    pub fn is_partial(&self) -> bool {
        self.snapshots
            .iter()
            .any(|snapshot| !snapshot.errors.is_empty())
            || self
                .snapshots
                .last()
                .is_some_and(|snapshot| !snapshot.final_sample)
    }
}

/// Scanner freshness metadata calculated by RustFS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerFreshness {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub last_cycle_end_unix_secs: u64,
    #[serde(default)]
    pub max_expected_age_seconds: u64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Typed operational fields from the scanner report, with future fields retained.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerMetrics {
    #[serde(default)]
    pub collected_at: String,
    #[serde(default)]
    pub current_cycle: u64,
    #[serde(default)]
    pub current_started: String,
    #[serde(default)]
    pub cycles_completed_at: Vec<String>,
    #[serde(default)]
    pub ongoing_buckets: usize,
    #[serde(default)]
    pub active_scan_paths: usize,
    #[serde(default)]
    pub oldest_active_path_age_seconds: u64,
    #[serde(default)]
    pub active_paths: Vec<String>,
    #[serde(default)]
    pub current_scan_mode: String,
    #[serde(default)]
    pub leader_lock_state: String,
    #[serde(default)]
    pub leader_lock_held_by_this_process: bool,
    #[serde(default)]
    pub leader_lock_last_error: String,
    #[serde(default)]
    pub last_cycle_end_unix_secs: u64,
    #[serde(default)]
    pub current_cycle_objects_scanned: u64,
    #[serde(default)]
    pub current_cycle_directories_scanned: u64,
    #[serde(default)]
    pub current_cycle_bucket_drive_failures: u64,
    #[serde(default)]
    pub last_cycle_result: String,
    #[serde(default)]
    pub last_cycle_result_code: u64,
    #[serde(default)]
    pub last_cycle_partial_reason: String,
    #[serde(default)]
    pub last_cycle_partial_source: String,
    #[serde(default)]
    pub last_cycle_duration_seconds: f64,
    #[serde(default)]
    pub last_cycle_objects_scanned: u64,
    #[serde(default)]
    pub last_cycle_directories_scanned: u64,
    #[serde(default)]
    pub last_cycle_bucket_drive_failures: u64,
    #[serde(default)]
    pub failed_cycles: u64,
    #[serde(default)]
    pub partial_cycles: u64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Effective scanner condition presented by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScannerHealth {
    Healthy,
    Stale,
    Empty,
    Partial,
    Disabled,
}

impl fmt::Display for ScannerHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Healthy => "healthy",
            Self::Stale => "stale",
            Self::Empty => "empty",
            Self::Partial => "partial",
            Self::Disabled => "disabled",
        };
        formatter.write_str(value)
    }
}

/// Effective scanner cycle scheduling information.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerCycleSchedule {
    #[serde(default)]
    pub effective_interval_seconds: u64,
    #[serde(default)]
    pub clean_idle_backoff_enabled: bool,
    #[serde(default = "default_backoff_multiplier")]
    pub clean_idle_backoff_multiplier: u64,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

const fn default_backoff_multiplier() -> u64 {
    1
}

/// One scanner runtime setting and the source selected by RustFS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerRuntimeConfigValue<T> {
    pub value: T,
    #[serde(default)]
    pub source: String,
}

/// Typed subset of scanner runtime settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerRuntimeConfig {
    #[serde(default)]
    pub speed: Option<ScannerRuntimeConfigValue<String>>,
    #[serde(default)]
    pub delay: Option<ScannerRuntimeConfigValue<f64>>,
    #[serde(default)]
    pub max_wait_seconds: Option<ScannerRuntimeConfigValue<f64>>,
    #[serde(default)]
    pub idle_mode: Option<ScannerRuntimeConfigValue<bool>>,
    #[serde(default)]
    pub start_delay_seconds: Option<ScannerRuntimeConfigValue<Option<u64>>>,
    #[serde(default)]
    pub cycle_interval_seconds: Option<ScannerRuntimeConfigValue<u64>>,
    #[serde(default)]
    pub bitrot_cycle_seconds: Option<ScannerRuntimeConfigValue<Option<u64>>>,
    #[serde(default)]
    pub cycle_max_duration_seconds: Option<ScannerRuntimeConfigValue<Option<u64>>>,
    #[serde(default)]
    pub cycle_max_objects: Option<ScannerRuntimeConfigValue<Option<u64>>>,
    #[serde(default)]
    pub cycle_max_directories: Option<ScannerRuntimeConfigValue<Option<u64>>>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// RustFS scanner status response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScannerStatus {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub disabled_reason: Option<String>,
    #[serde(default)]
    pub freshness: ScannerFreshness,
    #[serde(default)]
    pub metrics: ScannerMetrics,
    #[serde(default)]
    pub cycle_schedule: ScannerCycleSchedule,
    #[serde(default)]
    pub runtime_config: ScannerRuntimeConfig,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ScannerStatus {
    /// Classify operational state without treating stale or disabled data as transport failure.
    pub fn health(&self) -> ScannerHealth {
        if !self.enabled {
            return ScannerHealth::Disabled;
        }
        if self.freshness.state.eq_ignore_ascii_case("stale") {
            return ScannerHealth::Stale;
        }
        if self
            .metrics
            .last_cycle_result
            .eq_ignore_ascii_case("partial")
            || (!self.metrics.last_cycle_partial_reason.is_empty()
                && !self
                    .metrics
                    .last_cycle_partial_reason
                    .eq_ignore_ascii_case("unknown"))
        {
            return ScannerHealth::Partial;
        }
        if self.metrics.current_cycle == 0
            && self.metrics.last_cycle_end_unix_secs == 0
            && self.freshness.last_cycle_end_unix_secs == 0
        {
            return ScannerHealth::Empty;
        }
        ScannerHealth::Healthy
    }
}

/// Per-operation disk counters from storage information.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageDiskMetrics {
    #[serde(default)]
    pub last_minute: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub api_calls: BTreeMap<String, u64>,
    #[serde(default)]
    pub total_waiting: u32,
    #[serde(default)]
    pub total_errors_availability: u64,
    #[serde(default)]
    pub total_errors_timeout: u64,
    #[serde(default)]
    pub total_writes: u64,
    #[serde(default)]
    pub total_deletes: u64,
}

/// One disk returned by RustFS storage information.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageDisk {
    #[serde(default)]
    pub endpoint: String,
    #[serde(rename = "rootDisk", default)]
    pub root_disk: bool,
    #[serde(rename = "path", default)]
    pub drive_path: String,
    #[serde(default)]
    pub healing: bool,
    #[serde(default)]
    pub scanning: bool,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(rename = "totalspace", default)]
    pub total_space: u64,
    #[serde(rename = "usedspace", default)]
    pub used_space: u64,
    #[serde(rename = "availspace", default)]
    pub available_space: u64,
    #[serde(rename = "readthroughput", default)]
    pub read_throughput: f64,
    #[serde(rename = "writethroughput", default)]
    pub write_throughput: f64,
    #[serde(rename = "readlatency", default)]
    pub read_latency: f64,
    #[serde(rename = "writelatency", default)]
    pub write_latency: f64,
    #[serde(default)]
    pub utilization: f64,
    #[serde(default)]
    pub metrics: Option<StorageDiskMetrics>,
    #[serde(default)]
    pub heal_info: Option<serde_json::Value>,
    #[serde(default)]
    pub used_inodes: u64,
    #[serde(default)]
    pub free_inodes: u64,
    #[serde(default)]
    pub local: bool,
    #[serde(default)]
    pub pool_index: i32,
    #[serde(default)]
    pub set_index: i32,
    #[serde(default)]
    pub disk_index: i32,
    #[serde(rename = "runtimeState", default)]
    pub runtime_state: Option<String>,
    #[serde(rename = "offlineDurationSeconds", default)]
    pub offline_duration_seconds: Option<u64>,
    #[serde(rename = "capacityObservationSource", default)]
    pub capacity_observation_source: Option<String>,
    #[serde(rename = "capacityObservationAgeSeconds", default)]
    pub capacity_observation_age_seconds: Option<u64>,
    #[serde(rename = "physicalDeviceIds", default)]
    pub physical_device_ids: Option<Vec<String>>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// RustFS storage backend kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageBackendKind {
    #[default]
    Unknown,
    #[serde(rename = "FS")]
    Fs,
    Erasure,
    #[serde(other)]
    Other,
}

impl fmt::Display for StorageBackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Unknown => "unknown",
            Self::Fs => "filesystem",
            Self::Erasure => "erasure",
            Self::Other => "other",
        };
        formatter.write_str(value)
    }
}

/// Storage backend topology and erasure coding configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageBackend {
    #[serde(rename = "BackendType", alias = "backend_type", default)]
    pub kind: StorageBackendKind,
    #[serde(rename = "OnlineDisks", alias = "online_disks", default)]
    pub online_disks: BTreeMap<String, usize>,
    #[serde(rename = "OfflineDisks", alias = "offline_disks", default)]
    pub offline_disks: BTreeMap<String, usize>,
    #[serde(rename = "StandardSCData", default)]
    pub standard_sc_data: Vec<usize>,
    #[serde(rename = "StandardSCParities", default)]
    pub standard_sc_parities: Vec<usize>,
    #[serde(rename = "StandardSCParity", default)]
    pub standard_sc_parity: Option<usize>,
    #[serde(rename = "RRSCData", default)]
    pub rr_sc_data: Vec<usize>,
    #[serde(rename = "RRSCParities", default)]
    pub rr_sc_parities: Vec<usize>,
    #[serde(rename = "RRSCParity", default)]
    pub rr_sc_parity: Option<usize>,
    #[serde(rename = "TotalSets", alias = "total_sets", default)]
    pub total_sets: Vec<usize>,
    #[serde(rename = "DrivesPerSet", alias = "drives_per_set", default)]
    pub drives_per_set: Vec<usize>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// RustFS storage information response body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageInfo {
    #[serde(default)]
    pub disks: Vec<StorageDisk>,
    #[serde(default)]
    pub backend: StorageBackend,
}

impl StorageInfo {
    pub fn total_capacity(&self) -> u64 {
        self.disks.iter().map(|disk| disk.total_space).sum()
    }

    pub fn used_capacity(&self) -> u64 {
        self.disks.iter().map(|disk| disk.used_space).sum()
    }

    pub fn online_disks(&self) -> usize {
        self.disks
            .iter()
            .filter(|disk| {
                disk.runtime_state
                    .as_deref()
                    .unwrap_or(&disk.state)
                    .eq_ignore_ascii_case("online")
                    || disk.state.eq_ignore_ascii_case("ok")
            })
            .count()
    }
}

/// Read-only scanner, storage, and metrics Admin API boundary.
#[async_trait]
pub trait ObservabilityApi: Send + Sync {
    async fn scanner_status(&self) -> Result<ScannerStatus>;
    async fn storage_info(&self) -> Result<StorageInfo>;
    async fn realtime_metrics(&self, query: &MetricsQuery) -> Result<MetricsBatch>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_scope_masks_match_rustfs_beta_10() {
        assert_eq!(MetricsScope::Scanner.bit(), 1);
        assert_eq!(MetricsScope::Disk.bit(), 2);
        assert_eq!(MetricsScope::Os.bit(), 4);
        assert_eq!(MetricsScope::BatchJobs.bit(), 8);
        assert_eq!(MetricsScope::SiteResync.bit(), 16);
        assert_eq!(MetricsScope::Network.bit(), 32);
        assert_eq!(MetricsScope::Memory.bit(), 64);
        assert_eq!(MetricsScope::Cpu.bit(), 128);
        assert_eq!(MetricsScope::Rpc.bit(), 256);
        assert_eq!(MetricsScope::All.bit(), 511);
    }

    #[test]
    fn metrics_query_deduplicates_scopes_without_losing_bits() {
        let query = MetricsQuery {
            scopes: vec![
                MetricsScope::Scanner,
                MetricsScope::Disk,
                MetricsScope::Scanner,
            ],
            ..Default::default()
        };

        assert_eq!(query.types_mask(), 3);
    }

    #[test]
    fn scanner_health_distinguishes_healthy_stale_empty_partial_and_disabled() {
        let healthy = scanner_fixture("fresh", "success", 7, true);
        assert_eq!(healthy.health(), ScannerHealth::Healthy);

        let stale = scanner_fixture("stale", "success", 7, true);
        assert_eq!(stale.health(), ScannerHealth::Stale);

        let empty = scanner_fixture("unknown", "unknown", 0, true);
        assert_eq!(empty.health(), ScannerHealth::Empty);

        let partial = scanner_fixture("fresh", "partial", 7, true);
        assert_eq!(partial.health(), ScannerHealth::Partial);

        let disabled = scanner_fixture("unknown", "unknown", 0, false);
        assert_eq!(disabled.health(), ScannerHealth::Disabled);
    }

    #[test]
    fn scanner_status_preserves_unknown_server_fields() {
        let status: ScannerStatus = serde_json::from_str(
            r#"{
                "enabled":true,
                "freshness":{"state":"fresh","last_cycle_end_unix_secs":10,"max_expected_age_seconds":120,"reason":null,"server_hint":"retained"},
                "metrics":{"collected_at":"2026-07-21T04:00:00Z","current_cycle":2,"last_cycle_result":"success","future_counter":9},
                "cycle_schedule":{"effective_interval_seconds":60,"clean_idle_backoff_enabled":false,"clean_idle_backoff_multiplier":1},
                "runtime_config":{"speed":{"value":"fast","source":"default"},"future_setting":{"value":1,"source":"config"}}
            }"#,
        )
        .expect("scanner status should deserialize");

        assert_eq!(status.metrics.extra["future_counter"], 9);
        assert_eq!(status.freshness.extra["server_hint"], "retained");
        assert!(status.runtime_config.extra.contains_key("future_setting"));
    }

    #[test]
    fn realtime_metrics_preserve_numeric_values_labels_and_timestamps() {
        let snapshot: RealtimeMetrics = serde_json::from_str(
            r#"{
                "errors":[],
                "hosts":["node-1"],
                "aggregated":{"net":{"collected":"2026-07-21T04:00:00Z","netstats":{"rx_bytes":42}}},
                "by_host":{"node-1":{"rpc":{"collectedAt":"2026-07-21T04:00:01Z","incomingBytes":7}}},
                "by_disk":{"/data1":{"collected":"2026-07-21T04:00:02Z","n_disks":1}},
                "final":true
            }"#,
        )
        .expect("realtime metrics should deserialize");

        assert_eq!(
            snapshot.aggregated.net.expect("net group").0["netstats"]["rx_bytes"],
            42
        );
        assert_eq!(
            snapshot.by_host["node-1"]
                .rpc
                .as_ref()
                .expect("rpc group")
                .0["collectedAt"],
            "2026-07-21T04:00:01Z"
        );
        assert_eq!(snapshot.by_disk["/data1"].0["n_disks"], 1);
        assert!(snapshot.final_sample);
    }

    #[test]
    fn storage_info_reads_current_rustfs_field_names() {
        let info: StorageInfo = serde_json::from_str(
            r#"{
                "disks":[{
                    "endpoint":"http://node1:9000",
                    "path":"/data1",
                    "state":"online",
                    "totalspace":100,
                    "usedspace":40,
                    "availspace":60,
                    "runtimeState":"online",
                    "pool_index":0,
                    "set_index":1,
                    "disk_index":2
                }],
                "backend":{"BackendType":"Erasure","OnlineDisks":{"set-1":1},"OfflineDisks":{}}
            }"#,
        )
        .expect("storage info should deserialize");

        assert_eq!(info.disks[0].drive_path, "/data1");
        assert_eq!(info.disks[0].used_space, 40);
        assert_eq!(info.backend.kind, StorageBackendKind::Erasure);
        assert_eq!(info.backend.online_disks.values().sum::<usize>(), 1);
    }

    fn scanner_fixture(
        freshness: &str,
        last_result: &str,
        current_cycle: u64,
        enabled: bool,
    ) -> ScannerStatus {
        ScannerStatus {
            enabled,
            disabled_reason: (!enabled).then(|| "disabled by configuration".to_string()),
            freshness: ScannerFreshness {
                state: freshness.to_string(),
                ..Default::default()
            },
            metrics: ScannerMetrics {
                current_cycle,
                last_cycle_result: last_result.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}
