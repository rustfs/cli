//! Cluster management type definitions
//!
//! This module contains data structures for cluster management operations
//! including server information, disk status, and heal operations.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Server information representing a RustFS node
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Server state (online, offline, initializing)
    #[serde(default)]
    pub state: String,

    /// Server endpoint URL
    #[serde(default)]
    pub endpoint: String,

    /// Connection scheme (http/https)
    #[serde(default)]
    pub scheme: String,

    /// Uptime in seconds
    #[serde(default)]
    pub uptime: u64,

    /// Server version
    #[serde(default)]
    pub version: String,

    /// Git commit ID
    #[serde(default, rename = "commitID")]
    pub commit_id: String,

    /// Network interfaces
    #[serde(default)]
    pub network: HashMap<String, String>,

    /// Attached drives
    #[serde(default, rename = "drives")]
    pub disks: Vec<DiskInfo>,

    /// Pool number
    #[serde(default, rename = "poolNumber")]
    pub pool_number: i32,

    /// Memory statistics
    #[serde(default, rename = "mem_stats")]
    pub mem_stats: MemStats,
}

/// Disk information
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    /// Disk endpoint
    #[serde(default)]
    pub endpoint: String,

    /// Whether this is a root disk
    #[serde(default, rename = "rootDisk")]
    pub root_disk: bool,

    /// Drive path
    #[serde(default, rename = "path")]
    pub drive_path: String,

    /// Whether healing is in progress
    #[serde(default)]
    pub healing: bool,

    /// Whether scanning is in progress
    #[serde(default)]
    pub scanning: bool,

    /// Disk state (online, offline)
    #[serde(default)]
    pub state: String,

    /// Disk UUID
    #[serde(default)]
    pub uuid: String,

    /// Total space in bytes
    #[serde(default, rename = "totalspace")]
    pub total_space: u64,

    /// Used space in bytes
    #[serde(default, rename = "usedspace")]
    pub used_space: u64,

    /// Available space in bytes
    #[serde(default, rename = "availspace")]
    pub available_space: u64,

    /// Pool index
    #[serde(default, alias = "pool_index")]
    pub pool_index: i32,

    /// Set index
    #[serde(default, alias = "set_index")]
    pub set_index: i32,

    /// Disk index within set
    #[serde(default, alias = "disk_index")]
    pub disk_index: i32,

    /// Healing info if disk is being healed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heal_info: Option<HealingDiskInfo>,
}

/// Healing disk information
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealingDiskInfo {
    /// Heal ID
    #[serde(default)]
    pub id: String,

    /// Heal session ID
    #[serde(default)]
    pub heal_id: String,

    /// Pool index
    #[serde(default)]
    pub pool_index: Option<usize>,

    /// Set index
    #[serde(default)]
    pub set_index: Option<usize>,

    /// Disk index
    #[serde(default)]
    pub disk_index: Option<usize>,

    /// Endpoint being healed
    #[serde(default)]
    pub endpoint: String,

    /// Path being healed
    #[serde(default)]
    pub path: String,

    /// Objects total count
    #[serde(default)]
    pub objects_total_count: u64,

    /// Objects total size
    #[serde(default)]
    pub objects_total_size: u64,

    /// Items healed count
    #[serde(default)]
    pub items_healed: u64,

    /// Items failed count
    #[serde(default)]
    pub items_failed: u64,

    /// Bytes done
    #[serde(default)]
    pub bytes_done: u64,

    /// Whether healing is finished
    #[serde(default)]
    pub finished: bool,

    /// Current bucket being healed
    #[serde(default)]
    pub bucket: String,

    /// Current object being healed
    #[serde(default)]
    pub object: String,
}

/// Memory statistics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemStats {
    /// Current allocated memory
    #[serde(default)]
    pub alloc: u64,

    /// Total allocated memory over lifetime
    #[serde(default)]
    pub total_alloc: u64,

    /// Heap allocated memory
    #[serde(default)]
    pub heap_alloc: u64,
}

/// Storage backend type
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendType {
    /// Filesystem backend (single drive)
    #[default]
    #[serde(rename = "FS")]
    Fs,
    /// Erasure coding backend (distributed)
    #[serde(rename = "Erasure")]
    Erasure,
}

