//! Admin API module
//!
//! This module provides the AdminApi trait and types for managing
//! IAM users, policies, groups, service accounts, and cluster operations.

mod capabilities;
mod cluster;
mod configuration;
mod diagnostics;
mod kms;
mod kms_diagnostic;
mod observability;
mod replication;
mod site;
pub mod tier;
mod types;

pub use capabilities::{
    CapabilityAvailability, CapabilityEntry, CapabilityReport, ClusterSnapshotMetadata,
    ClusterSnapshotSummary, DiagnosticCapability, DiagnosticCapabilityGuardError,
    ExtensionMetadata, ExtensionsCatalog, RuntimeCapabilitiesSnapshot, RuntimeCapabilitiesSummary,
    RuntimeCapabilityState, RuntimeCapabilityStatus,
};
pub use cluster::{
    BackendInfo, BackendType, BucketsInfo, ClusterInfo, DecommissionPoolStatus, DecommissionStatus,
    DiskInfo, HealDriveInfo, HealDriveInfos, HealResultItem, HealRuntimeState, HealScanMode,
    HealStartRequest, HealStatus, HealTaskRequest, HealingDiskInfo, MemStats, ObjectsInfo,
    PoolDecommissionInfo, PoolErasureSetInfo, PoolStatus, PoolTarget, RebalanceCleanupWarnings,
    RebalancePoolProgress, RebalancePoolStatus, RebalanceStartResult, RebalanceStatus, ServerInfo,
    UsageInfo,
};
pub use configuration::{
    ConfigApi, ConfigChange, ConfigDiff, ConfigDocument, ConfigHelp, ConfigHelpEntry,
    ConfigHistoryEntry, ConfigMutationResult, ModuleSwitches, config_document_fields,
    config_import_diff, config_mutation_diff, redact_config_document, validate_config_directive,
    validate_config_import,
};
pub use diagnostics::{
    ClientDevnullRequest, ClientDevnullResult, ClusterComponentSnapshot, ClusterComponentSnapshots,
    ClusterListingSnapshot, ClusterSnapshotDocument, ClusterUsageSnapshot,
    DEFAULT_CLIENT_DEVNULL_BYTES, DEFAULT_CLIENT_DEVNULL_CONCURRENCY,
    DEFAULT_CLIENT_DEVNULL_TIMEOUT, DetailedHealthSnapshot, DiagnosticClusterSnapshot,
    DiagnosticClusterSummary, DiagnosticReadApi, HealthCpuSnapshot, HealthDriveSnapshot,
    HealthMemorySnapshot, HealthOsSnapshot, HealthProcessSnapshot,
    MAX_CLIENT_DEVNULL_AGGREGATE_BYTES, MAX_CLIENT_DEVNULL_CONCURRENCY, MAX_CLIENT_DEVNULL_TIMEOUT,
    MAX_DIAGNOSTIC_RESPONSE_BYTES,
};
pub use kms::{
    KmsApi, KmsBackendKind, KmsCacheSummary, KmsCancelKeyDeletionResult, KmsConfigSummary,
    KmsConfigureRequest, KmsCreateKeyRequest, KmsCreateKeyResult, KmsDeleteKeyRequest,
    KmsDeleteKeyResult, KmsKey, KmsKeyPage, KmsKeyState, KmsKeyUsage, KmsLocalConfigureRequest,
    KmsServiceState, KmsStatus, KmsVaultAuthMethod, KmsVaultKv2ConfigureRequest,
    KmsVaultTransitConfigureRequest,
};
pub use kms_diagnostic::{
    KMS_DIAGNOSTIC_CONTENT_BYTES, KmsDiagnosticStore, KmsRoundTripError, KmsRoundTripErrorClass,
    KmsRoundTripPhase, KmsRoundTripReport, KmsRoundTripTimings, run_kms_round_trip,
};
pub use observability::{
    MAX_METRICS_LINE_BYTES, MAX_METRICS_RESPONSE_BYTES, MAX_METRICS_SAMPLES, MetricGroup,
    MetricGroups, MetricsBatch, MetricsQuery, MetricsScope, ObservabilityApi, RealtimeMetrics,
    ScannerCycleSchedule, ScannerFreshness, ScannerHealth, ScannerMetrics, ScannerRuntimeConfig,
    ScannerRuntimeConfigValue, ScannerStatus, StorageBackend, StorageBackendKind, StorageDisk,
    StorageDiskMetrics, StorageInfo,
};
pub use replication::{
    MAX_REPLICATION_DIFF_RESPONSE_BYTES, ReplicationDiff, ReplicationDiffApi, ReplicationDiffEntry,
};
pub use site::{
    MAX_SITE_REPLICATION_CA_CERT_BYTES, MAX_SITE_REPLICATION_ERROR_RESPONSE_BYTES,
    MAX_SITE_REPLICATION_REQUEST_BYTES, MAX_SITE_REPLICATION_SUCCESS_RESPONSE_BYTES, PeerSiteSpec,
    ReplicateEditStatus, ServiceActionResult, SiteRemoveSpec, SiteReplicationInfo,
    SiteReplicationPeer, SiteReplicationResyncBucketStatus, SiteReplicationResyncOperation,
    SiteReplicationResyncStatus, SiteStatusOptions, validate_site_replication_ca_bundle,
};
pub use tier::{
    ManualTransitionRunReport, ManualTransitionRunRequest, ManualTransitionRunResponse, TierAliyun,
    TierAzure, TierConfig, TierCreds, TierGCS, TierHuaweicloud, TierMinIO, TierR2, TierRustFS,
    TierS3, TierTencent, TierType,
};
pub use types::{
    AccessKeyDetails, AccessKeyInfo, BucketQuota, CreateServiceAccountRequest, Group, GroupStatus,
    LdapAccessKeyInfo, OpenIdAccessKeyInfo, Policy, PolicyEntity, PolicyInfo, ServiceAccount,
    ServiceAccountCreateResponse, ServiceAccountCredentials, SetPolicyRequest,
    UpdateGroupMembersRequest, UpdateServiceAccountRequest, User, UserStatus,
};

