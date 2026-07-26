//! Tier configuration types for remote storage tiering
//!
//! These types match the RustFS admin API JSON format for tier management.
//! Tiers are used by lifecycle transition rules to move objects to
//! remote storage backends.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Supported remote storage tier types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TierType {
    #[serde(rename = "s3")]
    S3,
    #[serde(rename = "rustfs")]
    RustFS,
    #[serde(rename = "minio")]
    MinIO,
    #[serde(rename = "aliyun")]
    Aliyun,
    #[serde(rename = "tencent")]
    Tencent,
    #[serde(rename = "huaweicloud")]
    Huaweicloud,
    #[serde(rename = "azure")]
    Azure,
    #[serde(rename = "gcs")]
    GCS,
    #[serde(rename = "r2")]
    R2,
}

impl fmt::Display for TierType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TierType::S3 => write!(f, "S3"),
            TierType::RustFS => write!(f, "RustFS"),
            TierType::MinIO => write!(f, "MinIO"),
            TierType::Aliyun => write!(f, "Aliyun"),
            TierType::Tencent => write!(f, "Tencent"),
            TierType::Huaweicloud => write!(f, "Huaweicloud"),
            TierType::Azure => write!(f, "Azure"),
            TierType::GCS => write!(f, "GCS"),
            TierType::R2 => write!(f, "R2"),
        }
    }
}

impl std::str::FromStr for TierType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "s3" => Ok(TierType::S3),
            "rustfs" => Ok(TierType::RustFS),
            "minio" => Ok(TierType::MinIO),
            "aliyun" => Ok(TierType::Aliyun),
            "tencent" => Ok(TierType::Tencent),
            "huaweicloud" => Ok(TierType::Huaweicloud),
            "azure" => Ok(TierType::Azure),
            "gcs" => Ok(TierType::GCS),
            "r2" => Ok(TierType::R2),
            _ => Err(format!(
                "Invalid tier type: {s}. Valid types: s3, rustfs, minio, aliyun, tencent, huaweicloud, azure, gcs, r2"
            )),
        }
    }
}

/// Tier configuration matching the RustFS admin API format.
///
/// The backend uses a polymorphic structure: the `type` field selects which
/// sub-config (s3, rustfs, minio, etc.) is active. The tier name lives
/// inside the sub-config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TierConfig {
    #[serde(rename = "type")]
    pub tier_type: TierType,

    /// Tier name — extracted from the active sub-config on the backend side.
    /// Populated by the CLI when building a TierConfig for add operations.
    #[serde(skip)]
    pub name: String,

    #[serde(rename = "s3", skip_serializing_if = "Option::is_none")]
    pub s3: Option<TierS3>,
    #[serde(rename = "rustfs", skip_serializing_if = "Option::is_none")]
    pub rustfs: Option<TierRustFS>,
    #[serde(rename = "minio", skip_serializing_if = "Option::is_none")]
    pub minio: Option<TierMinIO>,
    #[serde(rename = "aliyun", skip_serializing_if = "Option::is_none")]
    pub aliyun: Option<TierAliyun>,
    #[serde(rename = "tencent", skip_serializing_if = "Option::is_none")]
    pub tencent: Option<TierTencent>,
    #[serde(rename = "huaweicloud", skip_serializing_if = "Option::is_none")]
    pub huaweicloud: Option<TierHuaweicloud>,
    #[serde(rename = "azure", skip_serializing_if = "Option::is_none")]
    pub azure: Option<TierAzure>,
    #[serde(rename = "gcs", skip_serializing_if = "Option::is_none")]
    pub gcs: Option<TierGCS>,
    #[serde(rename = "r2", skip_serializing_if = "Option::is_none")]
    pub r2: Option<TierR2>,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            tier_type: TierType::S3,
            name: String::new(),
            s3: None,
            rustfs: None,
            minio: None,
            aliyun: None,
            tencent: None,
            huaweicloud: None,
            azure: None,
            gcs: None,
            r2: None,
        }
    }
}