impl std::fmt::Display for BackendType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendType::Fs => write!(f, "FS"),
            BackendType::Erasure => write!(f, "Erasure"),
        }
    }
}

/// Backend information
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackendInfo {
    /// Backend type
    #[serde(default, rename = "backendType")]
    pub backend_type: BackendType,

    /// Number of online disks
    #[serde(default, rename = "onlineDisks")]
    pub online_disks: usize,

    /// Number of offline disks
    #[serde(default, rename = "offlineDisks")]
    pub offline_disks: usize,

    /// Standard storage class parity
    #[serde(default, rename = "standardSCParity")]
    pub standard_sc_parity: Option<usize>,

    /// Reduced redundancy storage class parity
    #[serde(default, rename = "rrSCParity")]
    pub rr_sc_parity: Option<usize>,

    /// Total erasure sets
    #[serde(default, rename = "totalSets")]
    pub total_sets: Vec<usize>,

    /// Drives per erasure set
    #[serde(default, rename = "totalDrivesPerSet")]
    pub drives_per_set: Vec<usize>,
}

/// Cluster usage statistics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageInfo {
    /// Total storage size in bytes
    #[serde(default)]
    pub size: u64,

    /// Error message if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Bucket count information
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BucketsInfo {
    /// Number of buckets
    #[serde(default)]
    pub count: u64,

    /// Error message if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Object count information
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectsInfo {
    /// Number of objects
    #[serde(default)]
    pub count: u64,

    /// Error message if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Pool erasure set metrics returned by cluster information.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PoolErasureSetInfo {
    /// Erasure set ID within the pool.
    #[serde(default)]
    pub id: i32,

    /// Raw used capacity in bytes.
    #[serde(default, rename = "rawUsage")]
    pub raw_usage: u64,

    /// Raw total capacity in bytes.
    #[serde(default, rename = "rawCapacity")]
    pub raw_capacity: u64,

    /// Object data usage in bytes.
    #[serde(default)]
    pub usage: u64,

    /// Number of objects in the set.
    #[serde(default, rename = "objectsCount")]
    pub objects_count: u64,

    /// Number of versions in the set.
    #[serde(default, rename = "versionsCount")]
    pub versions_count: u64,

    /// Number of delete markers in the set.
    #[serde(default, rename = "deleteMarkersCount")]
    pub delete_markers_count: u64,

    /// Number of healing disks in the set.
    #[serde(default, rename = "healDisks")]
    pub heal_disks: i32,
}

/// Complete cluster information response
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClusterInfo {
    /// Deployment mode (distributed, standalone)
    #[serde(default)]
    pub mode: Option<String>,

    /// Domain names
    #[serde(default)]
    pub domain: Option<Vec<String>>,

    /// Region
    #[serde(default)]
    pub region: Option<String>,

    /// Deployment ID
    #[serde(default, rename = "deploymentID")]
    pub deployment_id: Option<String>,

    /// Bucket information
    #[serde(default)]
    pub buckets: Option<BucketsInfo>,

    /// Object information
    #[serde(default)]
    pub objects: Option<ObjectsInfo>,

    /// Storage usage
    #[serde(default)]
    pub usage: Option<UsageInfo>,

    /// Backend information
    #[serde(default)]
    pub backend: Option<BackendInfo>,

    /// Server information
    #[serde(default)]
    pub servers: Option<Vec<ServerInfo>>,

    /// Pool metrics keyed by pool and erasure set index.
    #[serde(default)]
    pub pools: Option<BTreeMap<i32, BTreeMap<i32, PoolErasureSetInfo>>>,
}

impl ClusterInfo {
    /// Get the total number of online disks across all servers
    pub fn online_disks(&self) -> usize {
        self.servers
            .as_ref()
            .map(|servers| {
                servers
                    .iter()
                    .flat_map(|s| &s.disks)
                    .filter(|d| d.state == "online" || d.state == "ok")
                    .count()
            })
            .unwrap_or(0)
    }

    /// Get the total number of offline disks across all servers
    pub fn offline_disks(&self) -> usize {
        self.servers
            .as_ref()
            .map(|servers| {
                servers
                    .iter()
                    .flat_map(|s| &s.disks)
                    .filter(|d| d.state == "offline")
                    .count()
            })
            .unwrap_or(0)
    }

    /// Get total storage capacity in bytes
    pub fn total_capacity(&self) -> u64 {
        self.servers
            .as_ref()
            .map(|servers| {
                servers
                    .iter()
                    .flat_map(|s| &s.disks)
                    .map(|d| d.total_space)
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Get used storage in bytes
    pub fn used_capacity(&self) -> u64 {
        self.servers
            .as_ref()
            .map(|servers| {
                servers
                    .iter()
                    .flat_map(|s| &s.disks)
                    .map(|d| d.used_space)
                    .sum()
            })
            .unwrap_or(0)
    }
}

/// Heal operation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HealScanMode {
    /// Normal scan (default)
    #[default]
    Normal,
    /// Deep scan (slower but more thorough)
    Deep,
}

impl std::fmt::Display for HealScanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealScanMode::Normal => write!(f, "normal"),
            HealScanMode::Deep => write!(f, "deep"),
        }
    }
}

