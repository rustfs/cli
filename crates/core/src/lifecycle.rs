//! Lifecycle (ILM) configuration types
//!
//! Domain types for S3 bucket lifecycle rules including expiration,
//! transition, and noncurrent version management.

use std::collections::HashMap;
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

/// Full lifecycle configuration for a bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleConfiguration {
    /// Lifecycle rules
    pub rules: Vec<LifecycleRule>,
}

/// A single lifecycle rule
#[derive(Debug, Clone, Serialize)]
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

    /// Expire delete-marker history after the configured number of days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub del_marker_expiration: Option<LifecycleDelMarkerExpiration>,

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleRuleInput {
    #[serde(alias = "ID")]
    id: String,
    #[serde(alias = "Status")]
    status: LifecycleRuleStatus,
    #[serde(default, alias = "Prefix")]
    prefix: Option<String>,
    #[serde(default, alias = "Tags")]
    tags: Option<HashMap<String, String>>,
    #[serde(default, alias = "Expiration")]
    expiration: Option<LifecycleExpirationInput>,
    #[serde(default, alias = "Transition")]
    transition: Option<LifecycleTransition>,
    #[serde(default, alias = "NoncurrentVersionExpiration")]
    noncurrent_version_expiration: Option<NoncurrentVersionExpiration>,
    #[serde(default, alias = "NoncurrentVersionTransition")]
    noncurrent_version_transition: Option<NoncurrentVersionTransition>,
    #[serde(default, alias = "AbortIncompleteMultipartUploadDays")]
    abort_incomplete_multipart_upload_days: Option<i32>,
    #[serde(
        default,
        alias = "ExpiredObjectDeleteMarker",
        alias = "expired_object_delete_marker"
    )]
    expired_object_delete_marker: Option<bool>,
    #[serde(
        default,
        alias = "DelMarkerExpiration",
        alias = "del_marker_expiration"
    )]
    del_marker_expiration: Option<LifecycleDelMarkerInput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleExpirationInput {
    #[serde(default, alias = "Days")]
    days: Option<i32>,
    #[serde(default, alias = "Date")]
    date: Option<String>,
    #[serde(
        default,
        alias = "ExpiredObjectAllVersions",
        alias = "expired_object_all_versions"
    )]
    expired_object_all_versions: Option<bool>,
    #[serde(
        default,
        alias = "DelMarkerExpiration",
        alias = "del_marker_expiration"
    )]
    del_marker_expiration: Option<LifecycleDelMarkerInput>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LifecycleDelMarkerInput {
    Flag(bool),
    Configuration(LifecycleDelMarkerExpiration),
}

impl<'de> Deserialize<'de> for LifecycleRule {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = LifecycleRuleInput::deserialize(deserializer)?;
        let (expiration, nested_del_marker) = match input.expiration {
            Some(expiration) => {
                let nested_del_marker = expiration.del_marker_expiration;
                (
                    Some(LifecycleExpiration {
                        days: expiration.days,
                        date: expiration.date,
                        expired_object_all_versions: expiration.expired_object_all_versions,
                    }),
                    nested_del_marker,
                )
            }
            None => (None, None),
        };

        let fallback_days = expiration.as_ref().and_then(|expiration| expiration.days);
        let top_level_del_marker =
            normalize_del_marker_input(input.del_marker_expiration, fallback_days)
                .map_err(D::Error::custom)?;
        let nested_del_marker = normalize_del_marker_input(nested_del_marker, fallback_days)
            .map_err(D::Error::custom)?;

        let del_marker_expiration = match (top_level_del_marker, nested_del_marker) {
            (Some(top), Some(nested)) if top.days != nested.days => {
                return Err(D::Error::custom(
                    "conflicting DelMarkerExpiration values in lifecycle rule",
                ));
            }
            (Some(top), _) => Some(top),
            (_, Some(nested)) => Some(nested),
            (None, None) => None,
        };

        Ok(Self {
            id: input.id,
            status: input.status,
            prefix: input.prefix,
            tags: input.tags,
            expiration,
            del_marker_expiration,
            transition: input.transition,
            noncurrent_version_expiration: input.noncurrent_version_expiration,
            noncurrent_version_transition: input.noncurrent_version_transition,
            abort_incomplete_multipart_upload_days: input.abort_incomplete_multipart_upload_days,
            expired_object_delete_marker: input.expired_object_delete_marker,
        })
    }
}

