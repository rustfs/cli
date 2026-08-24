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
    #[serde(default)]
    pub access_key: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_user: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_status: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub implied_policy: Option<bool>,
}

impl ServiceAccount {
    pub fn new(access_key: impl Into<String>) -> Self {
        Self {
            access_key: access_key.into(),
            secret_key: None,
            parent_user: None,
            policy: None,
            account_status: None,
            expiration: None,
            name: None,
            description: None,
            implied_policy: None,
        }
    }
}

/// Identity-specific LDAP access key details.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LdapAccessKeyInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl LdapAccessKeyInfo {
    pub fn is_empty(&self) -> bool {
        self.username.is_none()
    }
}

/// Identity-specific OpenID access key details.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OpenIdAccessKeyInfo {
    #[serde(rename = "configName", skip_serializing_if = "Option::is_none")]
    pub config_name: Option<String>,

    #[serde(rename = "userID", skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    #[serde(rename = "userIDClaim", skip_serializing_if = "Option::is_none")]
    pub user_id_claim: Option<String>,

    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    #[serde(rename = "displayNameClaim", skip_serializing_if = "Option::is_none")]
    pub display_name_claim: Option<String>,
}

impl OpenIdAccessKeyInfo {
    pub fn is_empty(&self) -> bool {
        self.config_name.is_none()
            && self.user_id.is_none()
            && self.user_id_claim.is_none()
            && self.display_name.is_none()
            && self.display_name_claim.is_none()
    }
}

/// Common details shared by users, service accounts, and STS credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessKeyDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_user: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_status: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub implied_policy: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<String>,
}