use async_trait::async_trait;

use crate::error::Result;

/// Admin API trait for IAM and cluster management operations
///
/// This trait defines the interface for managing users, policies, groups,
/// service accounts, and cluster operations on S3-compatible storage systems
/// that support the RustFS Admin API.
#[async_trait]
pub trait AdminApi: Send + Sync {
    // ==================== Cluster Operations ====================

    /// Get cluster information including servers, disks, and usage
    async fn cluster_info(&self) -> Result<ClusterInfo>;

    /// Get current heal status
    async fn heal_status(&self) -> Result<HealStatus>;

    /// Start a heal operation
    async fn heal_start(&self, request: HealStartRequest) -> Result<HealStatus>;

    /// Get status for a token-scoped heal task
    async fn heal_task_status(&self, request: HealTaskRequest) -> Result<HealStatus>;

    /// Stop a running heal operation
    async fn heal_stop(&self) -> Result<()>;

    /// Stop a token-scoped heal task
    async fn heal_task_stop(&self, request: HealTaskRequest) -> Result<HealStatus>;

    /// List storage pools
    async fn list_pools(&self) -> Result<Vec<PoolStatus>>;

    /// Get storage pool status
    async fn pool_status(&self, target: PoolTarget) -> Result<PoolStatus>;

    /// Start decommissioning one or more storage pools
    async fn decommission_start(&self, target: PoolTarget) -> Result<()>;

    /// Cancel decommissioning a storage pool
    async fn decommission_cancel(&self, target: PoolTarget) -> Result<()>;

    /// Clear failed or canceled decommissioning metadata for a storage pool
    async fn decommission_clear(&self, target: PoolTarget) -> Result<()>;

    /// Get decommissioning status
    async fn decommission_status(&self, target: Option<PoolTarget>) -> Result<DecommissionStatus>;

    /// Start a rebalance operation
    async fn rebalance_start(&self) -> Result<RebalanceStartResult>;

    /// Get rebalance status
    async fn rebalance_status(&self) -> Result<RebalanceStatus>;

    /// Stop a running rebalance operation
    async fn rebalance_stop(&self) -> Result<()>;

