//! ObjectStore trait definition
//!
//! This trait defines the interface for S3-compatible storage operations.
//! It allows the CLI to be decoupled from the specific S3 SDK implementation.

use std::collections::HashMap;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;

use crate::cors::CorsRule;
use crate::encryption::{BucketEncryption, ObjectEncryptionRequest};
use crate::error::Result;
use crate::lifecycle::LifecycleRule;
use crate::path::RemotePath;
use crate::replication::{
    ReplicationConfiguration, ReplicationResyncStartOptions, ReplicationResyncStartResult,
    ReplicationResyncStatus,
};
use crate::select::SelectOptions;

/// Metadata for an object version
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectVersion {
    /// Object key
    pub key: String,

    /// Version ID
    pub version_id: String,

    /// Whether this is the latest version
    pub is_latest: bool,

    /// Whether this is a delete marker
    pub is_delete_marker: bool,

    /// Last modified timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<Timestamp>,

    /// Size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,

    /// ETag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Result of an object version list operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectVersionListResult {
    /// Listed object versions and delete markers
    pub items: Vec<ObjectVersion>,

    /// Whether the result is truncated (more items available)
    pub truncated: bool,

    /// Continuation key marker for pagination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,

    /// Continuation version marker for pagination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id_marker: Option<String>,
}

/// Metadata for an object or bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// Object key or bucket name
    pub key: String,

    /// Size in bytes (None for buckets)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,

    /// Human-readable size
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_human: Option<String>,

    /// Last modified timestamp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<Timestamp>,

    /// ETag (usually MD5 for single-part uploads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,

    /// Storage class
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,

    /// Content type
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,

    /// User-defined metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,

    /// Whether this is a directory/prefix
    pub is_dir: bool,
}

impl ObjectInfo {
    /// Create a new ObjectInfo for a file
    pub fn file(key: impl Into<String>, size: i64) -> Self {
        Self {
            key: key.into(),
            size_bytes: Some(size),
            size_human: Some(humansize::format_size(size as u64, humansize::BINARY)),
            last_modified: None,
            etag: None,
            storage_class: None,
            content_type: None,
            metadata: None,
            is_dir: false,
        }
    }

    /// Create a new ObjectInfo for a directory/prefix
    pub fn dir(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            size_bytes: None,
            size_human: None,
            last_modified: None,
            etag: None,
            storage_class: None,
            content_type: None,
            metadata: None,
            is_dir: true,
        }
    }

    /// Create a new ObjectInfo for a bucket
    pub fn bucket(name: impl Into<String>) -> Self {
        Self {
            key: name.into(),
            size_bytes: None,
            size_human: None,
            last_modified: None,
            etag: None,
            storage_class: None,
            content_type: None,
            metadata: None,
            is_dir: true,
        }
    }
}

/// Result of a list operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResult {
    /// Listed objects
    pub items: Vec<ObjectInfo>,

    /// Whether the result is truncated (more items available)
    pub truncated: bool,

    /// Continuation token for pagination
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
}

/// Options for list operations
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Maximum number of keys to return per request
    pub max_keys: Option<i32>,

    /// Delimiter for grouping (usually "/")
    pub delimiter: Option<String>,

    /// Prefix to filter by
    pub prefix: Option<String>,

    /// Continuation token for pagination
    pub continuation_token: Option<String>,

    /// Whether to list recursively (ignore delimiter)
    pub recursive: bool,
}

/// Backend capability information
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Supports bucket versioning
    pub versioning: bool,

    /// Supports object lock/retention
    pub object_lock: bool,

    /// Supports object tagging
    pub tagging: bool,

    /// Supports anonymous bucket access policies
    pub anonymous: bool,

    /// S3 Select (`SelectObjectContent`).
    ///
    /// This remains `false` in generic capability hints because support is determined by issuing
    /// a real request against the target object.
    pub select: bool,

    /// Supports event notifications
    pub notifications: bool,

    /// Supports lifecycle configuration
    pub lifecycle: bool,

    /// Supports bucket replication
    pub replication: bool,

    /// Supports bucket CORS configuration
    pub cors: bool,
}

/// Bucket notification target type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationTarget {
    /// SQS queue target
    Queue,
    /// SNS topic target
    Topic,
    /// Lambda function target
    Lambda,
}

