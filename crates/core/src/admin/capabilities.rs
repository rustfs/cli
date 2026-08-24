//! Runtime capability discovery contracts for the RustFS Admin API.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    #[serde(other)]
    Unknown,
}

/// A typed status from a RustFS runtime snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilityStatus {
    pub state: RuntimeCapabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
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

    /// Stable display label for the server-reported state.
    pub const fn state_label(&self) -> &'static str {
        match self.state {
            RuntimeCapabilityState::Supported => "supported",
            RuntimeCapabilityState::Unsupported => "unsupported",
            RuntimeCapabilityState::Disabled => "disabled",
            RuntimeCapabilityState::Unknown => "unknown",
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

/// One named admin capability advertised by the server itself
/// (rustfs/backlog#1900). Servers after 1.0.0-rc.3 include this list in
/// `/v4/runtime/capabilities`; older servers omit it, in which case the
/// client falls back to its pinned per-version contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertisedAdminCapability {
    pub name: String,
    pub status: RuntimeCapabilityStatus,
}

/// Typed subset of the RustFS runtime capability response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapabilitiesSnapshot {
    pub summary: RuntimeCapabilitiesSummary,
    #[serde(default)]
    pub advertised: Vec<AdvertisedAdminCapability>,
    #[serde(default)]
    pub inspect_archive: Option<super::InspectArchiveCapabilityContract>,
    #[serde(default)]
    pub site_replication_repair: Option<super::SiteReplicationRepairCapabilityContract>,
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
    #[serde(default)]
    pub runtime: serde_json::Value,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub disabled_by_default: bool,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Typed subset of `/v4/extensions/catalog`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionsCatalog {
    pub extensions: Vec<ExtensionMetadata>,
    pub runtime_capabilities: BTreeMap<String, serde_json::Value>,
    pub cluster_snapshot: BTreeMap<String, serde_json::Value>,
    pub external_plugin_flow: BTreeMap<String, serde_json::Value>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
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

/// A diagnostic operation whose support must be known before it is invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCapability {
    HealthSnapshot,
    ClusterSnapshot,
    ExtensionsCatalog,
    DriveObservations,
    ClientDevnull,
    InspectArchive,
    ObjectSpeedtest,
    NetworkSpeedtest,
    SiteSpeedtest,
    SiteReplicationNetperf,
}

impl DiagnosticCapability {
    /// All diagnostic capabilities in stable display order.
    pub const ALL: [Self; 10] = [
        Self::HealthSnapshot,
        Self::ClusterSnapshot,
        Self::ExtensionsCatalog,
        Self::DriveObservations,
        Self::ClientDevnull,
        Self::InspectArchive,
        Self::ObjectSpeedtest,
        Self::NetworkSpeedtest,
        Self::SiteSpeedtest,
        Self::SiteReplicationNetperf,
    ];

    /// Stable machine-readable capability name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::HealthSnapshot => "admin.diagnostics.health-snapshot",
            Self::ClusterSnapshot => "admin.diagnostics.cluster-snapshot",
            Self::ExtensionsCatalog => "admin.diagnostics.extensions-catalog",
            Self::DriveObservations => "admin.diagnostics.drive-observations",
            Self::ClientDevnull => "admin.diagnostics.client-devnull",
            Self::InspectArchive => "admin.diagnostics.inspect-archive",
            Self::ObjectSpeedtest => "admin.diagnostics.object-speedtest",
            Self::NetworkSpeedtest => "admin.diagnostics.network-speedtest",
            Self::SiteSpeedtest => "admin.diagnostics.site-speedtest",
            Self::SiteReplicationNetperf => "admin.site-replication.netperf",
        }
    }
}

/// Error returned when a diagnostic operation is not explicitly available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticCapabilityGuardError {
    capability: DiagnosticCapability,
    availability: CapabilityAvailability,
    reason: Option<String>,
}

impl DiagnosticCapabilityGuardError {
    /// Diagnostic operation rejected by the guard.
    pub const fn capability(&self) -> DiagnosticCapability {
        self.capability
    }

    /// Effective support classification that caused the rejection.
    pub const fn availability(&self) -> CapabilityAvailability {
        self.availability
    }