    // ==================== User Operations ====================

    /// List all users
    async fn list_users(&self) -> Result<Vec<User>>;

    /// Get user information
    async fn get_user(&self, access_key: &str) -> Result<User>;

    /// Create a new user
    async fn create_user(&self, access_key: &str, secret_key: &str) -> Result<User>;

    /// Delete a user
    async fn delete_user(&self, access_key: &str) -> Result<()>;

    /// Set user status (enable/disable)
    async fn set_user_status(&self, access_key: &str, status: UserStatus) -> Result<()>;

    // ==================== Policy Operations ====================

    /// List all policies
    async fn list_policies(&self) -> Result<Vec<PolicyInfo>>;

    /// Get policy information
    async fn get_policy(&self, name: &str) -> Result<Policy>;

    /// Create a new policy
    async fn create_policy(&self, name: &str, policy_document: &str) -> Result<()>;

    /// Delete a policy
    async fn delete_policy(&self, name: &str) -> Result<()>;

    /// Attach policy to a user or group
    async fn attach_policy(
        &self,
        policy_names: &[String],
        entity_type: PolicyEntity,
        entity_name: &str,
    ) -> Result<()>;

    /// Detach policy from a user or group
    async fn detach_policy(
        &self,
        policy_names: &[String],
        entity_type: PolicyEntity,
        entity_name: &str,
    ) -> Result<()>;

    // ==================== Group Operations ====================

    /// List all groups
    async fn list_groups(&self) -> Result<Vec<String>>;

    /// Get group information
    async fn get_group(&self, name: &str) -> Result<Group>;

    /// Create a new group
    async fn create_group(&self, name: &str, members: Option<&[String]>) -> Result<Group>;

    /// Delete a group
    async fn delete_group(&self, name: &str) -> Result<()>;

    /// Set group status (enable/disable)
    async fn set_group_status(&self, name: &str, status: GroupStatus) -> Result<()>;

    /// Add members to a group
    async fn add_group_members(&self, group: &str, members: &[String]) -> Result<()>;

    /// Remove members from a group
    async fn remove_group_members(&self, group: &str, members: &[String]) -> Result<()>;

    // ==================== Service Account Operations ====================

    /// List service accounts for a user
    async fn list_service_accounts(&self, user: Option<&str>) -> Result<Vec<ServiceAccount>>;

    /// Get service account information
    async fn get_service_account(&self, access_key: &str) -> Result<ServiceAccount>;

    /// Create a new service account
    async fn create_service_account(
        &self,
        request: CreateServiceAccountRequest,
    ) -> Result<ServiceAccount>;

    /// Update an existing service account
    async fn update_service_account(
        &self,
        access_key: &str,
        request: UpdateServiceAccountRequest,
    ) -> Result<()>;

    /// Delete a service account
    async fn delete_service_account(&self, access_key: &str) -> Result<()>;

    /// Get information for any access key type.
    async fn get_access_key_info(&self, access_key: &str) -> Result<AccessKeyInfo>;

    // ==================== Bucket Quota Operations ====================

    /// Set bucket quota in bytes
    async fn set_bucket_quota(&self, bucket: &str, quota: u64) -> Result<BucketQuota>;

    /// Get bucket quota information
    async fn get_bucket_quota(&self, bucket: &str) -> Result<BucketQuota>;

    /// Clear bucket quota
    async fn clear_bucket_quota(&self, bucket: &str) -> Result<BucketQuota>;

    // ==================== Tier Operations ====================

    /// List all configured storage tiers
    async fn list_tiers(&self) -> Result<Vec<TierConfig>>;

    /// Get tier statistics
    async fn tier_stats(&self) -> Result<serde_json::Value>;

    /// Add a new storage tier
    async fn add_tier(&self, config: TierConfig) -> Result<()>;

    /// Edit tier credentials
    async fn edit_tier(&self, name: &str, creds: TierCreds) -> Result<()>;

    /// Remove a storage tier
    async fn remove_tier(&self, name: &str, force: bool) -> Result<()>;

    /// Run bounded manual lifecycle transition evaluation for a bucket scope.
    async fn run_manual_transition(
        &self,
        request: ManualTransitionRunRequest,
    ) -> Result<ManualTransitionRunResponse>;