impl std::str::FromStr for HealScanMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "normal" => Ok(HealScanMode::Normal),
            "deep" => Ok(HealScanMode::Deep),
            _ => Err(format!("Invalid heal scan mode: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealRuntimeState {
    Disabled,
    Uninitialized,
    Idle,
    Active,
    #[serde(other)]
    Unknown,
}

/// Request to start a heal operation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealStartRequest {
    /// Bucket to heal (empty for all buckets)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,

    /// Object prefix to heal
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// Scan mode
    #[serde(default)]
    pub scan_mode: HealScanMode,

    /// Whether to remove dangling objects
    #[serde(default)]
    pub remove: bool,

    /// Whether to recreate missing data
    #[serde(default)]
    pub recreate: bool,

    /// Dry run mode (don't actually heal)
    #[serde(default)]
    pub dry_run: bool,
}

/// Request to inspect or stop a token-scoped heal task
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealTaskRequest {
    /// Bucket being healed
    pub bucket: String,

    /// Object prefix being healed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// Client token returned by the heal start request
    pub client_token: String,
}

/// Information about a single heal drive
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealDriveInfo {
    /// Drive UUID
    #[serde(default)]
    pub uuid: String,

    /// Drive endpoint
    #[serde(default)]
    pub endpoint: String,

    /// Drive state
    #[serde(default)]
    pub state: String,
}

/// Result of a heal operation on a single item
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealResultItem {
    /// Result index
    #[serde(default, rename = "resultId")]
    pub result_index: usize,

    /// Type of item healed (bucket, object, metadata)
    #[serde(default, rename = "type")]
    pub item_type: String,

    /// Bucket name
    #[serde(default)]
    pub bucket: String,

    /// Object key
    #[serde(default)]
    pub object: String,

    /// Version ID
    #[serde(default, rename = "versionId")]
    pub version_id: String,

    /// Detail message
    #[serde(default)]
    pub detail: String,

    /// Number of parity blocks
    #[serde(default, rename = "parityBlocks")]
    pub parity_blocks: usize,

    /// Number of data blocks
    #[serde(default, rename = "dataBlocks")]
    pub data_blocks: usize,

    /// Object size
    #[serde(default, rename = "objectSize")]
    pub object_size: u64,

    /// Drive info before healing
    #[serde(default)]
    pub before: HealDriveInfos,

    /// Drive info after healing
    #[serde(default)]
    pub after: HealDriveInfos,
}

/// Collection of heal drive infos
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealDriveInfos {
    /// Drive information
    #[serde(default)]
    pub drives: Vec<HealDriveInfo>,
}

