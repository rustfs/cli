//! Backend-neutral options for faithful object reads, writes, and copies.

use std::collections::HashMap;
use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use http::header::{HeaderName, HeaderValue};
use jiff::Timestamp;
use zeroize::Zeroizing;

use crate::encryption::ObjectEncryptionRequest;
use crate::object_lock::{LegalHoldStatus, ObjectRetention};
use crate::traits::{CopyObjectOptions, ObjectReadOptions};
use crate::{Error, Result};

const S3_MULTIPART_CHECKSUM_MAX_PARTS: u32 = 10_000;

/// Standard HTTP object attributes and user-defined metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectAttributes {
    /// Media type stored with the object.
    pub content_type: Option<String>,
    /// Cache policy stored with the object.
    pub cache_control: Option<String>,
    /// Content disposition stored with the object.
    pub content_disposition: Option<String>,
    /// Content encoding stored with the object.
    pub content_encoding: Option<String>,
    /// Content language stored with the object.
    pub content_language: Option<String>,
    /// Optional expiry timestamp stored with the object.
    pub expires: Option<Timestamp>,
    /// User-defined metadata without the protocol header prefix.
    pub user_metadata: HashMap<String, String>,
}

impl ObjectAttributes {
    /// Validate values that would otherwise be ambiguous at the S3 boundary.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("Content-Type", self.content_type.as_deref()),
            ("Cache-Control", self.cache_control.as_deref()),
            ("Content-Disposition", self.content_disposition.as_deref()),
            ("Content-Encoding", self.content_encoding.as_deref()),
            ("Content-Language", self.content_language.as_deref()),
        ] {
            if let Some(value) = value {
                if value.trim().is_empty() {
                    return Err(Error::InvalidPath(format!("{name} cannot be empty")));
                }
                HeaderValue::from_str(value).map_err(|_| {
                    Error::InvalidPath(format!("{name} contains invalid HTTP header characters"))
                })?;
            }
        }
        for (key, value) in &self.user_metadata {
            if key.trim().is_empty() {
                return Err(Error::InvalidPath(
                    "User metadata keys cannot be empty".to_string(),
                ));
            }
            let header_name = format!("x-amz-meta-{key}");
            HeaderName::from_bytes(header_name.as_bytes()).map_err(|_| {
                Error::InvalidPath(format!(
                    "User metadata key '{key}' cannot be represented as an HTTP header"
                ))
            })?;
            HeaderValue::from_str(value).map_err(|_| {
                Error::InvalidPath(format!(
                    "User metadata value for '{key}' contains invalid HTTP header characters"
                ))
            })?;
        }
        Ok(())
    }

    fn contains_only_content_type(&self) -> bool {
        self.cache_control.is_none()
            && self.content_disposition.is_none()
            && self.content_encoding.is_none()
            && self.content_language.is_none()
            && self.expires.is_none()
            && self.user_metadata.is_empty()
    }
}

/// How CopyObject handles source metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataDirective {
    /// Copy metadata from the selected source object.
    Copy,
    /// Replace source metadata with the provided destination attributes.
    Replace,
}

/// How CopyObject handles source tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaggingDirective {
    /// Copy tags from the selected source object.
    Copy,
    /// Replace source tags with the provided destination tags.
    Replace,
}

/// Checksum algorithms understood by S3-compatible object APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgorithm {
    /// CRC-32.
    Crc32,
    /// CRC-32C.
    Crc32c,
    /// CRC-64/NVME.
    Crc64Nvme,
    /// SHA-1.
    Sha1,
    /// SHA-256.
    Sha256,
}

/// An encoded checksum paired with its algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectChecksum {
    /// Algorithm used to calculate the checksum.
    pub algorithm: ChecksumAlgorithm,
    /// Protocol-encoded checksum value.
    pub value: String,
}

/// How a destination checksum should be supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumRequest {
    /// Ask the adapter and service to calculate the selected algorithm.
    Calculate(ChecksumAlgorithm),
    /// Send a checksum that the caller already calculated.
    Precomputed(ObjectChecksum),
}

