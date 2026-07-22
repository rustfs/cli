//! ObjectStore trait definition
//!
//! This trait defines the interface for S3-compatible storage operations.
//! It allows the CLI to be decoupled from the specific S3 SDK implementation.

use std::collections::HashMap;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::cors::CorsRule;
use crate::encryption::{BucketEncryption, ObjectEncryptionRequest};
use crate::error::{Error, Result};
use crate::lifecycle::LifecycleRule;
use crate::object_lock::{
    BucketObjectLockConfiguration, LegalHoldStatus, ObjectLockOptions, ObjectRetention,
};
use crate::path::RemotePath;
use crate::replication::{
    ReplicationConfiguration, ReplicationResyncStartOptions, ReplicationResyncStartResult,
    ReplicationResyncStatus,
};
use crate::select::SelectOptions;

/// Requested behavior for bucket creation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateBucketOptions {
    /// Explicit S3 location constraint. `None` omits the request body.
    pub region: Option<String>,
    /// Whether the resulting bucket must have versioning enabled.
    pub versioning_enabled: bool,
    /// Whether Object Lock must be enabled in the create request.
    pub object_lock_enabled: bool,
}

impl CreateBucketOptions {
    /// Build CLI options while applying Object Lock's required versioning invariant.
    pub fn for_cli(
        region: Option<String>,
        versioning_enabled: bool,
        object_lock_enabled: bool,
    ) -> Result<Self> {
        let options = Self {
            region,
            versioning_enabled: versioning_enabled || object_lock_enabled,
            object_lock_enabled,
        };
        options.validate()?;
        Ok(options)
    }

    /// Reject request states that cannot produce the promised bucket state.
    pub fn validate(&self) -> Result<()> {
        if self.object_lock_enabled && !self.versioning_enabled {
            return Err(Error::InvalidPath(
                "Bucket Object Lock requires versioning to be enabled".to_string(),
            ));
        }
        if self
            .region
            .as_deref()
            .is_some_and(|region| region.trim().is_empty())
        {
            return Err(Error::InvalidPath(
                "Bucket region cannot be empty".to_string(),
            ));
        }
        Ok(())
    }
}

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

/// Options for selecting an object version during read and metadata operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectReadOptions {
    /// Exact object version to select. `None` selects the current object.
    pub version_id: Option<String>,
}

impl ObjectReadOptions {
    /// Build read options while rejecting ambiguous empty version identifiers.
    pub fn for_version(version_id: Option<String>) -> Result<Self> {
        if version_id.as_deref().is_some_and(str::is_empty) {
            return Err(Error::InvalidPath("Version ID cannot be empty".to_string()));
        }
        Ok(Self { version_id })
    }
}

/// Pagination options for listing object versions and delete markers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListObjectVersionsOptions {
    /// Maximum number of entries to return.
    pub max_keys: Option<i32>,
    /// Key marker returned by the previous page.
    pub key_marker: Option<String>,
    /// Version marker returned by the previous page.
    pub version_id_marker: Option<String>,
}

/// Request-level options for object deletion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeleteRequestOptions {
    /// Exact object version to delete. `None` targets the current object state.
    pub version_id: Option<String>,
    /// Explicitly bypass Object Lock governance retention.
    pub bypass_governance: bool,
    /// Ask RustFS to permanently delete data instead of creating delete markers.
    pub force_delete: bool,
}

/// An object key and optional historical version selected for deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectVersionIdentifier {
    /// Object key.
    pub key: String,
    /// Exact version to delete, when version-aware deletion is requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Whether the selected version was listed as a delete marker.
    pub is_delete_marker: bool,
}

/// A version-aware delete result returned by the object store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedObject {
    /// Deleted object key.
    pub key: String,
    /// Deleted object version, when reported by the backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Whether the deleted entry is a delete marker.
    pub is_delete_marker: bool,
}

