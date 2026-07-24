//! Typed contracts for IAM policy inspection and mutation.

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Capability name used to guard policy-entity inspection.
pub const IAM_POLICY_ENTITIES_CAPABILITY: &str = "admin.iam.policy-entities";

/// Capability name used to guard builtin policy detach mutations.
pub const IAM_POLICY_DETACH_CAPABILITY: &str = "admin.iam.policy-detach";

/// Maximum encoded size accepted for one policy-entity response.
pub const MAX_IAM_POLICY_ENTITIES_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum encoded size accepted for one policy detach response.
pub const MAX_IAM_POLICY_DETACH_RESPONSE_BYTES: usize = 1024 * 1024;

/// Maximum encoded size sent for one policy detach request.
pub const MAX_IAM_POLICY_DETACH_REQUEST_BYTES: usize = 512 * 1024;

/// Maximum number of selectors accepted in a single request.
pub const MAX_IAM_POLICY_ENTITY_SELECTORS: usize = 1_000;

/// Maximum UTF-8 byte length accepted for one selector.
pub const MAX_IAM_POLICY_ENTITY_SELECTOR_BYTES: usize = 1_024;

/// Maximum number of policies accepted by one detach request.
pub const MAX_IAM_POLICY_DETACH_POLICIES: usize = 1_000;

/// Maximum UTF-8 byte length accepted for a detach selector.
pub const MAX_IAM_POLICY_DETACH_SELECTOR_BYTES: usize = 1_024;

/// A builtin IAM entity affected by a policy mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyDetachEntity {
    User,
    Group,
}

impl std::fmt::Display for PolicyDetachEntity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User => formatter.write_str("user"),
            Self::Group => formatter.write_str("group"),
        }
    }
}

/// A validated, retry-safe builtin policy detach request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDetachRequest {
    pub policies: Vec<String>,
    pub entity: PolicyDetachEntity,
    pub entity_name: String,
}

impl PolicyDetachRequest {
    /// Validate and normalize selectors before any network request is made.
    pub fn new(
        mut policies: Vec<String>,
        entity: PolicyDetachEntity,
        entity_name: String,
    ) -> Result<Self> {
        validate_detach_selector("entity", &entity_name)?;
        if policies.is_empty() {
            return Err(Error::InvalidPath(
                "At least one policy name is required".to_string(),
            ));
        }
        if policies.len() > MAX_IAM_POLICY_DETACH_POLICIES {
            return Err(Error::InvalidPath(format!(
                "Policy detach accepts at most {MAX_IAM_POLICY_DETACH_POLICIES} policies"
            )));
        }
        for policy in &policies {
            validate_detach_selector("policy", policy)?;
        }
        policies.sort();
        policies.dedup();
        Ok(Self {
            policies,
            entity,
            entity_name,
        })
    }
}

fn validate_detach_selector(kind: &str, selector: &str) -> Result<()> {
    if selector.is_empty() || selector.trim() != selector {
        return Err(Error::InvalidPath(format!(
            "Policy detach {kind} selector cannot be empty or contain surrounding whitespace"
        )));
    }
    if selector.len() > MAX_IAM_POLICY_DETACH_SELECTOR_BYTES {
        return Err(Error::InvalidPath(format!(
            "Policy detach {kind} selector exceeds {MAX_IAM_POLICY_DETACH_SELECTOR_BYTES} bytes"
        )));
    }
    if selector.chars().any(char::is_control) {
        return Err(Error::InvalidPath(format!(
            "Policy detach {kind} selector cannot contain control characters"
        )));
    }
    Ok(())
}

/// Normalized outcome for an idempotent builtin policy detach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDetachResult {
    pub entity: PolicyDetachEntity,
    pub entity_name: String,
    pub attached: Vec<String>,
    pub detached: Vec<String>,
    pub unchanged: Vec<String>,
    pub updated_at: Timestamp,
}

/// Typed builtin IAM mutation operations.
#[async_trait]
pub trait IamMutationApi: Send + Sync {
    /// Detach only the requested policies from exactly one user or group.
    async fn detach_policies(&self, request: &PolicyDetachRequest) -> Result<PolicyDetachResult>;
}

/// Filters for a policy-entity inspection request.
///
/// An empty query asks RustFS for every policy-to-entity mapping. User and group
/// filters return their direct and inherited policy mappings. Policy filters
/// return the matching users and groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyEntitiesQuery {
    pub users: Vec<String>,
    pub groups: Vec<String>,
    pub policies: Vec<String>,
}