impl ChecksumRequest {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Calculate(_) => Ok(()),
            Self::Precomputed(checksum) => checksum.validate(),
        }
    }
}

/// Fidelity metadata returned independently of the stable ObjectInfo output contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectTransferMetadata {
    /// Standard HTTP attributes and user-defined metadata.
    pub attributes: ObjectAttributes,
    /// Storage class reported by the service.
    pub storage_class: Option<String>,
    /// Persisted checksums reported by checksum-mode metadata reads.
    pub checksums: Vec<ObjectChecksum>,
}

impl ObjectChecksum {
    /// Build a checksum while rejecting an ambiguous empty value.
    pub fn new(algorithm: ChecksumAlgorithm, value: impl Into<String>) -> Result<Self> {
        let checksum = Self {
            algorithm,
            value: value.into(),
        };
        checksum.validate()?;
        Ok(checksum)
    }

    /// Build a checksum reported by a metadata read.
    ///
    /// Multipart SHA and CRC checksums use the S3 composite form
    /// `<base64-digest>-<part-count>`, which is not valid as a precomputed
    /// full-object write checksum.
    pub fn new_persisted(algorithm: ChecksumAlgorithm, value: impl Into<String>) -> Result<Self> {
        let checksum = Self {
            algorithm,
            value: value.into(),
        };
        if checksum.validate().is_ok() {
            return Ok(checksum);
        }

        let (digest, part_count) = checksum.value.rsplit_once('-').ok_or_else(|| {
            Error::InvalidPath("Persisted object checksum is not valid Base64".to_string())
        })?;
        let part_count = part_count.parse::<u32>().map_err(|_| {
            Error::InvalidPath(
                "Composite checksum part count must be a positive integer".to_string(),
            )
        })?;
        if part_count == 0
            || part_count > S3_MULTIPART_CHECKSUM_MAX_PARTS
            || checksum.algorithm == ChecksumAlgorithm::Crc64Nvme
        {
            return Err(Error::InvalidPath(
                "Persisted composite checksum is not valid for this algorithm".to_string(),
            ));
        }
        let decoded = BASE64_STANDARD.decode(digest).map_err(|_| {
            Error::InvalidPath("Composite checksum digest must be valid Base64".to_string())
        })?;
        let expected_length = match checksum.algorithm {
            ChecksumAlgorithm::Crc32 | ChecksumAlgorithm::Crc32c => 4,
            ChecksumAlgorithm::Sha1 => 20,
            ChecksumAlgorithm::Sha256 => 32,
            ChecksumAlgorithm::Crc64Nvme => 8,
        };
        if decoded.len() != expected_length {
            return Err(Error::InvalidPath(format!(
                "Composite checksum for {:?} must decode to {expected_length} bytes",
                checksum.algorithm
            )));
        }
        Ok(checksum)
    }

    /// Validate the checksum value before a request is attempted.
    pub fn validate(&self) -> Result<()> {
        if self.value.trim().is_empty() {
            return Err(Error::InvalidPath(
                "Object checksum value cannot be empty".to_string(),
            ));
        }
        let decoded = BASE64_STANDARD
            .decode(&self.value)
            .map_err(|_| Error::InvalidPath("Object checksum must be valid Base64".to_string()))?;
        let expected_length = match self.algorithm {
            ChecksumAlgorithm::Crc32 | ChecksumAlgorithm::Crc32c => 4,
            ChecksumAlgorithm::Crc64Nvme => 8,
            ChecksumAlgorithm::Sha1 => 20,
            ChecksumAlgorithm::Sha256 => 32,
        };
        if decoded.len() != expected_length {
            return Err(Error::InvalidPath(format!(
                "Object checksum for {:?} must decode to {expected_length} bytes",
                self.algorithm
            )));
        }
        Ok(())
    }
}

/// A 256-bit SSE-C key whose formatting never reveals key material.
#[derive(Clone, PartialEq, Eq)]
pub struct SseCustomerKey(Zeroizing<Vec<u8>>);