impl TierConfig {
    /// Get the tier name from the active sub-config
    pub fn tier_name(&self) -> &str {
        if !self.name.is_empty() {
            return &self.name;
        }
        match self.tier_type {
            TierType::S3 => self.s3.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::RustFS => self.rustfs.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::MinIO => self.minio.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::Aliyun => self.aliyun.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::Tencent => self.tencent.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::Huaweicloud => self
                .huaweicloud
                .as_ref()
                .map(|c| c.name.as_str())
                .unwrap_or(""),
            TierType::Azure => self.azure.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::GCS => self.gcs.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
            TierType::R2 => self.r2.as_ref().map(|c| c.name.as_str()).unwrap_or(""),
        }
    }

    /// Get the endpoint from the active sub-config
    pub fn endpoint(&self) -> &str {
        match self.tier_type {
            TierType::S3 => self.s3.as_ref().map(|c| c.endpoint.as_str()).unwrap_or(""),
            TierType::RustFS => self
                .rustfs
                .as_ref()
                .map(|c| c.endpoint.as_str())
                .unwrap_or(""),
            TierType::MinIO => self
                .minio
                .as_ref()
                .map(|c| c.endpoint.as_str())
                .unwrap_or(""),
            TierType::Aliyun => self
                .aliyun
                .as_ref()
                .map(|c| c.endpoint.as_str())
                .unwrap_or(""),
            TierType::Tencent => self
                .tencent
                .as_ref()
                .map(|c| c.endpoint.as_str())
                .unwrap_or(""),
            TierType::Huaweicloud => self
                .huaweicloud
                .as_ref()
                .map(|c| c.endpoint.as_str())
                .unwrap_or(""),
            TierType::Azure => self
                .azure
                .as_ref()
                .map(|c| c.endpoint.as_str())
                .unwrap_or(""),
            TierType::GCS => self.gcs.as_ref().map(|c| c.endpoint.as_str()).unwrap_or(""),
            TierType::R2 => self.r2.as_ref().map(|c| c.endpoint.as_str()).unwrap_or(""),
        }
    }

    /// Get the bucket from the active sub-config
    pub fn bucket(&self) -> &str {
        match self.tier_type {
            TierType::S3 => self.s3.as_ref().map(|c| c.bucket.as_str()).unwrap_or(""),
            TierType::RustFS => self
                .rustfs
                .as_ref()
                .map(|c| c.bucket.as_str())
                .unwrap_or(""),
            TierType::MinIO => self.minio.as_ref().map(|c| c.bucket.as_str()).unwrap_or(""),
            TierType::Aliyun => self
                .aliyun
                .as_ref()
                .map(|c| c.bucket.as_str())
                .unwrap_or(""),
            TierType::Tencent => self
                .tencent
                .as_ref()
                .map(|c| c.bucket.as_str())
                .unwrap_or(""),
            TierType::Huaweicloud => self
                .huaweicloud
                .as_ref()
                .map(|c| c.bucket.as_str())
                .unwrap_or(""),
            TierType::Azure => self.azure.as_ref().map(|c| c.bucket.as_str()).unwrap_or(""),
            TierType::GCS => self.gcs.as_ref().map(|c| c.bucket.as_str()).unwrap_or(""),
            TierType::R2 => self.r2.as_ref().map(|c| c.bucket.as_str()).unwrap_or(""),
        }
    }

    /// Get the prefix from the active sub-config
    pub fn prefix(&self) -> &str {
        match self.tier_type {
            TierType::S3 => self.s3.as_ref().map(|c| c.prefix.as_str()).unwrap_or(""),
            TierType::RustFS => self
                .rustfs
                .as_ref()
                .map(|c| c.prefix.as_str())
                .unwrap_or(""),
            TierType::MinIO => self.minio.as_ref().map(|c| c.prefix.as_str()).unwrap_or(""),
            TierType::Aliyun => self
                .aliyun
                .as_ref()
                .map(|c| c.prefix.as_str())
                .unwrap_or(""),
            TierType::Tencent => self
                .tencent
                .as_ref()
                .map(|c| c.prefix.as_str())
                .unwrap_or(""),
            TierType::Huaweicloud => self
                .huaweicloud
                .as_ref()
                .map(|c| c.prefix.as_str())
                .unwrap_or(""),
            TierType::Azure => self.azure.as_ref().map(|c| c.prefix.as_str()).unwrap_or(""),
            TierType::GCS => self.gcs.as_ref().map(|c| c.prefix.as_str()).unwrap_or(""),
            TierType::R2 => self.r2.as_ref().map(|c| c.prefix.as_str()).unwrap_or(""),
        }
    }

