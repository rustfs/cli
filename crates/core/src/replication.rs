//! Bucket replication configuration types
//!
//! Domain types for S3 bucket replication configuration and
//! RustFS admin API remote target management.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Overall or per-target outcome of an active replication check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationCheckStatus {
    #[serde(rename = "OK")]
    Ok,
    #[serde(rename = "FAILED")]
    Failed,
}

/// Outcome of one phase in an active replication check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationCheckPhaseState {
    #[serde(rename = "OK")]
    Ok,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "SKIPPED")]
    Skipped,
}

/// One phase of the server's active target validation state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationCheckPhase {
    #[serde(rename = "Status")]
    pub status: ReplicationCheckPhaseState,
    #[serde(rename = "Error", default)]
    pub error: Option<String>,
}

/// All phases reported for one configured replication target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationCheckPhases {
    #[serde(rename = "Bucket")]
    pub bucket: ReplicationCheckPhase,
    #[serde(rename = "Versioning")]
    pub versioning: ReplicationCheckPhase,
    #[serde(rename = "ObjectLock")]
    pub object_lock: ReplicationCheckPhase,
    #[serde(rename = "Put")]
    pub put: ReplicationCheckPhase,
    #[serde(rename = "DeleteMarker")]
    pub delete_marker: ReplicationCheckPhase,
    #[serde(rename = "VersionDelete")]
    pub version_delete: ReplicationCheckPhase,
    #[serde(rename = "Cleanup")]
    pub cleanup: ReplicationCheckPhase,
}

/// Structured outcome for one configured replication target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationCheckTarget {
    #[serde(rename = "Arn")]
    pub target_arn: String,
    #[serde(rename = "Bucket")]
    pub bucket: String,
    #[serde(rename = "Status")]
    pub status: ReplicationCheckStatus,
    #[serde(rename = "Error", default)]
    pub error: Option<String>,
    #[serde(rename = "Phases")]
    pub phases: ReplicationCheckPhases,
}

/// Result of the active replication-target validation extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationCheckResult {
    /// Optional explicit contract version for forward-compatible servers.
    #[serde(rename = "Version", default, skip_serializing_if = "Option::is_none")]
    pub contract_version: Option<u32>,
    #[serde(rename = "Status")]
    pub status: ReplicationCheckStatus,
    #[serde(rename = "ActiveMutation")]
    pub active_mutation: bool,
    #[serde(rename = "MutationDescription")]
    pub mutation_description: String,
    #[serde(rename = "ProbeNamespace")]
    pub probe_namespace: String,
    #[serde(rename = "Targets")]
    pub targets: Vec<ReplicationCheckTarget>,
    /// True only for the successful empty-body contract used by older servers.
    #[serde(skip)]
    pub legacy_empty_response: bool,
}

impl ReplicationCheckResult {
    /// Represent the legacy server contract that returned no target detail.
    pub fn legacy_success() -> Self {
        Self {
            contract_version: None,
            status: ReplicationCheckStatus::Ok,
            active_mutation: true,
            mutation_description: "Legacy active write/delete replication probe".to_string(),
            probe_namespace: String::new(),
            targets: Vec::new(),
            legacy_empty_response: true,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.status == ReplicationCheckStatus::Ok
    }
}

/// Options for starting a RustFS bucket replication resync.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplicationResyncStartOptions {
    /// Select a configured replication target. The server may infer the only target.
    pub target_arn: Option<String>,
    /// Only resync objects older than this duration.
    pub older_than: Option<Duration>,
    /// Caller-supplied operation identifier. The server generates one when omitted.
    pub reset_id: Option<String>,
}

/// A target accepted by a bucket replication resync start request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationResyncStartResult {
    pub target_arn: String,
    pub reset_id: String,
}

/// Server state for a bucket replication resync target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationResyncState {
    NotStarted,
    Pending,
    Ongoing,
    Completed,
    Failed,
    Canceled,
    Unknown,
}

impl ReplicationResyncState {
    pub fn from_server(value: &str) -> Self {
        match value {
            "" => Self::NotStarted,
            "Pending" => Self::Pending,
            "Ongoing" => Self::Ongoing,
            "Completed" => Self::Completed,
            "Failed" => Self::Failed,
            "Canceled" => Self::Canceled,
            _ => Self::Unknown,
        }
    }
}

