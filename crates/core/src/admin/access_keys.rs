//! Typed, secret-free contracts for bulk access-key inspection.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{Error, Result};

/// Capability for the builtin bulk access-key route.
pub const IAM_ACCESS_KEYS_BULK_CAPABILITY: &str = "admin.iam.access-keys-bulk";
/// Capability for the LDAP-scoped bulk access-key route.
pub const IAM_ACCESS_KEYS_BULK_LDAP_CAPABILITY: &str = "admin.iam.access-keys-bulk.ldap";
/// Capability for the OpenID-scoped bulk access-key route.
pub const IAM_ACCESS_KEYS_BULK_OPENID_CAPABILITY: &str = "admin.iam.access-keys-bulk.openid";

/// Maximum encoded response accepted from one bulk route.
pub const MAX_IAM_ACCESS_KEYS_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum selectors accepted by one logical command.
pub const MAX_IAM_ACCESS_KEY_SELECTORS: usize = 1_000;
/// Maximum UTF-8 byte length of one parent selector.
pub const MAX_IAM_ACCESS_KEY_SELECTOR_BYTES: usize = 1_024;
/// Maximum number of decoded keys accepted from one server response.
pub const MAX_IAM_ACCESS_KEY_RESULTS: usize = 10_000;

/// Identity provider owning an access key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessKeyProvider {
    #[default]
    Builtin,
    Ldap,
    Openid,
}

impl AccessKeyProvider {
    /// Capability that must be available before this provider route is called.
    pub const fn capability(self) -> &'static str {
        match self {
            Self::Builtin => IAM_ACCESS_KEYS_BULK_CAPABILITY,
            Self::Ldap => IAM_ACCESS_KEYS_BULK_LDAP_CAPABILITY,
            Self::Openid => IAM_ACCESS_KEYS_BULK_OPENID_CAPABILITY,
        }
    }
}

impl fmt::Display for AccessKeyProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Builtin => "builtin",
            Self::Ldap => "ldap",
            Self::Openid => "openid",
        })
    }
}

/// Access-key class represented by a RustFS bulk response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessKeyKind {
    ServiceAccount,
    Sts,
}

impl fmt::Display for AccessKeyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ServiceAccount => "service-account",
            Self::Sts => "sts",
        })
    }
}

/// Server-side access-key class filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccessKeyListType {
    #[default]
    All,
    UsersOnly,
    StsOnly,
    ServiceAccountsOnly,
}

impl AccessKeyListType {
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::UsersOnly => "users-only",
            Self::StsOnly => "sts-only",
            Self::ServiceAccountsOnly => "svcacc-only",
        }
    }
}

/// One bounded bulk-list request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BulkAccessKeyQuery {
    pub provider: AccessKeyProvider,
    pub users: Vec<String>,
    pub all: bool,
    pub list_type: AccessKeyListType,
}

impl BulkAccessKeyQuery {
    pub fn validate(&self) -> Result<()> {
        if self.all && !self.users.is_empty() {
            return Err(Error::InvalidPath(
                "bulk access-key inspection accepts either --all or --user, not both".to_string(),
            ));
        }
        if self.users.len() > MAX_IAM_ACCESS_KEY_SELECTORS {
            return Err(Error::InvalidPath(format!(
                "bulk access-key inspection accepts at most {MAX_IAM_ACCESS_KEY_SELECTORS} user selectors"
            )));
        }
        for user in &self.users {
            if user.trim().is_empty() {
                return Err(Error::InvalidPath(
                    "bulk access-key user selector cannot be empty".to_string(),
                ));
            }
            if user.len() > MAX_IAM_ACCESS_KEY_SELECTOR_BYTES {
                return Err(Error::InvalidPath(format!(
                    "bulk access-key user selector exceeds {MAX_IAM_ACCESS_KEY_SELECTOR_BYTES} bytes"
                )));
            }
        }
        Ok(())
    }
}

/// A safe projection of one key returned by RustFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKeyRecord {
    pub access_key: String,
    pub kind: AccessKeyKind,
    pub provider: AccessKeyProvider,
    pub parent_user: String,
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

/// Typed access-key bulk operations.
#[async_trait]
pub trait BulkAccessKeyApi: Send + Sync {
    async fn list_access_keys_bulk(
        &self,
        query: &BulkAccessKeyQuery,
    ) -> Result<Vec<AccessKeyRecord>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_validation_rejects_conflicting_empty_and_oversized_selectors() {
        let conflict = BulkAccessKeyQuery {
            users: vec!["alice".to_string()],
            all: true,
            ..Default::default()
        };
        assert!(matches!(conflict.validate(), Err(Error::InvalidPath(_))));

        let empty = BulkAccessKeyQuery {
            users: vec![" ".to_string()],
            ..Default::default()
        };
        assert!(matches!(empty.validate(), Err(Error::InvalidPath(_))));

        let oversized = BulkAccessKeyQuery {
            users: vec!["u".repeat(MAX_IAM_ACCESS_KEY_SELECTOR_BYTES + 1)],
            ..Default::default()
        };
        assert!(matches!(oversized.validate(), Err(Error::InvalidPath(_))));
    }

    #[test]
    fn provider_capability_mapping_is_stable() {
        assert_eq!(
            AccessKeyProvider::Builtin.capability(),
            IAM_ACCESS_KEYS_BULK_CAPABILITY
        );
        assert_eq!(
            AccessKeyProvider::Ldap.capability(),
            IAM_ACCESS_KEYS_BULK_LDAP_CAPABILITY
        );
        assert_eq!(
            AccessKeyProvider::Openid.capability(),
            IAM_ACCESS_KEYS_BULK_OPENID_CAPABILITY
        );
    }
}