    /// Get the region from the active sub-config
    pub fn region(&self) -> &str {
        match self.tier_type {
            TierType::S3 => self.s3.as_ref().map(|c| c.region.as_str()).unwrap_or(""),
            TierType::RustFS => self
                .rustfs
                .as_ref()
                .map(|c| c.region.as_str())
                .unwrap_or(""),
            TierType::MinIO => self.minio.as_ref().map(|c| c.region.as_str()).unwrap_or(""),
            TierType::Aliyun => self
                .aliyun
                .as_ref()
                .map(|c| c.region.as_str())
                .unwrap_or(""),
            TierType::Tencent => self
                .tencent
                .as_ref()
                .map(|c| c.region.as_str())
                .unwrap_or(""),
            TierType::Huaweicloud => self
                .huaweicloud
                .as_ref()
                .map(|c| c.region.as_str())
                .unwrap_or(""),
            TierType::Azure => self.azure.as_ref().map(|c| c.region.as_str()).unwrap_or(""),
            TierType::GCS => self.gcs.as_ref().map(|c| c.region.as_str()).unwrap_or(""),
            TierType::R2 => self.r2.as_ref().map(|c| c.region.as_str()).unwrap_or(""),
        }
    }
}

/// Credentials for updating a tier
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TierCreds {
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
}

/// Request for a bounded manual lifecycle transition run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualTransitionRunRequest {
    pub bucket: String,
    pub prefix: String,
    pub tier: Option<String>,
    pub dry_run: bool,
    pub max_objects: u64,
    pub max_duration_seconds: Option<u64>,
}

/// Response returned by the manual lifecycle transition endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualTransitionRunResponse {
    pub state: String,
    pub mode: String,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub status_endpoint: Option<String>,
    #[serde(default)]
    pub cancel_endpoint: Option<String>,
    pub report: ManualTransitionRunReport,
}

/// Response returned when inspecting or cancelling a durable manual transition job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualTransitionJobResponse {
    pub status: String,
    pub mode: String,
    pub job_id: String,
    pub status_endpoint: String,
    pub cancel_endpoint: String,
    pub cancel_requested: bool,
    pub bucket: String,
    pub prefix: String,
    pub tier: Option<String>,
    pub dry_run: bool,
    pub created_at_unix_nanos: i128,
    pub updated_at_unix_nanos: i128,
    pub completed_at_unix_nanos: Option<i128>,
    pub report: ManualTransitionRunReport,
    #[serde(default)]
    pub queue_snapshot: ManualTransitionQueueSnapshot,
    pub failure_reason: Option<String>,
}

/// Snapshot of transition worker pressure reported with manual transition jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualTransitionQueueSnapshot {
    #[serde(default)]
    pub queue_capacity: u64,
    #[serde(default)]
    pub queued: u64,
    #[serde(default)]
    pub active: u64,
    #[serde(default)]
    pub workers: u64,
    #[serde(default)]
    pub queue_full: u64,
    #[serde(default)]
    pub queue_send_timeout: u64,
    #[serde(default)]
    pub compensation_pending: u64,
    #[serde(default)]
    pub compensation_running: u64,
}