    /// Server or client explanation for the classification, when available.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

impl fmt::Display for DiagnosticCapabilityGuardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "diagnostic capability '{}' is {}",
            self.capability.name(),
            self.availability
        )?;
        if let Some(reason) = &self.reason {
            write!(formatter, ": {reason}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DiagnosticCapabilityGuardError {}

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

impl CapabilityReport {
    /// Find a normalized capability by its stable machine-readable name.
    pub fn capability(&self, name: &str) -> Option<&CapabilityEntry> {
        self.capabilities.iter().find(|entry| entry.name == name)
    }

    /// Require explicit availability before invoking a diagnostic operation.
    ///
    /// Missing entries and every non-available state fail closed. This prevents
    /// route presence or placeholder HTTP success responses from being treated
    /// as proof that an active diagnostic is implemented.
    pub fn require_diagnostic_capability(
        &self,
        capability: DiagnosticCapability,
    ) -> Result<&CapabilityEntry, DiagnosticCapabilityGuardError> {
        match self.capability(capability.name()) {
            Some(entry) if entry.availability == CapabilityAvailability::Available => Ok(entry),
            Some(entry) => Err(DiagnosticCapabilityGuardError {
                capability,
                availability: entry.availability,
                reason: entry.reason.clone(),
            }),
            None => Err(DiagnosticCapabilityGuardError {
                capability,
                availability: CapabilityAvailability::Unknown,
                reason: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(capabilities: Vec<CapabilityEntry>) -> CapabilityReport {
        CapabilityReport {
            server_version: Some("1.0.0-beta.10".to_string()),
            runtime_path: "/v4/runtime/capabilities".to_string(),
            extensions_path: "/v4/extensions/catalog".to_string(),
            cluster_snapshot_path: "/v4/cluster/snapshot".to_string(),
            capabilities,
            extensions: Vec::new(),
            cluster: ClusterSnapshotMetadata {
                summary: None,
                runtime_capabilities_path: None,
                extensions_catalog_path: None,
            },
        }
    }

    #[test]
    fn runtime_states_map_without_overstating_support() {
        let status = |state| RuntimeCapabilityStatus {
            state,
            reason: None,
            extra: BTreeMap::new(),
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

    #[test]
    fn diagnostic_capability_names_are_stable_and_unique() {
        let names = DiagnosticCapability::ALL.map(DiagnosticCapability::name);

        assert_eq!(names.len(), 10);
        assert_eq!(names[0], "admin.diagnostics.health-snapshot");
        assert_eq!(names[9], "admin.site-replication.netperf");
        for (index, name) in names.iter().enumerate() {
            assert!(!names[..index].contains(name), "duplicate name: {name}");
        }
    }

    #[test]
    fn diagnostic_guard_allows_only_available_capabilities() {
        let capability = DiagnosticCapability::HealthSnapshot;
        let report = report(vec![CapabilityEntry {
            name: capability.name().to_string(),
            availability: CapabilityAvailability::Available,
            reason: Some("Pinned server contract".to_string()),
        }]);

        let entry = report
            .require_diagnostic_capability(capability)
            .expect("available diagnostics should pass the guard");

        assert_eq!(entry.name, capability.name());
    }

    #[test]
    fn diagnostic_guard_preserves_non_available_states() {
        let capability = DiagnosticCapability::ObjectSpeedtest;
        let blocked_states = [
            CapabilityAvailability::Stubbed,
            CapabilityAvailability::Unsupported,
            CapabilityAvailability::Disabled,
            CapabilityAvailability::VersionGated,
            CapabilityAvailability::PermissionDenied,
            CapabilityAvailability::Unknown,
        ];

        for availability in blocked_states {
            let report = report(vec![CapabilityEntry {
                name: capability.name().to_string(),
                availability,
                reason: Some("Server classification".to_string()),
            }]);
            let error = report
                .require_diagnostic_capability(capability)
                .expect_err("non-available diagnostics must fail closed");

            assert_eq!(error.capability(), capability);
            assert_eq!(error.availability(), availability);
            assert_eq!(error.reason(), Some("Server classification"));
        }
    }

    #[test]
    fn diagnostic_guard_treats_missing_entries_as_unknown() {
        let capability = DiagnosticCapability::NetworkSpeedtest;
        let error = report(Vec::new())
            .require_diagnostic_capability(capability)
            .expect_err("missing diagnostic classifications must fail closed");

        assert_eq!(error.capability(), capability);
        assert_eq!(error.availability(), CapabilityAvailability::Unknown);
        assert_eq!(error.reason(), None);
    }
}
