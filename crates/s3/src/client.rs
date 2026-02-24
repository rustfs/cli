//! S3 client implementation
//!
//! Wraps aws-sdk-s3 and implements the ObjectStore trait from rc-core.

use async_trait::async_trait;

use jiff::Timestamp;
use rc_core::{
    Alias, Capabilities, Error, ListOptions, ListResult, ObjectInfo, ObjectStore, ObjectVersion,
    RemotePath, Result,
};

/// S3 client wrapper
pub struct S3Client {
    inner: aws_sdk_s3::Client,
    #[allow(dead_code)]
    alias: Alias,
}

impl S3Client {
    /// Create a new S3 client from an alias configuration
    pub async fn new(alias: Alias) -> Result<Self> {
        let endpoint = alias.endpoint.clone();
        let region = alias.region.clone();
        let access_key = alias.access_key.clone();
        let secret_key = alias.secret_key.clone();

        // Build credentials provider
        let credentials = aws_credential_types::Credentials::new(
            access_key,
            secret_key,
            None, // session token
            None, // expiry
            "rc-static-credentials",
        );

        // Build SDK config
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .credentials_provider(credentials)
            .region(aws_config::Region::new(region))
            .endpoint_url(&endpoint)
            .load()
            .await;

        // Build S3 client with path-style addressing for compatibility
        let s3_config = aws_sdk_s3::config::Builder::from(&config)
            .force_path_style(alias.bucket_lookup == "path" || alias.bucket_lookup == "auto")
            .build();

        let client = aws_sdk_s3::Client::from_conf(s3_config);

        Ok(Self {
            inner: client,
            alias,
        })
    }

    /// Get the underlying aws-sdk-s3 client
    pub fn inner(&self) -> &aws_sdk_s3::Client {
        &self.inner
    }

    /// Format AWS SDK error into a detailed error message
    fn format_sdk_error<E: std::fmt::Display>(error: &aws_sdk_s3::error::SdkError<E>) -> String {
        match error {
            aws_sdk_s3::error::SdkError::ServiceError(service_err) => {
                let err = service_err.err();
                let meta = service_err.raw();
                let mut msg = format!("Service error: {}", err);
                // Try to extract additional error information from headers
                if let Some(code) = meta.headers().get("x-amz-error-code")
                    && let Ok(code_str) = std::str::from_utf8(code.as_bytes())
                {
                    msg.push_str(&format!(" (code: {})", code_str));
                }
                msg
            }
            aws_sdk_s3::error::SdkError::ConstructionFailure(err) => {
                format!("Request construction failed: {:?}", err)
            }
            aws_sdk_s3::error::SdkError::TimeoutError(_) => "Request timeout".to_string(),
            aws_sdk_s3::error::SdkError::DispatchFailure(err) => {
                format!("Network dispatch error: {:?}", err)
            }
            aws_sdk_s3::error::SdkError::ResponseError(err) => {
                format!("Response error: {:?}", err)
            }
            _ => error.to_string(),
        }
    }
}

fn build_tagging(
    tags: std::collections::HashMap<String, String>,
) -> Result<aws_sdk_s3::types::Tagging> {
    use aws_sdk_s3::types::{Tag, Tagging};

    let mut tag_set = Vec::with_capacity(tags.len());
    for (key, value) in tags {
        let tag = Tag::builder()
            .key(key)
            .value(value)
            .build()
            .map_err(|e| Error::General(format!("invalid tag: {e}")))?;
        tag_set.push(tag);
    }

    Tagging::builder()
        .set_tag_set(Some(tag_set))
        .build()
        .map_err(|e| Error::General(format!("invalid tagging payload: {e}")))
}

#[async_trait]
impl ObjectStore for S3Client {
    async fn list_buckets(&self) -> Result<Vec<ObjectInfo>> {
        let response = self
            .inner
            .list_buckets()
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        let buckets = response
            .buckets()
            .iter()
            .map(|b| {
                let mut info = ObjectInfo::bucket(b.name().unwrap_or_default());
                if let Some(creation_date) = b.creation_date() {
                    info.last_modified = jiff::Timestamp::from_second(creation_date.secs()).ok();
                }
                info
            })
            .collect();

        Ok(buckets)
    }

