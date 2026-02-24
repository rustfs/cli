//! ObjectStore trait definition
//!
//! This trait defines the interface for S3-compatible storage operations.
//! It allows the CLI to be decoupled from the specific S3 SDK implementation.

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::path::RemotePath;

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

    /// Supports S3 Select
    pub select: bool,

    /// Supports event notifications
    pub notifications: bool,
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
    ) -> Result<ObjectInfo>;

    /// Delete an object
    async fn delete_object(&self, path: &RemotePath) -> Result<()>;

    /// Delete multiple objects (batch delete)
    async fn delete_objects(&self, bucket: &str, keys: Vec<String>) -> Result<Vec<String>>;

    /// Copy object within S3 (server-side copy)
    async fn copy_object(&self, src: &RemotePath, dst: &RemotePath) -> Result<ObjectInfo>;

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
}
