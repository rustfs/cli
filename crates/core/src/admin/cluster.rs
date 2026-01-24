//! Cluster management type definitions
//!
//! This module contains data structures for cluster management operations
//! including server information, disk status, and heal operations.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    #[serde(default)]
    pub pool_index: i32,

    /// Set index
    #[serde(default)]
    pub set_index: i32,

    /// Disk index within set
    #[serde(default)]
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

    /// Current bucket being healed
    #[serde(default)]
    pub bucket: String,

    /// Current object being healed
    #[serde(default)]
    pub object: String,

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
        assert_eq!(status.items_scanned, 0);
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