impl PolicyEntitiesQuery {
    /// Reject selectors that would create ambiguous or excessively large requests.
    pub fn validate(&self) -> Result<()> {
        let selector_count = self.users.len() + self.groups.len() + self.policies.len();
        if selector_count > MAX_IAM_POLICY_ENTITY_SELECTORS {
            return Err(Error::InvalidPath(format!(
                "IAM policy-entity query accepts at most {MAX_IAM_POLICY_ENTITY_SELECTORS} selectors"
            )));
        }

        for (kind, selectors) in [
            ("user", self.users.as_slice()),
            ("group", self.groups.as_slice()),
            ("policy", self.policies.as_slice()),
        ] {
            for selector in selectors {
                if selector.trim().is_empty() {
                    return Err(Error::InvalidPath(format!(
                        "IAM policy-entity {kind} selector cannot be empty"
                    )));
                }
                if selector.len() > MAX_IAM_POLICY_ENTITY_SELECTOR_BYTES {
                    return Err(Error::InvalidPath(format!(
                        "IAM policy-entity {kind} selector exceeds {MAX_IAM_POLICY_ENTITY_SELECTOR_BYTES} bytes"
                    )));
                }
            }
        }

        Ok(())
    }
}

/// Policy mappings returned by RustFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEntitiesResult {
    pub timestamp: Timestamp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_mappings: Vec<UserPolicyEntities>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_mappings: Vec<GroupPolicyEntities>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_mappings: Vec<PolicyEntities>,
}

impl PolicyEntitiesResult {
    /// Validate identifiers and normalize mapping order for deterministic output.
    pub fn normalize(mut self) -> Result<Self> {
        for mapping in &mut self.user_mappings {
            validate_name("user", &mapping.user)?;
            normalize_names("policy", &mut mapping.policies)?;
            for inherited in &mut mapping.member_of_mappings {
                normalize_group_mapping(inherited)?;
            }
            mapping
                .member_of_mappings
                .sort_by(|left, right| left.group.cmp(&right.group));
            mapping
                .member_of_mappings
                .dedup_by(|left, right| left.group == right.group);
        }
        self.user_mappings
            .sort_by(|left, right| left.user.cmp(&right.user));
        self.user_mappings
            .dedup_by(|left, right| left.user == right.user);

        for mapping in &mut self.group_mappings {
            normalize_group_mapping(mapping)?;
        }
        self.group_mappings
            .sort_by(|left, right| left.group.cmp(&right.group));
        self.group_mappings
            .dedup_by(|left, right| left.group == right.group);

        for mapping in &mut self.policy_mappings {
            validate_name("policy", &mapping.policy)?;
            normalize_names("user", &mut mapping.users)?;
            normalize_names("group", &mut mapping.groups)?;
        }
        self.policy_mappings
            .sort_by(|left, right| left.policy.cmp(&right.policy));
        self.policy_mappings
            .dedup_by(|left, right| left.policy == right.policy);

        Ok(self)
    }
}

fn normalize_group_mapping(mapping: &mut GroupPolicyEntities) -> Result<()> {
    validate_name("group", &mapping.group)?;
    normalize_names("policy", &mut mapping.policies)
}

fn normalize_names(kind: &str, names: &mut Vec<String>) -> Result<()> {
    for name in names.iter() {
        validate_name(kind, name)?;
    }
    names.sort();
    names.dedup();
    Ok(())
}

fn validate_name(kind: &str, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::General(format!(
            "RustFS IAM policy-entity response contains an empty {kind} name"
        )));
    }
    Ok(())
}

/// Direct and inherited policy mappings for one user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPolicyEntities {
    pub user: String,
    #[serde(default)]
    pub policies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub member_of_mappings: Vec<GroupPolicyEntities>,
}

/// Direct policy mappings for one group.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupPolicyEntities {
    pub group: String,
    #[serde(default)]
    pub policies: Vec<String>,
}

/// Users and groups attached to one policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEntities {
    pub policy: String,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
}