/// Status of a heal operation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HealStatus {
    /// Heal ID
    #[serde(default)]
    pub heal_id: String,

    /// Whether healing is in progress
    #[serde(default)]
    pub healing: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<HealRuntimeState>,

    /// Task summary for token-scoped manual heal status
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    /// Task detail for token-scoped manual heal status
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    /// Current bucket being healed
    #[serde(default)]
    pub bucket: String,

    /// Current object being healed
    #[serde(default)]
    pub object: String,

    /// Current scan mode reported by background healing
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_mode: Option<HealScanMode>,

    /// Background heal scan cycle
    #[serde(default)]
    pub scan_cycle: u64,

    /// Number of queued heal tasks
    #[serde(default)]
    pub heal_queue_length: u64,

    /// Number of active heal tasks
    #[serde(default)]
    pub heal_active_tasks: u64,

    /// Number of items scanned
    #[serde(default)]
    pub items_scanned: u64,

    /// Number of items healed
    #[serde(default)]
    pub items_healed: u64,

    /// Number of items failed
    #[serde(default)]
    pub items_failed: u64,

    /// Bytes scanned
    #[serde(default)]
    pub bytes_scanned: u64,

    /// Bytes healed
    #[serde(default)]
    pub bytes_healed: u64,

    /// Start time
    #[serde(default)]
    pub started: Option<String>,

    /// Last update time
    #[serde(default)]
    pub last_update: Option<String>,
}

/// Request targeting a storage pool by command line or numeric ID.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PoolTarget {
    /// Pool command line, or zero-based pool ID when `by_id` is true.
    pub pool: String,

    /// Interpret `pool` as a zero-based pool ID.
    #[serde(default)]
    pub by_id: bool,
}

/// Status of a server pool.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PoolStatus {
    /// Zero-based pool ID.
    #[serde(default)]
    pub id: usize,

    /// Pool command line used by the server process.
    #[serde(default, rename = "cmdline")]
    pub cmd_line: String,

    /// Last pool metadata update timestamp.
    #[serde(default, rename = "lastUpdate")]
    pub last_update: String,

    /// Pool lifecycle status.
    #[serde(default)]
    pub status: String,

    /// Decommission operation status for this pool.
    #[serde(default, rename = "decommissionStatus")]
    pub decommission_status: String,

    /// Rebalance operation status for this pool.
    #[serde(default, rename = "rebalanceStatus")]
    pub rebalance_status: String,

    /// Total pool size in bytes.
    #[serde(default, rename = "totalSize")]
    pub total_size: u64,

    /// Current free size in bytes.
    #[serde(default, rename = "currentSize")]
    pub current_size: u64,

    /// Used pool size in bytes.
    #[serde(default, rename = "usedSize")]
    pub used_size: u64,

    /// Used capacity ratio in the range 0.0..=1.0.
    #[serde(default)]
    pub used: f64,

    /// Decommission status and progress for this pool.
    #[serde(default, rename = "decommissionInfo")]
    pub decommission: Option<PoolDecommissionInfo>,
}

/// Decommission status response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DecommissionStatus {
    /// Per-pool decommission status.
    #[serde(default)]
    pub pools: Vec<DecommissionPoolStatus>,
}

/// Decommission operation status for a single pool.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DecommissionPoolStatus {
    /// Zero-based pool ID.
    #[serde(default)]
    pub id: usize,

    /// Pool command line used by the server process.
    #[serde(default, rename = "cmdline")]
    pub cmd_line: String,

    /// Decommission operation status for this pool.
    #[serde(default)]
    pub status: String,

    /// Pool lifecycle status.
    #[serde(default, rename = "poolStatus")]
    pub pool_status: String,

    /// Decommission state and progress for this pool.
    #[serde(default, rename = "decommissionInfo")]
    pub decommission: Option<PoolDecommissionInfo>,
}

