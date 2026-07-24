//! Typed, bounded RustFS bucket-metadata archive operations.

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::Result;

/// Runtime capability required by bucket-metadata archive commands.
pub const BUCKET_METADATA_CAPABILITY: &str = "admin.bucket-metadata";

/// RustFS server limit for both exported and imported bucket-metadata archives.
pub const MAX_BUCKET_METADATA_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;

/// An owned archive whose storage is cleared when dropped.
pub struct BucketMetadataArchive {
    bytes: Zeroizing<Vec<u8>>,
}

impl BucketMetadataArchive {
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > MAX_BUCKET_METADATA_ARCHIVE_BYTES {
            return Err(crate::Error::RequestRejected(format!(
                "Bucket metadata archive size {} exceeds the {} byte limit",
                bytes.len(),
                MAX_BUCKET_METADATA_ARCHIVE_BYTES
            )));
        }
        Ok(Self {
            bytes: Zeroizing::new(bytes),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub fn into_bytes(self) -> Zeroizing<Vec<u8>> {
        self.bytes
    }
}

#[async_trait]
pub trait BucketMetadataApi: Send + Sync {
    /// Export every bucket, or one selected bucket, as a bounded ZIP archive.
    async fn export_bucket_metadata(&self, bucket: Option<&str>) -> Result<BucketMetadataArchive>;

    /// Import a prevalidated bounded archive. Mutations are never retried automatically.
    async fn import_bucket_metadata(&self, archive: BucketMetadataArchive) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_rejects_oversized_input() {
        let bytes = vec![0; MAX_BUCKET_METADATA_ARCHIVE_BYTES + 1];
        assert!(BucketMetadataArchive::new(bytes).is_err());
    }
}
