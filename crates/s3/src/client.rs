//! S3 client implementation
//!
//! Wraps aws-sdk-s3 and implements the ObjectStore trait from rc-core.

use async_trait::async_trait;

use rc_core::{
    Alias, Capabilities, Error, ListOptions, ListResult, ObjectInfo, ObjectStore, RemotePath,
    Result,
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
                    info.last_modified = Some(
                        chrono::DateTime::from_timestamp(creation_date.secs(), 0)
                            .unwrap_or_default(),
                    );
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

        let response = request
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;

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
                info.last_modified =
                    Some(chrono::DateTime::from_timestamp(modified.secs(), 0).unwrap_or_default());
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
            info.last_modified =
                Some(chrono::DateTime::from_timestamp(modified.secs(), 0).unwrap_or_default());
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