/// Bucket notification rule
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketNotification {
    /// Optional rule id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Notification target type
    pub target: NotificationTarget,
    /// Target ARN
    pub arn: String,
    /// Event patterns
    pub events: Vec<String>,
    /// Optional key prefix filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Optional key suffix filter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

/// Trait for S3-compatible storage operations
///
/// This trait is implemented by the S3 adapter and can be mocked for testing.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// List buckets
    async fn list_buckets(&self) -> Result<Vec<ObjectInfo>>;

    /// List objects in a bucket or prefix
    async fn list_objects(&self, path: &RemotePath, options: ListOptions) -> Result<ListResult>;

    /// Get object metadata
    async fn head_object(&self, path: &RemotePath) -> Result<ObjectInfo>;

    /// Check if a bucket exists
    async fn bucket_exists(&self, bucket: &str) -> Result<bool>;

    /// Create a bucket
    async fn create_bucket(&self, bucket: &str) -> Result<()>;

    /// Delete a bucket
    async fn delete_bucket(&self, bucket: &str) -> Result<()>;

    /// Get backend capabilities
    async fn capabilities(&self) -> Result<Capabilities>;

    /// Get object content as bytes
    async fn get_object(&self, path: &RemotePath) -> Result<Vec<u8>>;

    /// Upload object from bytes
    async fn put_object(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
    ) -> Result<ObjectInfo>;

    /// Delete an object
    async fn delete_object(&self, path: &RemotePath) -> Result<()>;

    /// Delete multiple objects (batch delete)
    async fn delete_objects(&self, bucket: &str, keys: Vec<String>) -> Result<Vec<String>>;

    /// Copy object within S3 (server-side copy)
    async fn copy_object(
        &self,
        src: &RemotePath,
        dst: &RemotePath,
        encryption: Option<&ObjectEncryptionRequest>,
    ) -> Result<ObjectInfo>;

    /// Generate a presigned URL for an object
    async fn presign_get(&self, path: &RemotePath, expires_secs: u64) -> Result<String>;

    /// Generate a presigned URL for uploading an object
    async fn presign_put(
        &self,
        path: &RemotePath,
        expires_secs: u64,
        content_type: Option<&str>,
    ) -> Result<String>;

    // Phase 5: Optional operations (capability-dependent)

    /// Get bucket versioning status
    async fn get_versioning(&self, bucket: &str) -> Result<Option<bool>>;

    /// Set bucket versioning status
    async fn set_versioning(&self, bucket: &str, enabled: bool) -> Result<()>;

    /// Get bucket default encryption. Returns None when encryption is not configured.
    async fn get_bucket_encryption(&self, bucket: &str) -> Result<Option<BucketEncryption>>;

    /// Set bucket default encryption.
    async fn set_bucket_encryption(&self, bucket: &str, encryption: BucketEncryption)
    -> Result<()>;

    /// Delete bucket default encryption.
    async fn delete_bucket_encryption(&self, bucket: &str) -> Result<()>;

    /// List object versions
    async fn list_object_versions(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
    ) -> Result<Vec<ObjectVersion>>;

    /// Get object tags
    async fn get_object_tags(
        &self,
        path: &RemotePath,
    ) -> Result<std::collections::HashMap<String, String>>;

    /// Get bucket tags
    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<std::collections::HashMap<String, String>>;

    /// Set object tags
    async fn set_object_tags(
        &self,
        path: &RemotePath,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()>;

    /// Set bucket tags
    async fn set_bucket_tags(
        &self,
        bucket: &str,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()>;

    /// Delete object tags
    async fn delete_object_tags(&self, path: &RemotePath) -> Result<()>;

    /// Delete bucket tags
    async fn delete_bucket_tags(&self, bucket: &str) -> Result<()>;

    /// Get bucket policy as raw JSON string. Returns `None` when no policy exists.
    async fn get_bucket_policy(&self, bucket: &str) -> Result<Option<String>>;

    /// Replace bucket policy using raw JSON string.
    async fn set_bucket_policy(&self, bucket: &str, policy: &str) -> Result<()>;

    /// Remove bucket policy (set anonymous access to private).
    async fn delete_bucket_policy(&self, bucket: &str) -> Result<()>;

    /// Get bucket notification configuration as flat rules.
    async fn get_bucket_notifications(&self, bucket: &str) -> Result<Vec<BucketNotification>>;

    /// Replace bucket notification configuration with flat rules.
    async fn set_bucket_notifications(
        &self,
        bucket: &str,
        notifications: Vec<BucketNotification>,
    ) -> Result<()>;

    // Lifecycle operations (capability-dependent)

    /// Get bucket lifecycle rules. Returns empty vec if no lifecycle config exists.
    async fn get_bucket_lifecycle(&self, bucket: &str) -> Result<Vec<LifecycleRule>>;

    /// Set bucket lifecycle configuration (replaces all rules).
    async fn set_bucket_lifecycle(&self, bucket: &str, rules: Vec<LifecycleRule>) -> Result<()>;

    /// Delete bucket lifecycle configuration.
    async fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()>;

    /// Restore a transitioned (archived) object.
    async fn restore_object(&self, path: &RemotePath, days: i32) -> Result<()>;

    // Replication operations (capability-dependent)

    /// Get bucket replication configuration. Returns None if not configured.
    async fn get_bucket_replication(
        &self,
        bucket: &str,
    ) -> Result<Option<ReplicationConfiguration>>;

    /// Set bucket replication configuration.
    async fn set_bucket_replication(
        &self,
        bucket: &str,
        config: ReplicationConfiguration,
    ) -> Result<()>;

    /// Delete bucket replication configuration.
    async fn delete_bucket_replication(&self, bucket: &str) -> Result<()>;

    /// Actively validate configured replication targets.
    async fn check_bucket_replication(&self, bucket: &str) -> Result<()>;

    /// Start a server-side bucket replication resync.
    async fn start_bucket_replication_resync(
        &self,
        bucket: &str,
        options: ReplicationResyncStartOptions,
    ) -> Result<ReplicationResyncStartResult>;

    /// Read persisted server-side bucket replication resync status.
    async fn bucket_replication_resync_status(
        &self,
        bucket: &str,
        target_arn: Option<&str>,
    ) -> Result<ReplicationResyncStatus>;

    /// Get bucket CORS rules. Returns empty vec if no CORS config exists.
    async fn get_bucket_cors(&self, bucket: &str) -> Result<Vec<CorsRule>>;

    /// Set bucket CORS configuration (replaces all rules).
    async fn set_bucket_cors(&self, bucket: &str, rules: Vec<CorsRule>) -> Result<()>;

    /// Delete bucket CORS configuration.
    async fn delete_bucket_cors(&self, bucket: &str) -> Result<()>;

    /// Run S3 Select on an object and stream result payloads to `writer`.
    async fn select_object_content(
        &self,
        path: &RemotePath,
        options: &SelectOptions,
        writer: &mut (dyn AsyncWrite + Send + Unpin),
    ) -> Result<()>;
    // async fn get_versioning(&self, bucket: &str) -> Result<bool>;
    // async fn set_versioning(&self, bucket: &str, enabled: bool) -> Result<()>;
    // async fn get_tags(&self, path: &RemotePath) -> Result<HashMap<String, String>>;
    // async fn set_tags(&self, path: &RemotePath, tags: HashMap<String, String>) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_info_file() {
        let info = ObjectInfo::file("test.txt", 1024);
        assert_eq!(info.key, "test.txt");
        assert_eq!(info.size_bytes, Some(1024));
        assert!(!info.is_dir);
    }

    #[test]
    fn test_object_info_dir() {
        let info = ObjectInfo::dir("path/to/dir/");
        assert_eq!(info.key, "path/to/dir/");
        assert!(info.is_dir);
        assert!(info.size_bytes.is_none());
    }

    #[test]
    fn test_object_info_bucket() {
        let info = ObjectInfo::bucket("my-bucket");
        assert_eq!(info.key, "my-bucket");
        assert!(info.is_dir);
    }

    #[test]
    fn test_object_info_metadata_default_none() {
        let info = ObjectInfo::file("test.txt", 1024);
        assert!(info.metadata.is_none());
    }

    #[test]
    fn test_object_info_metadata_set() {
        let mut info = ObjectInfo::file("test.txt", 1024);
        let mut meta = HashMap::new();
        meta.insert("content-disposition".to_string(), "attachment".to_string());
        meta.insert("custom-key".to_string(), "custom-value".to_string());
        info.metadata = Some(meta);

        let metadata = info.metadata.as_ref().expect("metadata should be Some");
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata.get("content-disposition").unwrap(), "attachment");
        assert_eq!(metadata.get("custom-key").unwrap(), "custom-value");
    }
}