impl SseCustomerKey {
    /// Store an exact 256-bit key in zeroizing memory.
    pub fn new(key: Vec<u8>) -> Result<Self> {
        let key = Zeroizing::new(key);
        if key.len() != 32 {
            return Err(Error::InvalidPath(
                "SSE-C keys must contain exactly 32 bytes".to_string(),
            ));
        }
        Ok(Self(key))
    }

    /// Borrow key bytes for protocol adapters.
    ///
    /// Callers must not log, serialize, or include this value in errors.
    pub fn expose_secret(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl fmt::Debug for SseCustomerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SseCustomerKey([REDACTED])")
    }
}

/// Encryption requested for a destination object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectWriteEncryption {
    /// Existing S3-managed or KMS-managed encryption request.
    Managed(ObjectEncryptionRequest),
    /// Customer-provided key used only for this transfer.
    SseCustomer {
        /// Redacted, zeroizing customer key.
        key: SseCustomerKey,
    },
}

impl From<ObjectEncryptionRequest> for ObjectWriteEncryption {
    fn from(value: ObjectEncryptionRequest) -> Self {
        Self::Managed(value)
    }
}

/// Complete destination options for an object write or copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectWriteOptions {
    /// Attributes to store. `Some(Default::default())` represents an explicit empty replacement.
    pub attributes: Option<ObjectAttributes>,
    /// Tags to store. `Some(HashMap::new())` represents an explicit empty replacement.
    pub tags: Option<HashMap<String, String>>,
    /// Requested destination storage class.
    pub storage_class: Option<String>,
    /// Checksum supplied or selected for the write.
    pub checksum: Option<ChecksumRequest>,
    /// Destination encryption policy.
    pub encryption: Option<ObjectWriteEncryption>,
    /// Retention applied atomically with object creation.
    pub retention: Option<ObjectRetention>,
    /// Legal-hold state applied atomically with object creation.
    pub legal_hold: Option<LegalHoldStatus>,
}

impl ObjectWriteOptions {
    /// Validate fields before any backend mutation.
    pub fn validate(&self) -> Result<()> {
        if let Some(attributes) = &self.attributes {
            attributes.validate()?;
        }
        if let Some(tags) = &self.tags {
            if tags.len() > 10 {
                return Err(Error::InvalidPath(
                    "Objects can have at most 10 tags".to_string(),
                ));
            }
            for (key, value) in tags {
                let key_length = key.chars().count();
                if !(1..=128).contains(&key_length) {
                    return Err(Error::InvalidPath(
                        "Object tag keys must contain between 1 and 128 characters".to_string(),
                    ));
                }
                if value.chars().count() > 256 {
                    return Err(Error::InvalidPath(
                        "Object tag values cannot exceed 256 characters".to_string(),
                    ));
                }
            }
        }
        if self
            .storage_class
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(Error::InvalidPath(
                "Storage class cannot be empty".to_string(),
            ));
        }
        if let Some(checksum) = &self.checksum {
            checksum.validate()?;
        }
        if matches!(
            self.encryption.as_ref(),
            Some(ObjectWriteEncryption::Managed(
                ObjectEncryptionRequest::SseKms { key_id }
            )) if key_id.trim().is_empty()
        ) {
            return Err(Error::InvalidPath("KMS key ID cannot be empty".to_string()));
        }
        Ok(())
    }

    /// Translate the subset supported by the original ObjectStore put API.
    ///
    /// Backends use this to preserve compatibility without silently dropping advanced fields.
    pub fn legacy_put_arguments(&self) -> Result<(Option<&str>, Option<&ObjectEncryptionRequest>)> {
        self.validate()?;
        let content_type = match &self.attributes {
            Some(attributes) if attributes.contains_only_content_type() => {
                attributes.content_type.as_deref()
            }
            Some(_) => {
                return Err(Error::UnsupportedFeature(
                    "Object attributes other than Content-Type require advanced write support"
                        .to_string(),
                ));
            }
            None => None,
        };
        if self.tags.is_some()
            || self.storage_class.is_some()
            || self.checksum.is_some()
            || self.retention.is_some()
            || self.legal_hold.is_some()
        {
            return Err(Error::UnsupportedFeature(
                "Transfer fidelity options are not implemented by this object store".to_string(),
            ));
        }
        let encryption = match self.encryption.as_ref() {
            Some(ObjectWriteEncryption::Managed(request)) => Some(request),
            Some(ObjectWriteEncryption::SseCustomer { .. }) => {
                return Err(Error::UnsupportedFeature(
                    "SSE-C writes are not implemented by this object store".to_string(),
                ));
            }
            None => None,
        };
        Ok((content_type, encryption))
    }

    fn legacy_copy_encryption(&self) -> Result<Option<&ObjectEncryptionRequest>> {
        self.validate()?;
        if self.attributes.is_some()
            || self.tags.is_some()
            || self.storage_class.is_some()
            || self.checksum.is_some()
            || self.retention.is_some()
            || self.legal_hold.is_some()
        {
            return Err(Error::UnsupportedFeature(
                "Advanced copy destination options are not implemented by this object store"
                    .to_string(),
            ));
        }
        match self.encryption.as_ref() {
            Some(ObjectWriteEncryption::Managed(request)) => Ok(Some(request)),
            Some(ObjectWriteEncryption::SseCustomer { .. }) => Err(Error::UnsupportedFeature(
                "Destination SSE-C is not implemented by this object store".to_string(),
            )),
            None => Ok(None),
        }
    }
}