/// Status for one configured replication target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationResyncTargetStatus {
    pub target_arn: String,
    pub reset_id: String,
    pub reset_before: Option<jiff::Timestamp>,
    pub started_at: Option<jiff::Timestamp>,
    /// RustFS beta.10 calls this `EndTime`, but populates it from last-update time.
    pub last_updated_at: Option<jiff::Timestamp>,
    pub state: ReplicationResyncState,
    /// Exact state string returned by the server, including future values.
    pub server_state: String,
    pub replicated_count: u64,
    pub replicated_size: u64,
    pub failed_count: u64,
    pub failed_size: u64,
    pub current_bucket: Option<String>,
    pub current_object: Option<String>,
    /// Optional server detail. RustFS beta.10 does not reliably persist this field.
    pub error: Option<String>,
}

/// Current server-side bucket replication resync state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplicationResyncStatus {
    pub targets: Vec<ReplicationResyncTargetStatus>,
}

// ==================== S3 Replication Config Types ====================

/// Full replication configuration for a bucket (S3 API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationConfiguration {
    /// Role ARN or empty for per-rule destination ARNs
    #[serde(default)]
    pub role: String,

    /// Replication rules
    pub rules: Vec<ReplicationRule>,
}

/// A single replication rule
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationRule {
    /// Rule identifier
    pub id: String,

    /// Rule priority (higher = more important)
    pub priority: i32,

    /// Whether the rule is enabled or disabled
    pub status: ReplicationRuleStatus,

    /// Key prefix filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// Tag-based filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<std::collections::HashMap<String, String>>,

    /// Destination bucket ARN and optional storage class
    pub destination: ReplicationDestination,

    /// Whether to replicate delete markers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_marker_replication: Option<bool>,

    /// Whether to replicate existing objects
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_object_replication: Option<bool>,

    /// Whether to replicate version deletes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_replication: Option<bool>,
}

/// Rule status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationRuleStatus {
    Enabled,
    Disabled,
}

impl fmt::Display for ReplicationRuleStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplicationRuleStatus::Enabled => write!(f, "Enabled"),
            ReplicationRuleStatus::Disabled => write!(f, "Disabled"),
        }
    }
}

impl std::str::FromStr for ReplicationRuleStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "enabled" => Ok(ReplicationRuleStatus::Enabled),
            "disabled" => Ok(ReplicationRuleStatus::Disabled),
            _ => Err(format!("Invalid replication rule status: {s}")),
        }
    }
}

/// Replication destination
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationDestination {
    /// Destination bucket ARN
    pub bucket_arn: String,

    /// Optional storage class override at destination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
}

// ==================== Admin API Remote Target Types ====================

/// Remote bucket target for replication (matches RustFS admin API JSON format)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BucketTarget {
    #[serde(rename = "sourcebucket", default)]
    pub source_bucket: String,

    #[serde(default)]
    pub endpoint: String,

    #[serde(default)]
    pub credentials: Option<BucketTargetCredentials>,

    #[serde(rename = "targetbucket", default)]
    pub target_bucket: String,

    #[serde(default)]
    pub secure: bool,

    #[serde(
        rename = "skipTlsVerify",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub skip_tls_verify: Option<bool>,

    #[serde(rename = "caCertPem", default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_pem: Option<String>,

    #[serde(default)]
    pub path: String,

    #[serde(default)]
    pub api: String,

    #[serde(default)]
    pub arn: String,

    #[serde(rename = "type", default)]
    pub target_type: String,

    #[serde(default)]
    pub region: String,

    #[serde(alias = "bandwidth", default)]
    pub bandwidth_limit: i64,

    #[serde(rename = "replicationSync", default)]
    pub replication_sync: bool,

    #[serde(default)]
    pub storage_class: String,

    #[serde(rename = "healthCheckDuration", default)]
    pub health_check_duration: u64,

    #[serde(rename = "disableProxy", default)]
    pub disable_proxy: bool,

    #[serde(rename = "isOnline", default)]
    pub online: bool,
}

/// Credentials for a remote bucket target
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BucketTargetCredentials {
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
}

