use std::collections::HashMap;

use clap::{Args, ValueEnum};
use jiff::Timestamp;
use rc_core::{
    ChecksumAlgorithm, ChecksumRequest, Error, LegalHoldStatus, ObjectAttributes,
    ObjectEncryptionRequest, ObjectRetention, ObjectWriteEncryption, ObjectWriteOptions,
    RetentionMode, SseCustomerKey,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum MetadataDirectiveArg {
    Copy,
    Replace,
}

impl From<MetadataDirectiveArg> for rc_core::MetadataDirective {
    fn from(value: MetadataDirectiveArg) -> Self {
        match value {
            MetadataDirectiveArg::Copy => Self::Copy,
            MetadataDirectiveArg::Replace => Self::Replace,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum TaggingDirectiveArg {
    Copy,
    Replace,
}

impl From<TaggingDirectiveArg> for rc_core::TaggingDirective {
    fn from(value: TaggingDirectiveArg) -> Self {
        match value {
            TaggingDirectiveArg::Copy => Self::Copy,
            TaggingDirectiveArg::Replace => Self::Replace,
        }
    }
}

/// Destination object policies shared by copy and pipe uploads.
#[derive(Args, Clone, Debug, Default)]
pub(crate) struct TransferFidelityArgs {
    /// Cache policy stored with the destination object
    #[arg(long)]
    pub cache_control: Option<String>,

    /// Content-Disposition stored with the destination object
    #[arg(long)]
    pub content_disposition: Option<String>,

    /// Content-Encoding stored with the destination object
    #[arg(long)]
    pub content_encoding: Option<String>,

    /// Content-Language stored with the destination object
    #[arg(long)]
    pub content_language: Option<String>,

    /// RFC3339 expiry timestamp stored with the destination object
    #[arg(long)]
    pub expires: Option<String>,

    /// User metadata as KEY=VALUE; may be repeated
    #[arg(long = "metadata", value_name = "KEY=VALUE")]
    pub metadata: Vec<String>,

    /// Replace destination metadata with an explicit empty set
    #[arg(long, conflicts_with = "metadata")]
    pub clear_metadata: bool,

    /// Object tag as KEY=VALUE; may be repeated
    #[arg(long = "tags", value_name = "KEY=VALUE")]
    pub tags: Vec<String>,

    /// Replace destination tags with an explicit empty set
    #[arg(long, conflicts_with = "tags")]
    pub clear_tags: bool,

    /// Calculate and verify the destination checksum (SHA256 on RustFS beta.10)
    #[arg(long, value_name = "ALGORITHM")]
    pub checksum: Option<String>,

    /// Object retention mode (GOVERNANCE or COMPLIANCE)
    #[arg(long, requires = "retain_until")]
    pub retention_mode: Option<String>,

    /// RFC3339 UTC object retention deadline
    #[arg(long, requires = "retention_mode")]
    pub retain_until: Option<String>,

    /// Destination legal-hold state (ON or OFF)
    #[arg(long, value_name = "ON|OFF")]
    pub legal_hold: Option<String>,
}

impl TransferFidelityArgs {
    pub(crate) fn has_write_policy(&self) -> bool {
        self.cache_control.is_some()
            || self.content_disposition.is_some()
            || self.content_encoding.is_some()
            || self.content_language.is_some()
            || self.expires.is_some()
            || !self.metadata.is_empty()
            || self.clear_metadata
            || !self.tags.is_empty()
            || self.clear_tags
            || self.checksum.is_some()
            || self.retention_mode.is_some()
            || self.retain_until.is_some()
            || self.legal_hold.is_some()
    }

    pub(crate) fn has_attribute_policy(&self) -> bool {
        self.cache_control.is_some()
            || self.content_disposition.is_some()
            || self.content_encoding.is_some()
            || self.content_language.is_some()
            || self.expires.is_some()
            || !self.metadata.is_empty()
            || self.clear_metadata
    }

    pub(crate) fn has_tag_policy(&self) -> bool {
        !self.tags.is_empty() || self.clear_tags
    }

    pub(crate) fn build_write_options(
        &self,
        content_type: Option<&str>,
        encryption: Option<&ObjectEncryptionRequest>,
        customer_key: Option<&SseCustomerKey>,
        storage_class: Option<String>,
    ) -> rc_core::Result<ObjectWriteOptions> {
        let user_metadata = parse_assignments(&self.metadata, "metadata", true)?;
        let tags = parse_assignments(&self.tags, "tag", false)?;
        let expires = self
            .expires
            .as_deref()
            .map(parse_utc_timestamp)
            .transpose()?;
        let has_attributes = content_type.is_some()
            || self.cache_control.is_some()
            || self.content_disposition.is_some()
            || self.content_encoding.is_some()
            || self.content_language.is_some()
            || expires.is_some()
            || !user_metadata.is_empty()
            || self.clear_metadata;
        let attributes = has_attributes.then(|| ObjectAttributes {
            content_type: content_type.map(ToString::to_string),
            cache_control: self.cache_control.clone(),
            content_disposition: self.content_disposition.clone(),
            content_encoding: self.content_encoding.clone(),
            content_language: self.content_language.clone(),
            expires,
            user_metadata,
        });
        let tags = (!tags.is_empty() || self.clear_tags).then_some(tags);
        let checksum = self.checksum.as_deref().map(parse_checksum).transpose()?;
        let retention =
            parse_retention(self.retention_mode.as_deref(), self.retain_until.as_deref())?;
        let legal_hold = self
            .legal_hold
            .as_deref()
            .map(parse_legal_hold)
            .transpose()?;
        let options = ObjectWriteOptions {
            attributes,
            tags,
            storage_class,
            checksum,
            encryption: customer_key
                .cloned()
                .map(|key| ObjectWriteEncryption::SseCustomer { key })
                .or_else(|| encryption.cloned().map(ObjectWriteEncryption::Managed)),
            retention,
            legal_hold,
        };
        options.validate()?;
        Ok(options)
    }
}

fn parse_assignments(
    values: &[String],
    kind: &str,
    normalize_key: bool,
) -> rc_core::Result<HashMap<String, String>> {
    let mut parsed = HashMap::with_capacity(values.len());
    for value in values {
        let (key, value) = value.split_once('=').ok_or_else(|| {
            Error::InvalidPath(format!("Invalid {kind} '{value}'; expected KEY=VALUE"))
        })?;
        if key.is_empty() {
            return Err(Error::InvalidPath(format!("{kind} keys cannot be empty")));
        }
        let key = if normalize_key {
            key.to_ascii_lowercase()
        } else {
            key.to_string()
        };
        if parsed.insert(key.clone(), value.to_string()).is_some() {
            return Err(Error::InvalidPath(format!("Duplicate {kind} key '{key}'")));
        }
    }
    Ok(parsed)
}

fn parse_utc_timestamp(value: &str) -> rc_core::Result<Timestamp> {
    if !value.ends_with('Z') {
        return Err(Error::InvalidPath(
            "Object timestamps must be RFC3339 UTC values ending in 'Z'".to_string(),
        ));
    }
    value
        .parse()
        .map_err(|error| Error::InvalidPath(format!("Invalid object timestamp '{value}': {error}")))
}

fn parse_checksum(value: &str) -> rc_core::Result<ChecksumRequest> {
    match value.to_ascii_uppercase().as_str() {
        "SHA256" | "SHA-256" => Ok(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
        "CRC32" | "CRC32C" | "CRC64NVME" | "SHA1" | "SHA-1" => Err(Error::UnsupportedFeature(
            format!("RustFS beta.10 checksum writes do not support '{value}'"),
        )),
        _ => Err(Error::InvalidPath(format!(
            "Unknown checksum algorithm '{value}'"
        ))),
    }
}

fn parse_retention(
    mode: Option<&str>,
    retain_until: Option<&str>,
) -> rc_core::Result<Option<ObjectRetention>> {
    match (mode, retain_until) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(Error::InvalidPath(
            "--retention-mode and --retain-until must be provided together".to_string(),
        )),
        (Some(mode), Some(retain_until)) => {
            let mode = match mode.to_ascii_uppercase().as_str() {
                "GOVERNANCE" => RetentionMode::Governance,
                "COMPLIANCE" => RetentionMode::Compliance,
                _ => {
                    return Err(Error::InvalidPath(format!(
                        "Unknown retention mode '{mode}'"
                    )));
                }
            };
            Ok(Some(ObjectRetention {
                mode,
                retain_until: parse_utc_timestamp(retain_until)?,
            }))
        }
    }
}

fn parse_legal_hold(value: &str) -> rc_core::Result<LegalHoldStatus> {
    match value.to_ascii_uppercase().as_str() {
        "ON" => Ok(LegalHoldStatus::On),
        "OFF" => Ok(LegalHoldStatus::Off),
        _ => Err(Error::InvalidPath(format!(
            "Unknown legal-hold state '{value}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use rc_core::{ChecksumAlgorithm, ChecksumRequest, LegalHoldStatus, RetentionMode};

    use super::*;

    #[test]
    fn write_options_parse_all_supported_non_secret_policies() {
        let args = TransferFidelityArgs {
            cache_control: Some("max-age=3600".to_string()),
            content_disposition: Some("attachment".to_string()),
            content_encoding: Some("gzip".to_string()),
            content_language: Some("en-US".to_string()),
            expires: Some("2030-01-02T03:04:05Z".to_string()),
            metadata: vec!["owner=analytics".to_string(), "empty=".to_string()],
            clear_metadata: false,
            tags: vec!["env=prod".to_string(), "empty=".to_string()],
            clear_tags: false,
            checksum: Some("SHA256".to_string()),
            retention_mode: Some("GOVERNANCE".to_string()),
            retain_until: Some("2099-01-02T03:04:05Z".to_string()),
            legal_hold: Some("ON".to_string()),
        };

        let options = args
            .build_write_options(
                Some("application/json"),
                None,
                None,
                Some("STANDARD".to_string()),
            )
            .expect("valid fidelity policy");
        let attributes = options.attributes.expect("attributes");
        assert_eq!(attributes.content_type.as_deref(), Some("application/json"));
        assert_eq!(attributes.cache_control.as_deref(), Some("max-age=3600"));
        assert_eq!(
            attributes.expires,
            Some(
                "2030-01-02T03:04:05Z"
                    .parse::<Timestamp>()
                    .expect("valid timestamp")
            )
        );
        assert_eq!(
            attributes.user_metadata.get("owner").map(String::as_str),
            Some("analytics")
        );
        assert_eq!(
            options
                .tags
                .as_ref()
                .and_then(|tags| tags.get("env"))
                .map(String::as_str),
            Some("prod")
        );
        assert_eq!(
            options.checksum,
            Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256))
        );
        let retention = options.retention.expect("retention");
        assert_eq!(retention.mode, RetentionMode::Governance);
        assert_eq!(options.legal_hold, Some(LegalHoldStatus::On));
    }

    #[test]
    fn write_options_reject_ambiguous_or_unsupported_values() {
        for args in [
            TransferFidelityArgs {
                retention_mode: Some("GOVERNANCE".to_string()),
                ..TransferFidelityArgs::default()
            },
            TransferFidelityArgs {
                retain_until: Some("2031-01-02T03:04:05Z".to_string()),
                ..TransferFidelityArgs::default()
            },
            TransferFidelityArgs {
                metadata: vec!["missing-separator".to_string()],
                ..TransferFidelityArgs::default()
            },
            TransferFidelityArgs {
                tags: vec!["duplicate=one".to_string(), "duplicate=two".to_string()],
                ..TransferFidelityArgs::default()
            },
        ] {
            assert!(args.build_write_options(None, None, None, None).is_err());
        }

        let unsupported = TransferFidelityArgs {
            checksum: Some("CRC32".to_string()),
            ..TransferFidelityArgs::default()
        }
        .build_write_options(None, None, None, None)
        .expect_err("beta10 checksum boundary");
        assert!(matches!(unsupported, rc_core::Error::UnsupportedFeature(_)));
    }
}