/// Advanced read options that do not change the legacy ObjectReadOptions contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferReadOptions {
    /// Exact object version to select.
    pub version_id: Option<String>,
    /// Request persisted checksum fields from the backend.
    pub checksum_mode: bool,
    /// Customer key needed to read an SSE-C source object.
    pub customer_key: Option<SseCustomerKey>,
}

impl TransferReadOptions {
    /// Validate read selection before issuing a request.
    pub fn validate(&self) -> Result<()> {
        if self.version_id.as_deref().is_some_and(str::is_empty) {
            return Err(Error::InvalidPath("Version ID cannot be empty".to_string()));
        }
        Ok(())
    }

    pub(crate) fn legacy_read_options(&self) -> Result<ObjectReadOptions> {
        self.validate()?;
        if self.checksum_mode || self.customer_key.is_some() {
            return Err(Error::UnsupportedFeature(
                "Checksum-mode or SSE-C reads are not implemented by this object store".to_string(),
            ));
        }
        ObjectReadOptions::for_version(self.version_id.clone())
    }
}

impl From<ObjectReadOptions> for TransferReadOptions {
    fn from(value: ObjectReadOptions) -> Self {
        Self {
            version_id: value.version_id,
            ..Self::default()
        }
    }
}

/// Complete options for a faithful server-side object copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferCopyOptions {
    /// Source version, checksum, and SSE-C selection.
    pub source: TransferReadOptions,
    /// Source metadata handling requested for the destination.
    pub metadata_directive: Option<MetadataDirective>,
    /// Source tag handling requested for the destination.
    pub tagging_directive: Option<TaggingDirective>,
    /// Destination attributes, tags, checksum, encryption, and lock state.
    pub destination: ObjectWriteOptions,
}