impl fmt::Display for ReplicationRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (priority={}, {})",
            self.id, self.priority, self.status
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replication_rule_status_display() {
        assert_eq!(ReplicationRuleStatus::Enabled.to_string(), "Enabled");
        assert_eq!(ReplicationRuleStatus::Disabled.to_string(), "Disabled");
    }

    #[test]
    fn test_replication_rule_status_from_str() {
        assert_eq!(
            "enabled".parse::<ReplicationRuleStatus>().unwrap(),
            ReplicationRuleStatus::Enabled
        );
        assert!("invalid".parse::<ReplicationRuleStatus>().is_err());
    }

    #[test]
    fn resync_state_preserves_empty_and_future_wire_values() {
        assert_eq!(
            ReplicationResyncState::from_server("Ongoing"),
            ReplicationResyncState::Ongoing
        );
        assert_eq!(
            ReplicationResyncState::from_server(""),
            ReplicationResyncState::NotStarted
        );
        assert_eq!(
            ReplicationResyncState::from_server("FutureState"),
            ReplicationResyncState::Unknown
        );
    }

    #[test]
    fn resync_start_options_distinguish_server_and_caller_ids() {
        let generated = ReplicationResyncStartOptions {
            target_arn: Some("arn:rustfs:replication::id:dest".to_string()),
            older_than: Some(Duration::from_secs(3600)),
            reset_id: None,
        };
        let explicit = ReplicationResyncStartOptions {
            reset_id: Some("reset-1".to_string()),
            ..generated.clone()
        };

        assert_eq!(generated.reset_id, None);
        assert_eq!(explicit.reset_id.as_deref(), Some("reset-1"));
    }

    #[test]
    fn test_replication_configuration_serialization() {
        let config = ReplicationConfiguration {
            role: "arn:aws:iam::123456789:role/replication".to_string(),
            rules: vec![ReplicationRule {
                id: "rule-1".to_string(),
                priority: 1,
                status: ReplicationRuleStatus::Enabled,
                prefix: Some("data/".to_string()),
                tags: None,
                destination: ReplicationDestination {
                    bucket_arn: "arn:aws:s3:::dest-bucket".to_string(),
                    storage_class: None,
                },
                delete_marker_replication: Some(true),
                existing_object_replication: Some(true),
                delete_replication: None,
            }],
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let decoded: ReplicationConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.rules.len(), 1);
        assert_eq!(decoded.rules[0].id, "rule-1");
        assert_eq!(decoded.rules[0].priority, 1);
    }

    #[test]
    fn test_bucket_target_serialization() {
        let target = BucketTarget {
            source_bucket: "my-bucket".to_string(),
            endpoint: "http://remote:9000".to_string(),
            credentials: Some(BucketTargetCredentials {
                access_key: "admin".to_string(),
                secret_key: "secret".to_string(),
            }),
            target_bucket: "dest-bucket".to_string(),
            secure: false,
            skip_tls_verify: Some(true),
            target_type: "replication".to_string(),
            region: "us-east-1".to_string(),
            replication_sync: true,
            ..Default::default()
        };

        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("sourcebucket"));
        assert!(json.contains("targetbucket"));
        assert!(json.contains("replicationSync"));
        assert!(json.contains("skipTlsVerify"));

        let decoded: BucketTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.source_bucket, "my-bucket");
        assert_eq!(decoded.target_bucket, "dest-bucket");
        assert!(decoded.replication_sync);
        assert_eq!(decoded.skip_tls_verify, Some(true));
    }

    #[test]
    fn test_bucket_target_deserialization_from_backend() {
        let json = r#"{"sourcebucket":"src","endpoint":"http://host:9000","credentials":{"accessKey":"ak","secretKey":"sk"},"targetbucket":"dst","secure":false,"path":"","api":"","arn":"arn:rustfs:replication::id:dst","type":"replication","region":"","bandwidth":0,"replicationSync":false,"storage_class":"","healthCheckDuration":0,"disableProxy":false,"isOnline":true}"#;
        let target: BucketTarget = serde_json::from_str(json).unwrap();
        assert_eq!(target.source_bucket, "src");
        assert_eq!(target.target_bucket, "dst");
        assert!(target.online);
        assert_eq!(target.target_type, "replication");
    }

    #[test]
    fn test_bucket_target_serialization_includes_ca_cert_pem_content() {
        let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        let target = BucketTarget {
            source_bucket: "my-bucket".to_string(),
            endpoint: "remote:9000".to_string(),
            target_bucket: "dest-bucket".to_string(),
            secure: true,
            skip_tls_verify: Some(false),
            ca_cert_pem: Some(pem.to_string()),
            target_type: "replication".to_string(),
            ..Default::default()
        };

        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"skipTlsVerify\":false"));
        assert!(json.contains("caCertPem"));
        assert!(!json.contains("ca.pem"));
    }
}