fn normalize_del_marker_input(
    input: Option<LifecycleDelMarkerInput>,
    fallback_days: Option<i32>,
) -> std::result::Result<Option<LifecycleDelMarkerExpiration>, String> {
    match input {
        None => Ok(None),
        Some(LifecycleDelMarkerInput::Flag(false)) => Ok(None),
        Some(LifecycleDelMarkerInput::Flag(true)) => fallback_days
            .map(|days| Some(LifecycleDelMarkerExpiration { days: Some(days) }))
            .ok_or_else(|| {
                "DelMarkerExpiration=true requires expiration.days for compatibility input"
                    .to_string()
            }),
        Some(LifecycleDelMarkerInput::Configuration(configuration)) => Ok(Some(configuration)),
    }
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
#[serde(rename_all = "camelCase")]
pub struct LifecycleExpiration {
    /// Number of days after creation to expire
    #[serde(skip_serializing_if = "Option::is_none", alias = "Days")]
    pub days: Option<i32>,

    /// Specific date to expire (ISO 8601 format)
    #[serde(skip_serializing_if = "Option::is_none", alias = "Date")]
    pub date: Option<String>,

    /// Whether all object versions should be expired together.
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "ExpiredObjectAllVersions",
        alias = "expired_object_all_versions"
    )]
    pub expired_object_all_versions: Option<bool>,
}

/// Expiration settings for delete markers and their prior object versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleDelMarkerExpiration {
    /// Number of days before delete-marker history is removed.
    #[serde(skip_serializing_if = "Option::is_none", alias = "Days")]
    pub days: Option<i32>,
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
                expired_object_all_versions: None,
            }),
            del_marker_expiration: None,
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
                    expired_object_all_versions: None,
                }),
                del_marker_expiration: None,
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

    #[test]
    fn test_lifecycle_extensions_use_canonical_json_shape() {
        let config = LifecycleConfiguration {
            rules: vec![LifecycleRule {
                id: "all-versions".to_string(),
                status: LifecycleRuleStatus::Enabled,
                prefix: Some("test/".to_string()),
                tags: None,
                expiration: Some(LifecycleExpiration {
                    days: Some(1),
                    date: None,
                    expired_object_all_versions: Some(true),
                }),
                del_marker_expiration: Some(LifecycleDelMarkerExpiration { days: Some(1) }),
                transition: None,
                noncurrent_version_expiration: None,
                noncurrent_version_transition: None,
                abort_incomplete_multipart_upload_days: None,
                expired_object_delete_marker: None,
            }],
        };

        let value = serde_json::to_value(&config).expect("serialize lifecycle extensions");
        assert_eq!(
            value["rules"][0]["expiration"]["expiredObjectAllVersions"],
            true
        );
        assert_eq!(value["rules"][0]["delMarkerExpiration"]["days"], 1);
        assert!(
            value["rules"][0]["expiration"]
                .get("DelMarkerExpiration")
                .is_none()
        );
        let decoded: LifecycleConfiguration =
            serde_json::from_value(value).expect("canonical lifecycle extensions should parse");
        assert_eq!(
            decoded.rules[0]
                .expiration
                .as_ref()
                .and_then(|expiration| expiration.expired_object_all_versions),
            Some(true)
        );
        assert_eq!(
            decoded.rules[0]
                .del_marker_expiration
                .as_ref()
                .and_then(|expiration| expiration.days),
            Some(1)
        );
    }

    #[test]
    fn test_lifecycle_extensions_accept_issue_6334_compatibility_shape() {
        let input = r#"
        {
          "rules": [{
            "id": "rule-delayed-deletion",
            "status": "Enabled",
            "prefix": "test/",
            "expiration": {
              "ExpiredObjectAllVersions": true,
              "DelMarkerExpiration": true,
              "days": 1
            }
          }]
        }
        "#;

        let config: LifecycleConfiguration =
            serde_json::from_str(input).expect("issue compatibility shape should parse");
        let rule = &config.rules[0];
        assert_eq!(
            rule.expiration
                .as_ref()
                .and_then(|expiration| expiration.expired_object_all_versions),
            Some(true)
        );
        assert_eq!(
            rule.del_marker_expiration
                .as_ref()
                .and_then(|expiration| expiration.days),
            Some(1)
        );
    }

    #[test]
    fn test_lifecycle_extensions_reject_ambiguous_compatibility_shape() {
        let input = r#"
        {
          "rules": [{
            "id": "conflicting",
            "status": "Enabled",
            "expiration": {"days": 1, "DelMarkerExpiration": true},
            "delMarkerExpiration": {"days": 2}
          }]
        }
        "#;

        let result = serde_json::from_str::<LifecycleConfiguration>(input);
        assert!(result.is_err());
    }
}
