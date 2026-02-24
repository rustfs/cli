//! Admin API type definitions
//!
//! This module contains data structures for IAM management including
//! users, policies, groups, and service accounts.

use serde::{Deserialize, Serialize};

/// User status indicating whether the user is enabled or disabled
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    /// User is enabled and can access resources
    #[default]
    Enabled,
    /// User is disabled and cannot access resources
    Disabled,
}

impl std::fmt::Display for UserStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserStatus::Enabled => write!(f, "enabled"),
            UserStatus::Disabled => write!(f, "disabled"),
        }
    }
}

impl std::str::FromStr for UserStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "enabled" => Ok(UserStatus::Enabled),
            "disabled" => Ok(UserStatus::Disabled),
            _ => Err(format!("Invalid user status: {s}")),
        }
    }
}

/// Represents an IAM user
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    /// Access key ID (username)
    pub access_key: String,

    /// Secret access key (only present on creation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,

    /// User status
    #[serde(default)]
    pub status: UserStatus,

    /// Comma-separated policy names attached to this user
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_name: Option<String>,

    /// Groups this user belongs to
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_of: Vec<String>,
}

impl User {
    /// Create a new user with the given access key
    pub fn new(access_key: impl Into<String>) -> Self {
        Self {
            access_key: access_key.into(),
            secret_key: None,
            status: UserStatus::Enabled,
            policy_name: None,
            member_of: Vec::new(),
        }
    }

    /// Get the list of policy names as a vector
    pub fn policies(&self) -> Vec<String> {
        self.policy_name
            .as_ref()
            .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
            .unwrap_or_default()
    }
}

/// Group status indicating whether the group is enabled or disabled
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GroupStatus {
    /// Group is enabled
    #[default]
    Enabled,
    /// Group is disabled
    Disabled,
}

impl std::fmt::Display for GroupStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupStatus::Enabled => write!(f, "enabled"),
            GroupStatus::Disabled => write!(f, "disabled"),
        }
    }
}

impl std::str::FromStr for GroupStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "enabled" => Ok(GroupStatus::Enabled),
            "disabled" => Ok(GroupStatus::Disabled),
            _ => Err(format!("Invalid group status: {s}")),
        }
    }
}

/// Represents an IAM group
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    /// Group name
    pub name: String,

    /// Comma-separated policy names attached to this group
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,

    /// Group members (user access keys)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,

    /// Group status
    #[serde(default)]
    pub status: GroupStatus,
}

impl Group {
    /// Create a new group with the given name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            policy: None,
            members: Vec::new(),
            status: GroupStatus::Enabled,
        }
    }

    /// Get the list of policy names as a vector
    pub fn policies(&self) -> Vec<String> {
        self.policy
            .as_ref()
            .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
            .unwrap_or_default()
    }
}

/// Represents an IAM policy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    /// Policy name
    pub name: String,

    /// Policy document as JSON string
    pub policy: String,
}

impl Policy {
    /// Create a new policy with the given name and document
    pub fn new(name: impl Into<String>, policy: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            policy: policy.into(),
        }
    }

    /// Parse the policy document as JSON
    pub fn parse_document(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(&self.policy)
    }
}

/// Summary information about a policy (without the full document)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyInfo {
    /// Policy name
    pub name: String,
}

/// Represents a service account (access key pair)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccount {
    /// Access key ID
    pub access_key: String,

    /// Secret access key (only present on creation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,

    /// Parent user (owner of this service account)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_user: Option<String>,

    /// Policy attached to this service account
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,

    /// Account status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_status: Option<String>,

    /// Expiration time (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<String>,
}

impl ServiceAccount {
    /// Create a new service account with the given access key
    pub fn new(access_key: impl Into<String>) -> Self {
        Self {
            access_key: access_key.into(),
            secret_key: None,
            parent_user: None,
            policy: None,
            account_status: None,
            expiration: None,
        }
    }
}

/// Entity type for policy attachment
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEntity {
    /// Attach policy to a user
    User,
    /// Attach policy to a group
    Group,
}

impl std::fmt::Display for PolicyEntity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyEntity::User => write!(f, "user"),
            PolicyEntity::Group => write!(f, "group"),
        }
    }
}

/// Request to set/attach policies
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPolicyRequest {
    /// Policy names to attach
    pub name: Vec<String>,

    /// Entity type (user or group)
    pub entity_type: PolicyEntity,

    /// Entity name (user access key or group name)
    pub entity_name: String,
}

/// Request to update group members
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateGroupMembersRequest {
    /// Group name
    pub group: String,

    /// Members to add or remove
    pub members: Vec<String>,

    /// Whether to remove (true) or add (false) members
    #[serde(default)]
    pub is_remove: bool,
}