/// Aggregate report for a manual lifecycle transition run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualTransitionRunReport {
    pub bucket: String,
    pub prefix: String,
    pub tier: Option<String>,
    pub dry_run: bool,
    pub lifecycle_config_found: bool,
    pub scanned: u64,
    pub eligible: u64,
    pub enqueued: u64,
    pub dry_run_eligible: u64,
    pub skipped_not_transition: u64,
    pub skipped_tier: u64,
    pub skipped_delete_marker: u64,
    pub skipped_directory: u64,
    pub skipped_replication: u64,
    #[serde(default)]
    pub skipped_already_transitioned: u64,
    pub skipped_already_in_flight: u64,
    pub skipped_queue_full: u64,
    pub skipped_queue_closed: u64,
    pub skipped_queue_timeout: u64,
    #[serde(default)]
    pub transition_completed: u64,
    #[serde(default)]
    pub transition_failed: u64,
    #[serde(default)]
    pub tier_failure: u64,
    pub truncated_by_limit: bool,
    #[serde(default)]
    pub truncated_by_duration: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub continuation_token: Option<String>,
}

// ==================== Per-type sub-configs ====================
// These match the RustFS backend JSON format exactly.

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierS3 {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    #[serde(rename = "storageClass")]
    pub storage_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierRustFS {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    #[serde(rename = "storageClass")]
    pub storage_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierMinIO {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierAliyun {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierTencent {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierHuaweicloud {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierAzure {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    #[serde(rename = "storageClass")]
    pub storage_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierGCS {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "creds")]
    pub creds: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    #[serde(rename = "storageClass")]
    pub storage_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TierR2 {
    pub name: String,
    pub endpoint: String,
    #[serde(rename = "accessKey")]
    pub access_key: String,
    #[serde(rename = "secretKey")]
    pub secret_key: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_type_display() {
        assert_eq!(TierType::S3.to_string(), "S3");
        assert_eq!(TierType::RustFS.to_string(), "RustFS");
        assert_eq!(TierType::MinIO.to_string(), "MinIO");
        assert_eq!(TierType::Azure.to_string(), "Azure");
        assert_eq!(TierType::GCS.to_string(), "GCS");
        assert_eq!(TierType::R2.to_string(), "R2");
    }

    #[test]
    fn test_tier_type_from_str() {
        assert_eq!("s3".parse::<TierType>().unwrap(), TierType::S3);
        assert_eq!("rustfs".parse::<TierType>().unwrap(), TierType::RustFS);
        assert_eq!("MINIO".parse::<TierType>().unwrap(), TierType::MinIO);
        assert_eq!("Azure".parse::<TierType>().unwrap(), TierType::Azure);
        assert!("invalid".parse::<TierType>().is_err());
    }

    #[test]
    fn test_tier_config_serialization_s3() {
        let config = TierConfig {
            tier_type: TierType::S3,
            name: "WARM".to_string(),
            s3: Some(TierS3 {
                name: "WARM".to_string(),
                endpoint: "https://s3.amazonaws.com".to_string(),
                access_key: "AKID".to_string(),
                secret_key: "REDACTED".to_string(),
                bucket: "warm-bucket".to_string(),
                prefix: "tier/".to_string(),
                region: "us-east-1".to_string(),
                storage_class: "STANDARD_IA".to_string(),
            }),
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""type":"s3""#));
        assert!(json.contains("warm-bucket"));

        let decoded: TierConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.tier_type, TierType::S3);
        assert_eq!(decoded.tier_name(), "WARM");
        assert_eq!(decoded.bucket(), "warm-bucket");
    }

    #[test]
    fn test_tier_config_deserialization_from_backend() {
        // Simulates the JSON format returned by the RustFS admin API
        let json = r#"{"type":"rustfs","rustfs":{"name":"ARCHIVE","endpoint":"http://remote:9000","accessKey":"admin","secretKey":"REDACTED","bucket":"archive","prefix":"","region":""}}"#;
        let config: TierConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.tier_type, TierType::RustFS);
        assert_eq!(config.tier_name(), "ARCHIVE");
        assert_eq!(config.endpoint(), "http://remote:9000");
        assert_eq!(config.bucket(), "archive");
    }

    #[test]
    fn test_tier_creds_serialization() {
        let creds = TierCreds {
            access_key: "newkey".to_string(),
            secret_key: "newsecret".to_string(),
        };

        let json = serde_json::to_string(&creds).unwrap();
        assert!(json.contains("accessKey"));
        assert!(json.contains("secretKey"));

        let decoded: TierCreds = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.access_key, "newkey");
    }
}