/// A per-object failure returned by a multi-object delete request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteObjectFailure {
    /// Object key that could not be deleted.
    pub key: String,
    /// Requested version, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// S3 error code, when provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Backend error message, when provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Result of deleting multiple version-aware object identifiers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteObjectsResult {
    /// Successfully deleted entries.
    pub deleted: Vec<DeletedObject>,
    /// Entries rejected by the backend.
    pub failures: Vec<DeleteObjectFailure>,
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

    /// Object version selected or created by the operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,

    /// Source object version used by a copy operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_version_id: Option<String>,

    /// Whether the selected version is a delete marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_delete_marker: Option<bool>,

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
            version_id: None,
            source_version_id: None,
            is_delete_marker: None,
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
            version_id: None,
            source_version_id: None,
            is_delete_marker: None,
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
            version_id: None,
            source_version_id: None,
            is_delete_marker: None,
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

    /// Get metadata for the current object or an exact historical version.
    async fn head_object_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
    ) -> Result<ObjectInfo> {
        if options.version_id.is_some() {
            return Err(Error::UnsupportedFeature(
                "Exact-version metadata reads are not implemented by this object store".to_string(),
            ));
        }
        self.head_object(path).await
    }

    /// Check if a bucket exists
    async fn bucket_exists(&self, bucket: &str) -> Result<bool>;

    /// Create a bucket
    async fn create_bucket(&self, bucket: &str) -> Result<()>;

    /// Create a bucket with explicit region, versioning, and Object Lock intent.
    ///
    /// The default preserves existing implementations for the option-free request and rejects
    /// advanced behavior instead of silently ignoring it.
    async fn create_bucket_with_options(
        &self,
        bucket: &str,
        options: &CreateBucketOptions,
    ) -> Result<()> {
        options.validate()?;
        if options != &CreateBucketOptions::default() {
            return Err(Error::UnsupportedFeature(
                "Bucket creation options are not implemented by this object store".to_string(),
            ));
        }
        self.create_bucket(bucket).await
    }

    /// Return the effective location reported by the service.
    ///
    /// `None` is the S3 representation for the default `us-east-1` location.
    async fn get_bucket_location(&self, _bucket: &str) -> Result<Option<String>> {
        Err(Error::UnsupportedFeature(
            "Bucket location inspection is not implemented by this object store".to_string(),
        ))
    }

    /// Delete a bucket
    async fn delete_bucket(&self, bucket: &str) -> Result<()>;

    /// Get backend capabilities
    async fn capabilities(&self) -> Result<Capabilities>;

    /// Get object content as bytes
    async fn get_object(&self, path: &RemotePath) -> Result<Vec<u8>>;

    /// Get current object content or an exact historical version as bytes.
    async fn get_object_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
    ) -> Result<Vec<u8>> {
        if options.version_id.is_some() {
            return Err(Error::UnsupportedFeature(
                "Exact-version object reads are not implemented by this object store".to_string(),
            ));
        }
        self.get_object(path).await
    }

    /// Stream current object content or an exact historical version to a writer.
    async fn write_object_to_with_options(
        &self,
        path: &RemotePath,
        options: &ObjectReadOptions,
        writer: &mut (dyn AsyncWrite + Send + Unpin),
        max_bytes: Option<u64>,
    ) -> Result<u64> {
        let data = self.get_object_with_options(path, options).await?;
        let write_len = max_bytes
            .and_then(|limit| usize::try_from(limit).ok())
            .map(|limit| limit.min(data.len()))
            .unwrap_or(data.len());
        writer.write_all(&data[..write_len]).await?;
        writer.flush().await?;
        Ok(write_len as u64)
    }

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

    /// Delete the current object state or one exact version with explicit request options.
    async fn delete_object_with_options(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> Result<DeletedObject> {
        if options.version_id.is_some() || options.bypass_governance || options.force_delete {
            return Err(Error::UnsupportedFeature(
                "Version-aware or policy-bypassing deletion is not implemented by this object store"
                    .to_string(),
            ));
        }
        self.delete_object(path).await?;
        Ok(DeletedObject {
            key: path.key.clone(),
            version_id: None,
            is_delete_marker: false,
        })
    }

    /// Delete multiple objects (batch delete)
    async fn delete_objects(&self, bucket: &str, keys: Vec<String>) -> Result<Vec<String>>;

    /// Delete exact object versions and delete markers in one request.
    async fn delete_object_versions(
        &self,
        _bucket: &str,
        _objects: Vec<ObjectVersionIdentifier>,
        _options: DeleteRequestOptions,
    ) -> Result<DeleteObjectsResult> {
        Err(Error::UnsupportedFeature(
            "Multi-object version deletion is not implemented by this object store".to_string(),
        ))
    }

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

    /// Get a bucket's Object Lock configuration.
    ///
    /// `None` means the bucket exists but has no Object Lock configuration.
    async fn get_bucket_object_lock_configuration(
        &self,
        _bucket: &str,
    ) -> Result<Option<BucketObjectLockConfiguration>> {
        Err(Error::UnsupportedFeature(
            "Bucket Object Lock configuration is not implemented by this object store".to_string(),
        ))
    }

    /// Update an Object Lock enabled bucket's configuration.
    async fn put_bucket_object_lock_configuration(
        &self,
        _bucket: &str,
        _configuration: BucketObjectLockConfiguration,
    ) -> Result<()> {
        Err(Error::UnsupportedFeature(
            "Bucket Object Lock configuration is not implemented by this object store".to_string(),
        ))
    }

    /// Get retention applied to the selected object version.
    async fn get_object_retention(
        &self,
        _path: &RemotePath,
        _options: &ObjectLockOptions,
    ) -> Result<Option<ObjectRetention>> {
        Err(Error::UnsupportedFeature(
            "Object retention is not implemented by this object store".to_string(),
        ))
    }

    /// Set or clear retention on the selected object version.
    async fn put_object_retention(
        &self,
        _path: &RemotePath,
        _retention: Option<ObjectRetention>,
        _options: &ObjectLockOptions,
    ) -> Result<()> {
        Err(Error::UnsupportedFeature(
            "Object retention is not implemented by this object store".to_string(),
        ))
    }

    /// Get legal-hold status for the selected object version.
    async fn get_object_legal_hold(
        &self,
        _path: &RemotePath,
        _options: &ObjectLockOptions,
    ) -> Result<LegalHoldStatus> {
        Err(Error::UnsupportedFeature(
            "Object legal hold is not implemented by this object store".to_string(),
        ))
    }

    /// Set legal-hold status for the selected object version.
    async fn put_object_legal_hold(
        &self,
        _path: &RemotePath,
        _status: LegalHoldStatus,
        _options: &ObjectLockOptions,
    ) -> Result<()> {
        Err(Error::UnsupportedFeature(
            "Object legal hold is not implemented by this object store".to_string(),
        ))
    }

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

    /// List one page of object versions and delete markers with both S3 pagination markers.
    async fn list_object_versions_page_with_options(
        &self,
        _path: &RemotePath,
        _options: &ListObjectVersionsOptions,
    ) -> Result<ObjectVersionListResult> {
        Err(Error::UnsupportedFeature(
            "Paginated object version listing is not implemented by this object store".to_string(),
        ))
    }

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
    fn create_bucket_options_reject_lock_without_versioning() {
        let options = CreateBucketOptions {
            region: Some("us-east-1".to_string()),
            versioning_enabled: false,
            object_lock_enabled: true,
        };

        let error = options
            .validate()
            .expect_err("Object Lock without versioning must be rejected");

        assert!(matches!(error, crate::Error::InvalidPath(_)));
    }

    #[test]
    fn create_bucket_options_normalize_cli_lock_to_versioning() {
        let options = CreateBucketOptions::for_cli(Some("us-east-1".to_string()), false, true)
            .expect("CLI Object Lock options should be valid");

        assert!(options.object_lock_enabled);
        assert!(options.versioning_enabled);
        assert_eq!(options.region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn test_object_info_file() {
        let info = ObjectInfo::file("test.txt", 1024);
        assert_eq!(info.key, "test.txt");
        assert_eq!(info.size_bytes, Some(1024));
        assert!(!info.is_dir);
        assert_eq!(info.version_id, None);
        assert_eq!(info.source_version_id, None);
        assert_eq!(info.is_delete_marker, None);
    }

    #[test]
    fn object_read_options_reject_empty_version_ids() {
        let error = ObjectReadOptions::for_version(Some(String::new()))
            .expect_err("empty version IDs must be rejected");

        assert!(matches!(error, crate::Error::InvalidPath(_)));
    }

    #[test]
    fn versioned_delete_targets_are_serializable_for_structured_output() {
        let target = ObjectVersionIdentifier {
            key: "reports/a.csv".to_string(),
            version_id: Some("v1".to_string()),
            is_delete_marker: true,
        };

        let json = serde_json::to_value(target).expect("serialize versioned delete target");
        assert_eq!(json["key"], "reports/a.csv");
        assert_eq!(json["version_id"], "v1");
        assert_eq!(json["is_delete_marker"], true);
    }

    #[test]
    fn object_info_version_fields_are_optional_and_serializable() {
        let current = serde_json::to_value(ObjectInfo::file("current.txt", 1))
            .expect("serialize current object info");
        assert!(current.get("version_id").is_none());
        assert!(current.get("source_version_id").is_none());
        assert!(current.get("is_delete_marker").is_none());

        let mut copied = ObjectInfo::file("copy.txt", 1);
        copied.version_id = Some("destination-v2".to_string());
        copied.source_version_id = Some("source-v1".to_string());
        let copied = serde_json::to_value(copied).expect("serialize copy object info");
        assert_eq!(copied["version_id"], "destination-v2");
        assert_eq!(copied["source_version_id"], "source-v1");
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
