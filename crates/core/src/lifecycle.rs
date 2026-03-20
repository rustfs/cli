//! Lifecycle (ILM) configuration types
//!
//! Domain types for S3 bucket lifecycle rules including expiration,
//! transition, and noncurrent version management.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Full lifecycle configuration for a bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleConfiguration {
    /// Lifecycle rules
    pub rules: Vec<LifecycleRule>,
}

/// A single lifecycle rule
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleRule {
    /// Rule identifier
    pub id: String,

    /// Whether the rule is enabled or disabled
    pub status: LifecycleRuleStatus,

    /// Key prefix filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// Tag-based filter (key=value pairs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<HashMap<String, String>>,

    /// Expiration settings for current object versions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<LifecycleExpiration>,

    /// Transition settings for current object versions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<LifecycleTransition>,

    /// Expiration settings for noncurrent object versions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noncurrent_version_expiration: Option<NoncurrentVersionExpiration>,

    /// Transition settings for noncurrent object versions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noncurrent_version_transition: Option<NoncurrentVersionTransition>,

    /// Days after initiation to abort incomplete multipart uploads
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_incomplete_multipart_upload_days: Option<i32>,

    /// Whether to remove expired delete markers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired_object_delete_marker: Option<bool>,
}

/// Rule status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecycleRuleStatus {
    Enabled,
    Disabled,
}

impl fmt::Display for LifecycleRuleStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LifecycleRuleStatus::Enabled => write!(f, "Enabled"),
            LifecycleRuleStatus::Disabled => write!(f, "Disabled"),
        }
    }
}

impl std::str::FromStr for LifecycleRuleStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "enabled" => Ok(LifecycleRuleStatus::Enabled),
            "disabled" => Ok(LifecycleRuleStatus::Disabled),
            _ => Err(format!("Invalid lifecycle rule status: {s}")),
        }
    }
}

/// Expiration settings for current object versions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleExpiration {
    /// Number of days after creation to expire
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<i32>,

    /// Specific date to expire (ISO 8601 format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

/// Transition settings for current object versions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleTransition {
    /// Number of days after creation to transition
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<i32>,

    /// Specific date to transition (ISO 8601 format)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,

    /// Target storage class (tier name)
    pub storage_class: String,
}

/// Expiration settings for noncurrent object versions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoncurrentVersionExpiration {
    /// Number of days after becoming noncurrent to expire
    pub noncurrent_days: i32,

    /// Maximum number of noncurrent versions to retain
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newer_noncurrent_versions: Option<i32>,
}

/// Transition settings for noncurrent object versions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoncurrentVersionTransition {
    /// Number of days after becoming noncurrent to transition
    pub noncurrent_days: i32,

    /// Target storage class (tier name)
    pub storage_class: String,
}

impl fmt::Display for LifecycleRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.id, self.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lifecycle_rule_status_display() {
        assert_eq!(LifecycleRuleStatus::Enabled.to_string(), "Enabled");
        assert_eq!(LifecycleRuleStatus::Disabled.to_string(), "Disabled");
    }

    #[test]
    fn test_lifecycle_rule_status_from_str() {
        assert_eq!(
            "enabled".parse::<LifecycleRuleStatus>().unwrap(),
            LifecycleRuleStatus::Enabled
        );
        assert_eq!(
            "Disabled".parse::<LifecycleRuleStatus>().unwrap(),
            LifecycleRuleStatus::Disabled
        );
        assert!("invalid".parse::<LifecycleRuleStatus>().is_err());
    }

    #[test]
    fn test_lifecycle_rule_serialization() {
        let rule = LifecycleRule {
            id: "rule-1".to_string(),
            status: LifecycleRuleStatus::Enabled,
            prefix: Some("logs/".to_string()),
            tags: None,
            expiration: Some(LifecycleExpiration {
                days: Some(30),
                date: None,
            }),
            transition: None,
            noncurrent_version_expiration: None,
            noncurrent_version_transition: None,
            abort_incomplete_multipart_upload_days: Some(7),
            expired_object_delete_marker: None,
        };

        let json = serde_json::to_string(&rule).unwrap();
        let decoded: LifecycleRule = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "rule-1");
        assert_eq!(decoded.status, LifecycleRuleStatus::Enabled);
        assert_eq!(decoded.prefix.as_deref(), Some("logs/"));
        assert_eq!(decoded.expiration.as_ref().unwrap().days, Some(30));
        assert_eq!(decoded.abort_incomplete_multipart_upload_days, Some(7));
    }

    #[test]
    fn test_lifecycle_transition_serialization() {
        let transition = LifecycleTransition {
            days: Some(90),
            date: None,
            storage_class: "WARM_TIER".to_string(),
        };

        let json = serde_json::to_string(&transition).unwrap();
        assert!(json.contains("storageClass"));
        assert!(json.contains("WARM_TIER"));

        let decoded: LifecycleTransition = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.storage_class, "WARM_TIER");
    }

    #[test]
    fn test_lifecycle_configuration_serialization() {
        let config = LifecycleConfiguration {
            rules: vec![LifecycleRule {
                id: "expire-old".to_string(),
                status: LifecycleRuleStatus::Enabled,
                prefix: None,
                tags: None,
                expiration: Some(LifecycleExpiration {
                    days: Some(365),
                    date: None,
                }),
                transition: None,
                noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                    noncurrent_days: 30,
                    newer_noncurrent_versions: Some(3),
                }),
                noncurrent_version_transition: None,
                abort_incomplete_multipart_upload_days: None,
                expired_object_delete_marker: Some(true),
            }],
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let decoded: LifecycleConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.rules.len(), 1);
        assert_eq!(decoded.rules[0].id, "expire-old");
        assert_eq!(
            decoded.rules[0]
                .noncurrent_version_expiration
                .as_ref()
                .unwrap()
                .newer_noncurrent_versions,
            Some(3)
        );
    }

    #[test]
    fn test_noncurrent_version_transition_serialization() {
        let nvt = NoncurrentVersionTransition {
            noncurrent_days: 60,
            storage_class: "COLD_TIER".to_string(),
        };

        let json = serde_json::to_string(&nvt).unwrap();
        let decoded: NoncurrentVersionTransition = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.noncurrent_days, 60);
        assert_eq!(decoded.storage_class, "COLD_TIER");
    }
}
