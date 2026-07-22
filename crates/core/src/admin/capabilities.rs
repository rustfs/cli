//! Runtime capability discovery contracts for the RustFS Admin API.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Effective availability of an Admin API capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityAvailability {
    Available,
    Stubbed,
    Unsupported,
    Disabled,
    VersionGated,
    PermissionDenied,
    Unknown,
}

impl fmt::Display for CapabilityAvailability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Available => "available",
            Self::Stubbed => "stubbed",
            Self::Unsupported => "unsupported",
            Self::Disabled => "disabled",
            Self::VersionGated => "version-gated",
            Self::PermissionDenied => "permission-denied",
            Self::Unknown => "unknown",
        };
        formatter.write_str(value)
    }
}

/// RustFS capability state returned by runtime snapshot endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCapabilityState {
    Supported,
    Unsupported,
    Disabled,
    Unknown,
}

/// A typed status from a RustFS runtime snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilityStatus {
    pub state: RuntimeCapabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RuntimeCapabilityStatus {
    /// Convert the server contract into an effective client availability.
    pub const fn availability(&self) -> CapabilityAvailability {
        match self.state {
            RuntimeCapabilityState::Supported => CapabilityAvailability::Available,
            RuntimeCapabilityState::Unsupported => CapabilityAvailability::Unsupported,
            RuntimeCapabilityState::Disabled => CapabilityAvailability::Disabled,
            RuntimeCapabilityState::Unknown => CapabilityAvailability::Unknown,
        }
    }
}

/// Summary fields provided by `/v4/runtime/capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilitiesSummary {
    pub observability: RuntimeCapabilityStatus,
    pub userspace_profiling: RuntimeCapabilityStatus,
    pub memory_sampling: RuntimeCapabilityStatus,
    pub platform: RuntimeCapabilityStatus,
    pub topology: RuntimeCapabilityStatus,
    pub cluster_snapshot: RuntimeCapabilityStatus,
}

/// Typed subset of the RustFS runtime capability response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapabilitiesSnapshot {
    pub summary: RuntimeCapabilitiesSummary,
    pub cluster_snapshot_path: String,
    pub cluster_snapshot_summary: Option<RuntimeCapabilityStatus>,
    pub topology_status: RuntimeCapabilityStatus,
    #[serde(default)]
    pub observability: serde_json::Value,
    #[serde(default)]
    pub workload_admission: serde_json::Value,
    #[serde(default)]
    pub topology: Option<serde_json::Value>,
}

/// Extension metadata advertised by RustFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionMetadata {
    pub schema_version: String,
    pub extension_id: String,
    pub display_name: String,
    pub provider: String,
    pub version: String,
    pub kind: String,
    pub disabled_by_default: bool,
}

/// Typed subset of `/v4/extensions/catalog`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionsCatalog {
    pub extensions: Vec<ExtensionMetadata>,
    #[serde(default)]
    pub runtime_capabilities: serde_json::Value,
    #[serde(default)]
    pub cluster_snapshot: serde_json::Value,
    #[serde(default)]
    pub external_plugin_flow: serde_json::Value,
}

/// Typed summary returned inside a cluster snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSnapshotSummary {
    pub runtime: RuntimeCapabilityStatus,
    pub topology: RuntimeCapabilityStatus,
    pub membership: RuntimeCapabilityStatus,
    pub peer_health: RuntimeCapabilityStatus,
    pub rpc_boundary: RuntimeCapabilityStatus,
    pub observability: RuntimeCapabilityStatus,
    pub workload_admission: RuntimeCapabilityStatus,
    pub actionable_pressure: RuntimeCapabilityStatus,
}

/// Metadata from `/v4/cluster/snapshot` without exposing server internals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterSnapshotMetadata {
    pub summary: Option<ClusterSnapshotSummary>,
    pub runtime_capabilities_path: Option<String>,
    pub extensions_catalog_path: Option<String>,
}

/// One normalized capability row shown by the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub name: String,
    pub availability: CapabilityAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Aggregate capability discovery result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityReport {
    pub server_version: Option<String>,
    pub runtime_path: String,
    pub extensions_path: String,
    pub cluster_snapshot_path: String,
    pub capabilities: Vec<CapabilityEntry>,
    pub extensions: Vec<ExtensionMetadata>,
    pub cluster: ClusterSnapshotMetadata,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_states_map_without_overstating_support() {
        let status = |state| RuntimeCapabilityStatus {
            state,
            reason: None,
        };

        assert_eq!(
            status(RuntimeCapabilityState::Supported).availability(),
            CapabilityAvailability::Available
        );
        assert_eq!(
            status(RuntimeCapabilityState::Unsupported).availability(),
            CapabilityAvailability::Unsupported
        );
        assert_eq!(
            status(RuntimeCapabilityState::Disabled).availability(),
            CapabilityAvailability::Disabled
        );
        assert_eq!(
            status(RuntimeCapabilityState::Unknown).availability(),
            CapabilityAvailability::Unknown
        );
    }

    #[test]
    fn availability_has_stable_machine_readable_values() {
        assert_eq!(
            serde_json::to_string(&CapabilityAvailability::VersionGated)
                .expect("availability should serialize"),
            "\"version-gated\""
        );
        assert_eq!(
            CapabilityAvailability::PermissionDenied.to_string(),
            "permission-denied"
        );
        assert_eq!(
            serde_json::to_string(&CapabilityAvailability::Unsupported)
                .expect("availability should serialize"),
            "\"unsupported\""
        );
    }
}