    // ==================== Replication Target Operations ====================

    /// Set a remote replication target for a bucket, returns the ARN
    async fn set_remote_target(
        &self,
        bucket: &str,
        target: crate::replication::BucketTarget,
        update: bool,
    ) -> Result<String>;

    /// List remote replication targets for a bucket
    async fn list_remote_targets(
        &self,
        bucket: &str,
    ) -> Result<Vec<crate::replication::BucketTarget>>;

    /// Remove a remote replication target
    async fn remove_remote_target(&self, bucket: &str, arn: &str) -> Result<()>;

    /// Get replication metrics for a bucket
    async fn replication_metrics(&self, bucket: &str) -> Result<serde_json::Value>;

    // ==================== Service Control Operations ====================

    /// Request a service action (restart, stop, freeze, unfreeze)
    async fn service_action(&self, action: &str) -> Result<ServiceActionResult>;

    // ==================== Site Replication Operations ====================

    /// Get current site replication configuration
    async fn site_replication_info(&self) -> Result<SiteReplicationInfo>;

    /// Edit a peer using a complete read-modify-write snapshot
    async fn site_replication_edit(
        &self,
        peer: &SiteReplicationPeer,
    ) -> Result<ReplicateEditStatus>;

    /// Start, inspect, or cancel a resync toward one complete peer snapshot
    async fn site_replication_resync(
        &self,
        operation: SiteReplicationResyncOperation,
        peer: &SiteReplicationPeer,
    ) -> Result<SiteReplicationResyncStatus>;

    /// Add peer sites to the site replication cluster
    async fn site_replication_add(&self, sites: &[PeerSiteSpec]) -> Result<serde_json::Value>;

    /// Get site replication status
    async fn site_replication_status(
        &self,
        options: &SiteStatusOptions,
    ) -> Result<serde_json::Value>;

    /// Remove sites from the site replication cluster
    async fn site_replication_remove(&self, spec: &SiteRemoveSpec) -> Result<serde_json::Value>;
}

/// Read-only RustFS runtime capability discovery.
#[async_trait]
pub trait CapabilityApi: Send + Sync {
    /// Discover capabilities, bypassing the process cache when `refresh` is true.
    async fn discover_capabilities(&self, refresh: bool) -> Result<CapabilityReport>;
}

/// Bounded active RustFS diagnostic probes.
#[async_trait]
pub trait DiagnosticApi: CapabilityApi {
    /// Measure client-to-server upload throughput without persisting an object.
    async fn client_devnull(&self, request: ClientDevnullRequest) -> Result<ClientDevnullResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test that types are re-exported correctly
    #[test]
    fn test_user_status_reexport() {
        assert_eq!(UserStatus::Enabled.to_string(), "enabled");
        assert_eq!(UserStatus::Disabled.to_string(), "disabled");
    }

    #[test]
    fn test_group_status_reexport() {
        assert_eq!(GroupStatus::Enabled.to_string(), "enabled");
        assert_eq!(GroupStatus::Disabled.to_string(), "disabled");
    }

    #[test]
    fn test_policy_entity_reexport() {
        assert_eq!(PolicyEntity::User.to_string(), "user");
        assert_eq!(PolicyEntity::Group.to_string(), "group");
    }

    #[test]
    fn test_user_new() {
        let user = User::new("testuser");
        assert_eq!(user.access_key, "testuser");
        assert_eq!(user.status, UserStatus::Enabled);
    }

    #[test]
    fn test_group_new() {
        let group = Group::new("developers");
        assert_eq!(group.name, "developers");
        assert_eq!(group.status, GroupStatus::Enabled);
    }

    #[test]
    fn test_policy_new() {
        let policy = Policy::new("readonly", r#"{"Version":"2012-10-17","Statement":[]}"#);
        assert_eq!(policy.name, "readonly");
        assert!(policy.parse_document().is_ok());
    }

    #[test]
    fn test_service_account_new() {
        let sa = ServiceAccount::new("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(sa.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert!(sa.secret_key.is_none());
    }
}