/// Decommission state and progress for a server pool.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PoolDecommissionInfo {
    /// Decommission start timestamp.
    #[serde(default, rename = "startTime")]
    pub start_time: Option<String>,

    /// Free bytes when decommission started.
    #[serde(default, rename = "startSize")]
    pub start_size: u64,

    /// Total pool size in bytes.
    #[serde(default, rename = "totalSize")]
    pub total_size: u64,

    /// Current free size in bytes.
    #[serde(default, rename = "currentSize")]
    pub current_size: u64,

    /// Whether decommission completed.
    #[serde(default)]
    pub complete: bool,

    /// Whether decommission failed.
    #[serde(default)]
    pub failed: bool,

    /// Whether decommission was canceled.
    #[serde(default)]
    pub canceled: bool,

    /// Whether decommission is queued.
    #[serde(default)]
    pub queued: bool,

    /// Buckets waiting to be decommissioned.
    #[serde(default, rename = "queuedBuckets")]
    pub queued_buckets: Vec<String>,

    /// Buckets already decommissioned.
    #[serde(default, rename = "decommissionedBuckets")]
    pub decommissioned_buckets: Vec<String>,

    /// Current bucket.
    #[serde(default)]
    pub bucket: String,

    /// Current prefix.
    #[serde(default)]
    pub prefix: String,

    /// Current object.
    #[serde(default)]
    pub object: String,

    /// Current decommission stage.
    #[serde(default)]
    pub stage: String,

    /// Number of successfully decommissioned objects.
    #[serde(default, rename = "objectsDecommissioned")]
    pub objects_decommissioned: u64,

    /// Number of objects that failed to decommission.
    #[serde(default, rename = "objectsDecommissionedFailed")]
    pub objects_decommissioned_failed: u64,

    /// Bytes successfully moved off the pool.
    #[serde(default, rename = "bytesDecommissioned")]
    pub bytes_decommissioned: u64,

    /// Bytes that failed to move off the pool.
    #[serde(default, rename = "bytesDecommissionedFailed")]
    pub bytes_decommissioned_failed: u64,

    /// Reason why decommission is waiting.
    #[serde(default, rename = "waitingReason")]
    pub waiting_reason: Option<String>,
}

/// Response from starting a rebalance operation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RebalanceStartResult {
    /// Rebalance operation ID.
    #[serde(default)]
    pub id: String,
}

/// Cluster-wide rebalance status.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RebalanceStatus {
    /// Rebalance operation ID.
    #[serde(default)]
    pub id: String,

    /// Per-pool rebalance status.
    #[serde(default)]
    pub pools: Vec<RebalancePoolStatus>,

    /// Timestamp when rebalance was stopped.
    #[serde(default, rename = "stoppedAt")]
    pub stopped_at: Option<String>,
}

/// Rebalance status for a single pool.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RebalancePoolStatus {
    /// Zero-based pool ID.
    #[serde(default)]
    pub id: usize,

    /// Rebalance status for this pool.
    #[serde(default)]
    pub status: String,

    /// Used capacity ratio in the range 0.0..=1.0.
    #[serde(default)]
    pub used: f64,

    /// Last rebalance error, if any.
    #[serde(default, rename = "lastError")]
    pub last_error: Option<String>,

    /// Cleanup warnings observed after this pool finishes rebalance.
    #[serde(default, rename = "cleanupWarnings")]
    pub cleanup_warnings: RebalanceCleanupWarnings,

    /// Rebalance progress, if this pool is active.
    #[serde(default)]
    pub progress: Option<RebalancePoolProgress>,
}

/// Cleanup warnings recorded for a rebalanced pool.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RebalanceCleanupWarnings {
    /// Number of cleanup warnings observed.
    #[serde(default)]
    pub count: u64,

    /// Last cleanup warning message.
    #[serde(default, rename = "lastMsg")]
    pub last_message: Option<String>,

    /// Bucket associated with the last cleanup warning.
    #[serde(default, rename = "lastBucket")]
    pub last_bucket: Option<String>,

    /// Object associated with the last cleanup warning.
    #[serde(default, rename = "lastObject")]
    pub last_object: Option<String>,

    /// Timestamp of the last cleanup warning.
    #[serde(default, rename = "lastAt")]
    pub last_at: Option<String>,
}