    async fn list_objects(&self, path: &RemotePath, options: ListOptions) -> Result<ListResult> {
        let mut request = self.inner.list_objects_v2().bucket(&path.bucket);

        // Set prefix
        let prefix = if path.key.is_empty() {
            options.prefix.clone()
        } else if let Some(p) = &options.prefix {
            Some(format!("{}{}", path.key, p))
        } else {
            Some(path.key.clone())
        };

        if let Some(p) = prefix {
            request = request.prefix(p);
        }

        // Set delimiter (for non-recursive listing)
        if !options.recursive {
            request = request.delimiter(options.delimiter.as_deref().unwrap_or("/"));
        }

        // Set max keys
        if let Some(max) = options.max_keys {
            request = request.max_keys(max);
        }

        // Set continuation token
        if let Some(token) = &options.continuation_token {
            request = request.continuation_token(token);
        }

        let response = request.send().await.map_err(|e| {
            let err_str = Self::format_sdk_error(&e);
            if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                Error::NotFound(format!("Bucket not found: {}", path.bucket))
            } else {
                Error::Network(err_str)
            }
        })?;

        let mut items = Vec::new();

        // Add common prefixes (directories)
        for prefix in response.common_prefixes() {
            if let Some(p) = prefix.prefix() {
                items.push(ObjectInfo::dir(p));
            }
        }

        // Add objects
        for object in response.contents() {
            let key = object.key().unwrap_or_default().to_string();
            let size = object.size().unwrap_or(0);
            let mut info = ObjectInfo::file(&key, size);

            if let Some(modified) = object.last_modified() {
                info.last_modified = jiff::Timestamp::from_second(modified.secs()).ok();
            }

            if let Some(etag) = object.e_tag() {
                info.etag = Some(etag.trim_matches('"').to_string());
            }

            if let Some(sc) = object.storage_class() {
                info.storage_class = Some(sc.as_str().to_string());
            }

            items.push(info);
        }

