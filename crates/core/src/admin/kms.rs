//! Typed contracts for RustFS KMS administration.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::Result;

/// Runtime state of the RustFS KMS service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsServiceState {
    NotConfigured,
    Configured,
    Running,
    Error,
    Unknown,
}

/// Configured KMS backend family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsBackendKind {
    Local,
    VaultKv2,
    VaultTransit,
    Unknown,
}

/// Non-secret KMS cache configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsCacheSummary {
    pub enabled: bool,
    pub max_keys: Option<u64>,
    pub ttl_seconds: Option<u64>,
    pub metrics_enabled: Option<bool>,
}

/// Non-secret KMS configuration summary returned by RustFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsConfigSummary {
    pub backend: KmsBackendKind,
    pub default_key_id: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u32>,
    pub cache: KmsCacheSummary,
    pub endpoint: Option<String>,
    pub auth_method: Option<String>,
    pub credentials_configured: Option<bool>,
    pub tls_verification_disabled: Option<bool>,
}

/// KMS health and configuration state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsStatus {
    pub state: KmsServiceState,
    pub backend: Option<KmsBackendKind>,
    pub healthy: Option<bool>,
    pub error_message: Option<String>,
    pub config: Option<KmsConfigSummary>,
}

/// KMS key state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsKeyState {
    Enabled,
    Active,
    Disabled,
    PendingDeletion,
    PendingImport,
    Unavailable,
    Deleted,
    Unknown,
}

/// KMS key usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsKeyUsage {
    EncryptDecrypt,
    SignVerify,
    Unknown,
}

/// A normalized KMS key returned by list or describe operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsKey {
    pub key_id: String,
    pub state: KmsKeyState,
    pub usage: KmsKeyUsage,
    pub description: Option<String>,
    pub algorithm: Option<String>,
    pub version: Option<u32>,
    pub created_at: Option<String>,
    pub deletion_date: Option<String>,
    pub rotated_at: Option<String>,
    pub origin: Option<String>,
    pub manager: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// One page of KMS keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsKeyPage {
    pub keys: Vec<KmsKey>,
    pub truncated: bool,
    pub next_marker: Option<String>,
}

/// RustFS KMS administration operations.
#[async_trait]
pub trait KmsApi: Send + Sync {
    async fn kms_status(&self) -> Result<KmsStatus>;
    async fn kms_list_keys(&self, limit: u32, marker: Option<&str>) -> Result<KmsKeyPage>;
    async fn kms_describe_key(&self, key_id: &str) -> Result<KmsKey>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_readable_states_are_stable() {
        assert_eq!(
            serde_json::to_string(&KmsServiceState::NotConfigured)
                .expect("service state should serialize"),
            "\"not-configured\""
        );
        assert_eq!(
            serde_json::to_string(&KmsKeyState::PendingDeletion)
                .expect("key state should serialize"),
            "\"pending-deletion\""
        );
    }
}
