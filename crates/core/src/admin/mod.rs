//! Admin API module
//!
//! This module provides the AdminApi trait and types for managing
//! IAM users, policies, groups, service accounts, and cluster operations.

mod cluster;
mod types;

pub use cluster::{
    BackendInfo, BackendType, BucketsInfo, ClusterInfo, DiskInfo, HealDriveInfo, HealDriveInfos,
    HealResultItem, HealScanMode, HealStartRequest, HealStatus, HealingDiskInfo, MemStats,
    ObjectsInfo, ServerInfo, UsageInfo,
};
pub use types::{
    CreateServiceAccountRequest, Group, GroupStatus, Policy, PolicyEntity, PolicyInfo,
    ServiceAccount, SetPolicyRequest, UpdateGroupMembersRequest, User, UserStatus,
};

use async_trait::async_trait;

use crate::error::Result;

/// Admin API trait for IAM and cluster management operations
///
/// This trait defines the interface for managing users, policies, groups,
/// service accounts, and cluster operations on S3-compatible storage systems
/// that support the RustFS/MinIO Admin API.
#[async_trait]
pub trait AdminApi: Send + Sync {
    // ==================== Cluster Operations ====================

    /// Get cluster information including servers, disks, and usage
    async fn cluster_info(&self) -> Result<ClusterInfo>;

    /// Get current heal status
    async fn heal_status(&self) -> Result<HealStatus>;

    /// Start a heal operation
    async fn heal_start(&self, request: HealStartRequest) -> Result<HealStatus>;

    /// Stop a running heal operation
    async fn heal_stop(&self) -> Result<()>;

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

    /// Delete a service account
    async fn delete_service_account(&self, access_key: &str) -> Result<()>;
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