/// General access key information returned by the RustFS Admin API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessKeyInfo {
    pub access_key: String,
    pub user_type: String,
    pub user_provider: String,

    #[serde(flatten)]
    pub info: AccessKeyDetails,

    #[serde(default, skip_serializing_if = "LdapAccessKeyInfo::is_empty")]
    pub ldap_specific_info: LdapAccessKeyInfo,

    #[serde(
        rename = "openIDSpecificInfo",
        default,
        skip_serializing_if = "OpenIdAccessKeyInfo::is_empty"
    )]
    pub open_id_specific_info: OpenIdAccessKeyInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccountCreateResponse {
    pub credentials: ServiceAccountCredentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccountCredentials {
    pub access_key: String,
    pub secret_key: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
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

    /// Group status
    #[serde(rename = "groupStatus", default)]
    pub status: String,
}

/// Request to create a service account
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateServiceAccountRequest {
    /// Optional policy document (JSON string)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,

    /// Expiration time (ISO 8601). The `expiration` field must be present in the request body; use null when no expiration is set.
    #[serde(rename = "expiration")]
    pub expiry: Option<String>,

    /// Optional name/description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Access key (required)
    #[serde(rename = "accessKey")]
    pub access_key: String,

    /// Secret key (required)
    #[serde(rename = "secretKey")]
    pub secret_key: String,

    /// Optional parent IAM user. Owner credentials may create a service
    /// account for another user; omitted requests stay parented to the caller.
    #[serde(rename = "targetUser", skip_serializing_if = "Option::is_none")]
    pub target_user: Option<String>,
}

/// Request to update an existing service account
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateServiceAccountRequest {
    /// Replacement policy document (JSON string)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_policy: Option<String>,

    /// Replacement secret key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_secret_key: Option<String>,

    /// Replacement account status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_status: Option<String>,

    /// Replacement friendly name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,

    /// Replacement description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_description: Option<String>,

    /// Replacement expiration time (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_expiration: Option<String>,
}

impl UpdateServiceAccountRequest {
    /// Return true when the request does not change any field.
    pub fn is_empty(&self) -> bool {
        self.new_policy.is_none()
            && self.new_secret_key.is_none()
            && self.new_status.is_none()
            && self.new_name.is_none()
            && self.new_description.is_none()
            && self.new_expiration.is_none()
    }
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
    #[serde(default = "default_quota_type")]
    pub quota_type: String,
}

fn default_quota_type() -> String {
    "HARD".to_string()
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
    fn test_create_service_account_request_includes_expiration() {
        let request = CreateServiceAccountRequest {
            policy: None,
            expiry: None,
            name: None,
            description: None,
            access_key: "myaccesskey".to_string(),
            secret_key: "mysecretkey".to_string(),
            target_user: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        // expiration field must always be present even when None
        assert!(
            json.contains("\"expiration\""),
            "JSON must contain expiration field, got: {json}"
        );

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("expiration").is_some());
        assert!(parsed["expiration"].is_null());
        assert!(parsed.get("targetUser").is_none());
    }

    #[test]
    fn test_create_service_account_request_with_expiry() {
        let request = CreateServiceAccountRequest {
            policy: None,
            expiry: Some("2025-12-31T23:59:59Z".to_string()),
            name: None,
            description: None,
            access_key: "myaccesskey".to_string(),
            secret_key: "mysecretkey".to_string(),
            target_user: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.get("expiration").and_then(|v| v.as_str()),
            Some("2025-12-31T23:59:59Z")
        );
    }

    #[test]
    fn test_create_service_account_request_serializes_target_user() {
        let request = CreateServiceAccountRequest {
            policy: None,
            expiry: None,
            name: None,
            description: None,
            access_key: "myaccesskey".to_string(),
            secret_key: "mysecretkey".to_string(),
            target_user: Some("test-user".to_string()),
        };

        let parsed: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(parsed["targetUser"], "test-user");
        assert_eq!(parsed["accessKey"], "myaccesskey");
    }

    #[test]
    fn test_update_service_account_request_serializes_provided_fields() {
        let request = UpdateServiceAccountRequest {
            new_policy: Some(r#"{"Version":"2012-10-17"}"#.to_string()),
            new_secret_key: Some("new-secret-key".to_string()),
            new_status: Some("enabled".to_string()),
            new_name: Some("automation-key".to_string()),
            new_description: Some("Used by automation".to_string()),
            new_expiration: Some("2030-01-01T00:00:00Z".to_string()),
        };

        let value = serde_json::to_value(&request).expect("serialize update request");
        assert_eq!(value["newPolicy"], r#"{"Version":"2012-10-17"}"#);
        assert_eq!(value["newSecretKey"], "new-secret-key");
        assert_eq!(value["newStatus"], "enabled");
        assert_eq!(value["newName"], "automation-key");
        assert_eq!(value["newDescription"], "Used by automation");
        assert_eq!(value["newExpiration"], "2030-01-01T00:00:00Z");
    }

    #[test]
    fn test_update_service_account_request_omits_unset_fields() {
        let request = UpdateServiceAccountRequest {
            new_description: Some("Updated description".to_string()),
            ..Default::default()
        };

        let value = serde_json::to_value(&request).expect("serialize update request");
        assert_eq!(value.as_object().expect("request object").len(), 1);
        assert_eq!(value["newDescription"], "Updated description");
    }

    #[test]
    fn test_access_key_info_deserializes_openid_server_shape() {
        let value = serde_json::json!({
            "accessKey": "sts-openid",
            "userType": "STS",
            "userProvider": "openid",
            "parentUser": "openid-parent",
            "accountStatus": "on",
            "openIDSpecificInfo": {
                "configName": "dex",
                "userID": "subject-123",
                "userIDClaim": "sub",
                "displayName": "RustFS User",
                "displayNameClaim": "name"
            }
        });

        let info: AccessKeyInfo =
            serde_json::from_value(value).expect("deserialize access key info");

        assert_eq!(info.access_key, "sts-openid");
        assert_eq!(info.user_type, "STS");
        assert_eq!(info.user_provider, "openid");
        assert_eq!(info.info.parent_user.as_deref(), Some("openid-parent"));
        assert_eq!(info.info.account_status.as_deref(), Some("on"));
        assert_eq!(
            info.open_id_specific_info.config_name.as_deref(),
            Some("dex")
        );
        assert_eq!(
            info.open_id_specific_info.user_id.as_deref(),
            Some("subject-123")
        );
        assert_eq!(
            info.open_id_specific_info.user_id_claim.as_deref(),
            Some("sub")
        );
        assert_eq!(
            info.open_id_specific_info.display_name.as_deref(),
            Some("RustFS User")
        );
        assert_eq!(
            info.open_id_specific_info.display_name_claim.as_deref(),
            Some("name")
        );
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

    #[test]
    fn test_bucket_quota_defaults_quota_type_when_missing() {
        let json = r#"{"bucket":"my-bucket","quota":1024,"size":512}"#;
        let decoded: BucketQuota = serde_json::from_str(json).unwrap();

        assert_eq!(decoded.bucket, "my-bucket");
        assert_eq!(decoded.quota, Some(1024));
        assert_eq!(decoded.size, 512);
        assert_eq!(decoded.quota_type, "HARD");
    }
}