impl TransferCopyOptions {
    /// Validate directives and their replacement payloads before mutation.
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        self.destination.validate()?;
        match self.metadata_directive {
            None | Some(MetadataDirective::Copy) if self.destination.attributes.is_some() => {
                return Err(Error::InvalidPath(
                    "Destination attributes require an explicit metadata REPLACE directive"
                        .to_string(),
                ));
            }
            Some(MetadataDirective::Replace) if self.destination.attributes.is_none() => {
                return Err(Error::InvalidPath(
                    "Metadata REPLACE requires an explicit attributes value".to_string(),
                ));
            }
            _ => {}
        }
        match self.tagging_directive {
            None | Some(TaggingDirective::Copy) if self.destination.tags.is_some() => {
                return Err(Error::InvalidPath(
                    "Destination tags require an explicit tagging REPLACE directive".to_string(),
                ));
            }
            Some(TaggingDirective::Replace) if self.destination.tags.is_none() => {
                return Err(Error::InvalidPath(
                    "Tagging REPLACE requires an explicit tags value".to_string(),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn legacy_copy_arguments(
        &self,
    ) -> Result<(CopyObjectOptions, Option<&ObjectEncryptionRequest>)> {
        self.validate()?;
        if self.metadata_directive.is_some() || self.tagging_directive.is_some() {
            return Err(Error::UnsupportedFeature(
                "Metadata or tagging directives are not implemented by this object store"
                    .to_string(),
            ));
        }
        let source = self.source.legacy_read_options()?;
        let encryption = self.destination.legacy_copy_encryption()?;
        let copy = CopyObjectOptions::for_source_version(source.version_id)?;
        Ok((copy, encryption))
    }

    pub(crate) fn validate_multipart_source_version(
        &self,
        multipart_source_version_id: Option<&str>,
    ) -> Result<()> {
        if self.source.version_id.is_some()
            && self.source.version_id.as_deref() != multipart_source_version_id
        {
            return Err(Error::InvalidPath(
                "Transfer and multipart source version IDs must match".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_checksums_accept_valid_composite_values_only() {
        let digest = BASE64_STANDARD.encode([7_u8; 32]);
        let checksum =
            ObjectChecksum::new_persisted(ChecksumAlgorithm::Sha256, format!("{digest}-3"))
                .expect("valid composite checksum");
        assert_eq!(checksum.value, format!("{digest}-3"));

        for value in [
            format!("{digest}-0"),
            format!("{digest}-10001"),
            format!("{digest}-not-a-number"),
        ] {
            assert!(matches!(
                ObjectChecksum::new_persisted(ChecksumAlgorithm::Sha256, value),
                Err(Error::InvalidPath(_))
            ));
        }
        assert!(matches!(
            ObjectChecksum::new_persisted(
                ChecksumAlgorithm::Crc64Nvme,
                format!("{}-2", BASE64_STANDARD.encode([3_u8; 8]))
            ),
            Err(Error::InvalidPath(_))
        ));
    }

    #[test]
    fn advanced_reads_cannot_fall_through_to_legacy_backends() {
        let checksum_read = TransferReadOptions {
            checksum_mode: true,
            ..TransferReadOptions::default()
        };
        assert!(matches!(
            checksum_read.legacy_read_options(),
            Err(Error::UnsupportedFeature(_))
        ));

        let customer_key = SseCustomerKey::new(vec![3; 32]).expect("valid customer key");
        let encrypted_read = TransferReadOptions {
            customer_key: Some(customer_key),
            ..TransferReadOptions::default()
        };
        assert!(matches!(
            encrypted_read.legacy_read_options(),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn explicit_copy_directives_cannot_fall_through_to_legacy_backends() {
        for options in [
            TransferCopyOptions {
                metadata_directive: Some(MetadataDirective::Copy),
                ..TransferCopyOptions::default()
            },
            TransferCopyOptions {
                tagging_directive: Some(TaggingDirective::Copy),
                ..TransferCopyOptions::default()
            },
        ] {
            assert!(matches!(
                options.legacy_copy_arguments(),
                Err(Error::UnsupportedFeature(_))
            ));
        }
    }

    #[test]
    fn multipart_source_versions_must_match_when_transfer_selects_one() {
        let options = TransferCopyOptions {
            source: TransferReadOptions {
                version_id: Some("source-v1".to_string()),
                ..TransferReadOptions::default()
            },
            ..TransferCopyOptions::default()
        };

        options
            .validate_multipart_source_version(Some("source-v1"))
            .expect("matching source versions should be accepted");
        assert!(matches!(
            options.validate_multipart_source_version(Some("source-v2")),
            Err(Error::InvalidPath(_))
        ));
        assert!(matches!(
            options.validate_multipart_source_version(None),
            Err(Error::InvalidPath(_))
        ));
    }
}