/// Rebalance progress for a single pool.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RebalancePoolProgress {
    /// Number of objects moved.
    #[serde(default, rename = "objects")]
    pub num_objects: u64,

    /// Number of object versions moved.
    #[serde(default, rename = "versions")]
    pub num_versions: u64,

    /// Number of bytes moved.
    #[serde(default)]
    pub bytes: u64,

    /// Number of buckets remaining.
    #[serde(default, rename = "remainingBuckets")]
    pub remaining_buckets: usize,

    /// Current bucket.
    #[serde(default)]
    pub bucket: String,

    /// Current object.
    #[serde(default)]
    pub object: String,

    /// Elapsed seconds.
    #[serde(default)]
    pub elapsed: u64,

    /// Estimated seconds remaining.
    #[serde(default)]
    pub eta: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_type_display() {
        assert_eq!(BackendType::Fs.to_string(), "FS");
        assert_eq!(BackendType::Erasure.to_string(), "Erasure");
    }

    #[test]
    fn test_heal_scan_mode_display() {
        assert_eq!(HealScanMode::Normal.to_string(), "normal");
        assert_eq!(HealScanMode::Deep.to_string(), "deep");
    }

    #[test]
    fn test_heal_scan_mode_from_str() {
        assert_eq!(
            "normal".parse::<HealScanMode>().unwrap(),
            HealScanMode::Normal
        );
        assert_eq!("deep".parse::<HealScanMode>().unwrap(), HealScanMode::Deep);
        assert!("invalid".parse::<HealScanMode>().is_err());
    }

    #[test]
    fn test_cluster_info_default() {
        let info = ClusterInfo::default();
        assert!(info.mode.is_none());
        assert!(info.servers.is_none());
        assert_eq!(info.online_disks(), 0);
        assert_eq!(info.offline_disks(), 0);
    }

    #[test]
    fn test_cluster_info_disk_counts() {
        let info = ClusterInfo {
            servers: Some(vec![ServerInfo {
                disks: vec![
                    DiskInfo {
                        state: "online".to_string(),
                        ..Default::default()
                    },
                    DiskInfo {
                        state: "online".to_string(),
                        ..Default::default()
                    },
                    DiskInfo {
                        state: "offline".to_string(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(info.online_disks(), 2);
        assert_eq!(info.offline_disks(), 1);
    }

    #[test]
    fn test_cluster_info_capacity() {
        let info = ClusterInfo {
            servers: Some(vec![ServerInfo {
                disks: vec![
                    DiskInfo {
                        total_space: 1000,
                        used_space: 300,
                        ..Default::default()
                    },
                    DiskInfo {
                        total_space: 2000,
                        used_space: 500,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert_eq!(info.total_capacity(), 3000);
        assert_eq!(info.used_capacity(), 800);
    }

    #[test]
    fn test_disk_info_default() {
        let disk = DiskInfo::default();
        assert!(disk.endpoint.is_empty());
        assert!(!disk.healing);
        assert!(!disk.scanning);
        assert_eq!(disk.total_space, 0);
    }

    #[test]
    fn test_disk_info_deserializes_snake_case_location_indexes() {
        let json = r#"{"pool_index":1,"set_index":2,"disk_index":3}"#;

        let disk: DiskInfo = serde_json::from_str(json).unwrap();

        assert_eq!(disk.pool_index, 1);
        assert_eq!(disk.set_index, 2);
        assert_eq!(disk.disk_index, 3);
    }

    #[test]
    fn test_server_info_default() {
        let server = ServerInfo::default();
        assert!(server.state.is_empty());
        assert!(server.endpoint.is_empty());
        assert_eq!(server.uptime, 0);
    }

    #[test]
    fn test_heal_start_request_default() {
        let req = HealStartRequest::default();
        assert!(req.bucket.is_none());
        assert!(req.prefix.is_none());
        assert_eq!(req.scan_mode, HealScanMode::Normal);
        assert!(!req.remove);
        assert!(!req.dry_run);
    }

    #[test]
    fn test_heal_status_default() {
        let status = HealStatus::default();
        assert!(status.heal_id.is_empty());
        assert!(!status.healing);
        assert!(status.state.is_none());
        assert!(status.scan_mode.is_none());
        assert_eq!(status.scan_cycle, 0);
        assert_eq!(status.heal_queue_length, 0);
        assert_eq!(status.heal_active_tasks, 0);
        assert_eq!(status.items_scanned, 0);
    }

    #[test]
    fn test_heal_runtime_state_unknown_value_is_preserved() {
        let state: HealRuntimeState = serde_json::from_str(r#""future""#).unwrap();

        assert_eq!(state, HealRuntimeState::Unknown);
    }

    #[test]
    fn test_pool_status_deserialization() {
        let json = r#"{"id":1,"cmdline":"/data/pool1/disk{1...4}","lastUpdate":"2026-05-06T00:00:00Z","status":"decommissioning","decommissionStatus":"running","rebalanceStatus":"none","totalSize":1000,"currentSize":600,"usedSize":400,"used":0.4,"decommissionInfo":{"startTime":"2026-05-06T00:00:01Z","startSize":100,"totalSize":1000,"currentSize":600,"complete":false,"failed":false,"canceled":false,"queued":true,"queuedBuckets":["bucket-a"],"decommissionedBuckets":["bucket-b"],"bucket":"bucket-a","prefix":"","object":"object.txt","stage":"migrate_object","objectsDecommissioned":2,"objectsDecommissionedFailed":1,"bytesDecommissioned":128,"bytesDecommissionedFailed":64,"waitingReason":"queued"}}"#;

        let status: PoolStatus = serde_json::from_str(json).unwrap();

        assert_eq!(status.id, 1);
        assert_eq!(status.cmd_line, "/data/pool1/disk{1...4}");
        assert_eq!(status.status, "decommissioning");
        assert_eq!(status.decommission_status, "running");
        assert_eq!(status.rebalance_status, "none");
        assert_eq!(status.used_size, 400);
        let info = status.decommission.expect("decommission info exists");
        assert!(info.queued);
        assert_eq!(info.queued_buckets, vec!["bucket-a"]);
        assert_eq!(info.bucket, "bucket-a");
        assert_eq!(info.object, "object.txt");
        assert_eq!(info.waiting_reason.as_deref(), Some("queued"));
        assert_eq!(info.objects_decommissioned, 2);
        assert_eq!(info.bytes_decommissioned_failed, 64);
    }

    #[test]
    fn test_decommission_status_deserialization() {
        let json = r#"{"pools":[{"id":2,"cmdline":"/data/pool2/disk{1...4}","status":"failed","poolStatus":"blocked","decommissionInfo":{"failed":true,"totalSize":1000,"currentSize":900}}]}"#;

        let status: DecommissionStatus = serde_json::from_str(json).unwrap();

        assert_eq!(status.pools.len(), 1);
        assert_eq!(status.pools[0].id, 2);
        assert_eq!(status.pools[0].status, "failed");
        assert_eq!(status.pools[0].pool_status, "blocked");
        assert!(
            status.pools[0]
                .decommission
                .as_ref()
                .is_some_and(|info| info.failed)
        );
    }

    #[test]
    fn test_rebalance_status_deserialization() {
        let json = r#"{"id":"rebalance-1","pools":[{"id":0,"status":"Started","used":0.5,"lastError":null,"cleanupWarnings":{"count":1,"lastMsg":"cleanup warning","lastBucket":"bucket","lastObject":"object","lastAt":"2026-06-12T00:00:00Z"},"progress":{"objects":3,"versions":4,"bytes":1024,"remainingBuckets":2,"bucket":"bucket","object":"object","elapsed":10,"eta":20}}],"stoppedAt":null}"#;

        let status: RebalanceStatus = serde_json::from_str(json).unwrap();

        assert_eq!(status.id, "rebalance-1");
        assert_eq!(status.pools.len(), 1);
        assert_eq!(status.pools[0].used, 0.5);
        assert_eq!(status.pools[0].cleanup_warnings.count, 1);
        assert_eq!(
            status.pools[0].cleanup_warnings.last_message.as_deref(),
            Some("cleanup warning")
        );
        let progress = status.pools[0]
            .progress
            .as_ref()
            .expect("progress should exist");
        assert_eq!(progress.num_objects, 3);
        assert_eq!(progress.remaining_buckets, 2);
    }

    #[test]
    fn test_rebalance_status_defaults_cleanup_warnings() {
        let json = r#"{"id":"rebalance-1","pools":[{"id":0,"status":"Completed","used":0.5,"lastError":null,"progress":null}],"stoppedAt":null}"#;

        let status: RebalanceStatus = serde_json::from_str(json).unwrap();

        assert_eq!(status.pools[0].cleanup_warnings.count, 0);
        assert_eq!(status.pools[0].cleanup_warnings.last_message, None);
    }

    #[test]
    fn test_serialization() {
        let info = ClusterInfo {
            mode: Some("distributed".to_string()),
            deployment_id: Some("test-123".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("distributed"));
        assert!(json.contains("test-123"));

        let deserialized: ClusterInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.mode, Some("distributed".to_string()));
    }
}