/// Request to create a service account
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateServiceAccountRequest {
    /// Optional policy document (JSON string)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,

    /// Optional expiration time
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,

    /// Optional name/description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Bucket quota information returned by Admin API
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BucketQuota {
    /// Bucket name
    pub bucket: String,

    /// Quota limit in bytes (None means unlimited)
    pub quota: Option<u64>,

    /// Current bucket usage in bytes
    pub size: u64,

    /// Quota type (currently only HARD)
    pub quota_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_status_display() {
        assert_eq!(UserStatus::Enabled.to_string(), "enabled");
        assert_eq!(UserStatus::Disabled.to_string(), "disabled");
    }

    #[test]
    fn test_user_status_from_str() {
        assert_eq!(
            "enabled".parse::<UserStatus>().unwrap(),
            UserStatus::Enabled
        );
        assert_eq!(
            "disabled".parse::<UserStatus>().unwrap(),
            UserStatus::Disabled
        );
        assert_eq!(
            "ENABLED".parse::<UserStatus>().unwrap(),
            UserStatus::Enabled
        );
        assert!("invalid".parse::<UserStatus>().is_err());
    }

    #[test]
    fn test_user_new() {
        let user = User::new("testuser");
        assert_eq!(user.access_key, "testuser");
        assert_eq!(user.status, UserStatus::Enabled);
        assert!(user.secret_key.is_none());
        assert!(user.member_of.is_empty());
    }

    #[test]
    fn test_user_policies() {
        let mut user = User::new("testuser");
        assert!(user.policies().is_empty());

        user.policy_name = Some("policy1, policy2, policy3".to_string());
        let policies = user.policies();
        assert_eq!(policies.len(), 3);
        assert_eq!(policies[0], "policy1");
        assert_eq!(policies[1], "policy2");
        assert_eq!(policies[2], "policy3");
    }

    #[test]
    fn test_group_new() {
        let group = Group::new("testgroup");
        assert_eq!(group.name, "testgroup");
        assert_eq!(group.status, GroupStatus::Enabled);
        assert!(group.members.is_empty());
    }

    #[test]
    fn test_group_policies() {
        let mut group = Group::new("testgroup");
        assert!(group.policies().is_empty());

        group.policy = Some("readonly,writeonly".to_string());
        let policies = group.policies();
        assert_eq!(policies.len(), 2);
        assert_eq!(policies[0], "readonly");
        assert_eq!(policies[1], "writeonly");
    }

    #[test]
    fn test_policy_new() {
        let policy = Policy::new("mypolicy", r#"{"Version":"2012-10-17"}"#);
        assert_eq!(policy.name, "mypolicy");
        assert!(policy.parse_document().is_ok());
    }

    #[test]
    fn test_policy_parse_document() {
        let policy = Policy::new("test", r#"{"Statement":[]}"#);
        let doc = policy.parse_document().unwrap();
        assert!(doc.get("Statement").is_some());
    }

    #[test]
    fn test_service_account_new() {
        let sa = ServiceAccount::new("accesskey123");
        assert_eq!(sa.access_key, "accesskey123");
        assert!(sa.secret_key.is_none());
        assert!(sa.parent_user.is_none());
    }

    #[test]
    fn test_policy_entity_display() {
        assert_eq!(PolicyEntity::User.to_string(), "user");
        assert_eq!(PolicyEntity::Group.to_string(), "group");
    }

    #[test]
    fn test_user_serialization() {
        let user = User {
            access_key: "testuser".to_string(),
            secret_key: Some("secret".to_string()),
            status: UserStatus::Enabled,
            policy_name: Some("policy1".to_string()),
            member_of: vec!["group1".to_string()],
        };

        let json = serde_json::to_string(&user).unwrap();
        assert!(json.contains("testuser"));
        assert!(json.contains("accessKey"));

        let deserialized: User = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.access_key, "testuser");
    }

    #[test]
    fn test_group_status_from_str() {
        assert_eq!(
            "enabled".parse::<GroupStatus>().unwrap(),
            GroupStatus::Enabled
        );
        assert_eq!(
            "disabled".parse::<GroupStatus>().unwrap(),
            GroupStatus::Disabled
        );
        assert!("invalid".parse::<GroupStatus>().is_err());
    }

    #[test]
    fn test_bucket_quota_serialization() {
        let quota = BucketQuota {
            bucket: "my-bucket".to_string(),
            quota: Some(1024),
            size: 512,
            quota_type: "HARD".to_string(),
        };

        let json = serde_json::to_string(&quota).unwrap();
        assert!(json.contains("my-bucket"));
        assert!(json.contains("quotaType"));

        let decoded: BucketQuota = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.bucket, "my-bucket");
        assert_eq!(decoded.quota, Some(1024));
    }
}