        Ok(ListResult {
            items,
            truncated: response.is_truncated().unwrap_or(false),
            continuation_token: response.next_continuation_token().map(|s| s.to_string()),
        })
    }

    async fn head_object(&self, path: &RemotePath) -> Result<ObjectInfo> {
        let response = self
            .inner
            .head_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    Error::NotFound(path.to_string())
                } else {
                    Error::Network(err_str)
                }
            })?;

        let size = response.content_length().unwrap_or(0);
        let mut info = ObjectInfo::file(&path.key, size);

        if let Some(modified) = response.last_modified() {
            info.last_modified = jiff::Timestamp::from_second(modified.secs()).ok();
        }

        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }

        if let Some(ct) = response.content_type() {
            info.content_type = Some(ct.to_string());
        }

        if let Some(sc) = response.storage_class() {
            info.storage_class = Some(sc.as_str().to_string());
        }

        Ok(info)
    }

    async fn bucket_exists(&self, bucket: &str) -> Result<bool> {
        match self.inner.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(true),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    Ok(false)
                } else {
                    Err(Error::Network(err_str))
                }
            }
        }
    }

    async fn create_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        Ok(())
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    Error::NotFound(format!("Bucket not found: {bucket}"))
                } else {
                    Error::Network(err_str)
                }
            })?;

        Ok(())
    }

    async fn capabilities(&self) -> Result<Capabilities> {
        // For now, return conservative defaults
        // In Phase 5, we'll implement actual capability detection
        Ok(Capabilities {
            versioning: true,
            object_lock: false,
            tagging: true,
            select: false,
            notifications: false,
        })
    }

    async fn get_object(&self, path: &RemotePath) -> Result<Vec<u8>> {
        let response = self
            .inner
            .get_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    Error::NotFound(path.to_string())
                } else {
                    Error::Network(err_str)
                }
            })?;

        let data = response
            .body
            .collect()
            .await
            .map_err(|e| Error::Network(e.to_string()))?
            .into_bytes()
            .to_vec();

        Ok(data)
    }

    async fn put_object(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<ObjectInfo> {
        let size = data.len() as i64;
        let body = aws_sdk_s3::primitives::ByteStream::from(data);

        let mut request = self
            .inner
            .put_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .body(body);

        if let Some(ct) = content_type {
            request = request.content_type(ct);
        }

        let response = request
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        let mut info = ObjectInfo::file(&path.key, size);
        if let Some(etag) = response.e_tag() {
            info.etag = Some(etag.trim_matches('"').to_string());
        }
        info.last_modified = Some(jiff::Timestamp::now());

        Ok(info)
    }

    async fn delete_object(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .delete_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    Error::NotFound(path.to_string())
                } else {
                    Error::Network(err_str)
                }
            })?;

        Ok(())
    }

    async fn delete_objects(&self, bucket: &str, keys: Vec<String>) -> Result<Vec<String>> {
        use aws_sdk_s3::types::{Delete, ObjectIdentifier};

        if keys.is_empty() {
            return Ok(vec![]);
        }

        let objects: Vec<ObjectIdentifier> = keys
            .iter()
            .map(|k| ObjectIdentifier::builder().key(k).build().unwrap())
            .collect();

        let delete = Delete::builder()
            .set_objects(Some(objects))
            .build()
            .map_err(|e| Error::General(e.to_string()))?;

        let response = self
            .inner
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

        // Collect deleted keys
        let deleted: Vec<String> = response
            .deleted()
            .iter()
            .filter_map(|d| d.key().map(|k| k.to_string()))
            .collect();

        // Check for errors
        if !response.errors().is_empty() {
            let error_keys: Vec<String> = response
                .errors()
                .iter()
                .filter_map(|e| e.key().map(|k| k.to_string()))
                .collect();
            tracing::warn!("Failed to delete some objects: {:?}", error_keys);
        }

        Ok(deleted)
    }

    async fn copy_object(&self, src: &RemotePath, dst: &RemotePath) -> Result<ObjectInfo> {
        // Build copy source: bucket/key
        let copy_source = format!("{}/{}", src.bucket, src.key);

        let response = self
            .inner
            .copy_object()
            .copy_source(&copy_source)
            .bucket(&dst.bucket)
            .key(&dst.key)
            .send()
            .await
            .map_err(|e| {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    Error::NotFound(src.to_string())
                } else {
                    Error::Network(err_str)
                }
            })?;

        // Get size from head_object since copy doesn't return it
        let info = self.head_object(dst).await?;

        // Update etag from copy response if available
        let mut result = info;
        if let Some(copy_result) = response.copy_object_result()
            && let Some(etag) = copy_result.e_tag()
        {
            result.etag = Some(etag.trim_matches('"').to_string());
        }

        Ok(result)
    }

    async fn presign_get(&self, path: &RemotePath, expires_secs: u64) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::builder()
            .expires_in(std::time::Duration::from_secs(expires_secs))
            .build()
            .map_err(|e| Error::General(format!("presign_get config: {e}")))?;

        let request = self
            .inner
            .get_object()
            .bucket(&path.bucket)
            .key(&path.key)
            .presigned(config)
            .await
            .map_err(|e| Error::General(format!("presign_get: {e}")))?;

        Ok(request.uri().to_string())
    }

    async fn presign_put(
        &self,
        path: &RemotePath,
        expires_secs: u64,
        content_type: Option<&str>,
    ) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::builder()
            .expires_in(std::time::Duration::from_secs(expires_secs))
            .build()
            .map_err(|e| Error::General(format!("presign_put config: {e}")))?;

        let mut builder = self.inner.put_object().bucket(&path.bucket).key(&path.key);

        if let Some(ct) = content_type {
            builder = builder.content_type(ct);
        }

        let request = builder
            .presigned(config)
            .await
            .map_err(|e| Error::General(format!("presign_put: {e}")))?;

        Ok(request.uri().to_string())
    }

    async fn get_versioning(&self, bucket: &str) -> Result<Option<bool>> {
        let response = self
            .inner
            .get_bucket_versioning()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::General(format!("get_versioning: {e}")))?;

        Ok(response
            .status()
            .map(|s| *s == aws_sdk_s3::types::BucketVersioningStatus::Enabled))
    }

    async fn set_versioning(&self, bucket: &str, enabled: bool) -> Result<()> {
        use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};

        let status = if enabled {
            BucketVersioningStatus::Enabled
        } else {
            BucketVersioningStatus::Suspended
        };

        let config = VersioningConfiguration::builder().status(status).build();

        self.inner
            .put_bucket_versioning()
            .bucket(bucket)
            .versioning_configuration(config)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_versioning: {e}")))?;

        Ok(())
    }

    async fn list_object_versions(
        &self,
        path: &RemotePath,
        max_keys: Option<i32>,
    ) -> Result<Vec<ObjectVersion>> {
        let mut builder = self.inner.list_object_versions().bucket(&path.bucket);

        if !path.key.is_empty() {
            builder = builder.prefix(&path.key);
        }

        if let Some(max) = max_keys {
            builder = builder.max_keys(max);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| Error::General(format!("list_object_versions: {e}")))?;

        let mut versions = Vec::new();

        // Add regular versions
        for v in response.versions() {
            versions.push(ObjectVersion {
                key: v.key().unwrap_or_default().to_string(),
                version_id: v.version_id().unwrap_or("null").to_string(),
                is_latest: v.is_latest().unwrap_or(false),
                is_delete_marker: false,
                last_modified: v
                    .last_modified()
                    .and_then(|dt| Timestamp::from_second(dt.secs()).ok()),
                size_bytes: v.size(),
                etag: v.e_tag().map(|s| s.trim_matches('"').to_string()),
            });
        }

        // Add delete markers
        for m in response.delete_markers() {
            versions.push(ObjectVersion {
                key: m.key().unwrap_or_default().to_string(),
                version_id: m.version_id().unwrap_or("null").to_string(),
                is_latest: m.is_latest().unwrap_or(false),
                is_delete_marker: true,
                last_modified: m
                    .last_modified()
                    .and_then(|dt| Timestamp::from_second(dt.secs()).ok()),
                size_bytes: None,
                etag: None,
            });
        }

        // Sort by key and then by last_modified (descending)
        versions.sort_by(|a, b| {
            a.key
                .cmp(&b.key)
                .then_with(|| b.last_modified.cmp(&a.last_modified))
        });

        Ok(versions)
    }

    async fn get_object_tags(
        &self,
        path: &RemotePath,
    ) -> Result<std::collections::HashMap<String, String>> {
        let response = match self
            .inner
            .get_object_tagging()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                if e.to_string().contains("NoSuchTagSet") {
                    return Ok(std::collections::HashMap::new());
                }
                return Err(Error::General(format!("get_object_tags: {e}")));
            }
        };

        let mut tags = std::collections::HashMap::new();
        for tag in response.tag_set() {
            let key = tag.key();
            let value = tag.value();
            tags.insert(key.to_string(), value.to_string());
        }

        Ok(tags)
    }

    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let response = match self.inner.get_bucket_tagging().bucket(bucket).send().await {
            Ok(response) => response,
            Err(e) => {
                if e.to_string().contains("NoSuchTagSet") {
                    return Ok(std::collections::HashMap::new());
                }
                return Err(Error::General(format!("get_bucket_tags: {e}")));
            }
        };

        let mut tags = std::collections::HashMap::new();
        for tag in response.tag_set() {
            let key = tag.key();
            let value = tag.value();
            tags.insert(key.to_string(), value.to_string());
        }

        Ok(tags)
    }

    async fn set_object_tags(
        &self,
        path: &RemotePath,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let tagging = build_tagging(tags)?;

        self.inner
            .put_object_tagging()
            .bucket(&path.bucket)
            .key(&path.key)
            .tagging(tagging)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_object_tags: {e}")))?;

        Ok(())
    }

    async fn set_bucket_tags(
        &self,
        bucket: &str,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let tagging = build_tagging(tags)?;

        self.inner
            .put_bucket_tagging()
            .bucket(bucket)
            .tagging(tagging)
            .send()
            .await
            .map_err(|e| Error::General(format!("set_bucket_tags: {e}")))?;

        Ok(())
    }

    async fn delete_object_tags(&self, path: &RemotePath) -> Result<()> {
        self.inner
            .delete_object_tagging()
            .bucket(&path.bucket)
            .key(&path.key)
            .send()
            .await
            .map_err(|e| Error::General(format!("delete_object_tags: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_tags(&self, bucket: &str) -> Result<()> {
        self.inner
            .delete_bucket_tagging()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| Error::General(format!("delete_bucket_tags: {e}")))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_info_creation() {
        let info = ObjectInfo::file("test.txt", 1024);
        assert_eq!(info.key, "test.txt");
        assert_eq!(info.size_bytes, Some(1024));
    }
}