/// Read-only RustFS IAM policy-entity operations.
#[async_trait]
pub trait IamReadApi: Send + Sync {
    /// Inspect direct and inherited policy associations.
    async fn policy_entities(&self, query: &PolicyEntitiesQuery) -> Result<PolicyEntitiesResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_decodes_current_rustfs_shape_without_secret_fields() {
        let result: PolicyEntitiesResult = serde_json::from_str(
            r#"{
                "timestamp": "2026-07-24T08:00:00Z",
                "userMappings": [{
                    "user": "alice",
                    "policies": ["readonly"],
                    "memberOfMappings": [{"group": "ops", "policies": ["diagnostics"]}],
                    "secretKey": "must-not-survive"
                }],
                "groupMappings": [{"group": "ops", "policies": ["diagnostics"]}],
                "policyMappings": [{
                    "policy": "readonly",
                    "users": ["alice"],
                    "groups": []
                }],
                "sessionToken": "must-not-survive"
            }"#,
        )
        .expect("decode RustFS policy entities");

        assert_eq!(result.user_mappings[0].user, "alice");
        assert_eq!(result.user_mappings[0].member_of_mappings[0].group, "ops");

        let encoded = serde_json::to_string(&result).expect("encode typed response");
        assert!(!encoded.contains("must-not-survive"));
        assert!(!encoded.contains("secretKey"));
        assert!(!encoded.contains("sessionToken"));
    }

    #[test]
    fn query_validation_rejects_empty_and_oversized_selectors() {
        let empty = PolicyEntitiesQuery {
            users: vec![" ".to_string()],
            ..Default::default()
        };
        assert!(matches!(empty.validate(), Err(Error::InvalidPath(_))));

        let oversized = PolicyEntitiesQuery {
            policies: vec!["p".repeat(MAX_IAM_POLICY_ENTITY_SELECTOR_BYTES + 1)],
            ..Default::default()
        };
        assert!(matches!(oversized.validate(), Err(Error::InvalidPath(_))));
    }

    #[test]
    fn query_validation_limits_aggregate_selector_count() {
        let query = PolicyEntitiesQuery {
            users: vec!["alice".to_string(); MAX_IAM_POLICY_ENTITY_SELECTORS + 1],
            ..Default::default()
        };
        assert!(matches!(query.validate(), Err(Error::InvalidPath(_))));
    }

    #[test]
    fn response_normalization_is_deterministic_and_rejects_empty_names() {
        let result: PolicyEntitiesResult = serde_json::from_str(
            r#"{
                "timestamp": "2026-07-24T08:00:00Z",
                "userMappings": [
                    {"user":"bob","policies":["write","read","read"]},
                    {"user":"alice","policies":["read"]}
                ],
                "groupMappings": [
                    {"group":"ops","policies":["write","read"]},
                    {"group":"dev","policies":["read"]}
                ],
                "policyMappings": [
                    {"policy":"write","users":["bob"],"groups":["ops"]},
                    {"policy":"read","users":["bob","alice","alice"],"groups":["ops","dev"]}
                ]
            }"#,
        )
        .expect("decode response");
        let normalized = result.normalize().expect("normalize response");

        assert_eq!(normalized.user_mappings[0].user, "alice");
        assert_eq!(
            normalized.user_mappings[1].policies,
            vec!["read".to_string(), "write".to_string()]
        );
        assert_eq!(normalized.group_mappings[0].group, "dev");
        assert_eq!(normalized.policy_mappings[0].policy, "read");
        assert_eq!(
            normalized.policy_mappings[0].users,
            vec!["alice".to_string(), "bob".to_string()]
        );

        let invalid: PolicyEntitiesResult = serde_json::from_str(
            r#"{
                "timestamp": "2026-07-24T08:00:00Z",
                "userMappings": [],
                "groupMappings": [],
                "policyMappings": [{"policy":" ","users":[],"groups":[]}]
            }"#,
        )
        .expect("decode invalid response");
        assert!(matches!(invalid.normalize(), Err(Error::General(_))));
    }

    #[test]
    fn detach_request_normalizes_policies_and_rejects_ambiguous_selectors() {
        let request = PolicyDetachRequest::new(
            vec!["write".to_string(), "read".to_string(), "write".to_string()],
            PolicyDetachEntity::User,
            "alice".to_string(),
        )
        .expect("valid detach request");
        assert_eq!(request.policies, ["read", "write"]);

        assert!(matches!(
            PolicyDetachRequest::new(
                vec!["read".to_string()],
                PolicyDetachEntity::Group,
                " ops ".to_string(),
            ),
            Err(Error::InvalidPath(_))
        ));
        assert!(matches!(
            PolicyDetachRequest::new(
                vec!["".to_string()],
                PolicyDetachEntity::User,
                "alice".to_string(),
            ),
            Err(Error::InvalidPath(_))
        ));
    }
}
