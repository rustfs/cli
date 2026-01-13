//! Capability detection for S3 backends
//!
//! Different S3-compatible backends support different features.
//! This module provides capability detection to gracefully handle
//! unsupported features.

use rc_core::{Capabilities, Error, Result};

/// Detect capabilities of an S3 backend
///
/// This function probes the backend to determine which features are supported.
/// For optional commands, we use this to determine whether to proceed or
/// return EXIT_UNSUPPORTED_FEATURE.
pub async fn detect_capabilities(
    client: &aws_sdk_s3::Client,
    bucket: &str,
) -> Result<Capabilities> {
    // Note: Object Lock and S3 Select detection would require additional probes
    // that might have side effects. For now, we default to false and let users
    // use --force if they know their backend supports these features.
    let caps = Capabilities {
        versioning: check_versioning(client, bucket).await,
        tagging: check_tagging(client, bucket).await,
        ..Default::default()
    };

    Ok(caps)
}

/// Check if bucket versioning is supported
async fn check_versioning(client: &aws_sdk_s3::Client, bucket: &str) -> bool {
    // Try to get versioning configuration
    // If we get a successful response (even if versioning is not enabled),
    // the backend supports versioning
    client
        .get_bucket_versioning()
        .bucket(bucket)
        .send()
        .await
        .is_ok()
}

/// Check if object tagging is supported
async fn check_tagging(client: &aws_sdk_s3::Client, bucket: &str) -> bool {
    // Try to get bucket tagging
    // Even if no tags are set, a supported backend will return a valid response
    // or a specific "no tags" error, not an unsupported operation error
    match client.get_bucket_tagging().bucket(bucket).send().await {
        Ok(_) => true,
        Err(e) => {
            let err = e.into_service_error();
            // NoSuchTagSet means tagging is supported, just no tags set
            // AccessDenied might mean we don't have permission but feature exists
            !err.to_string().contains("NotImplemented")
        }
    }
}

/// Check if a specific operation is supported, returning appropriate error
pub fn require_capability(caps: &Capabilities, feature: &str) -> Result<()> {
    let supported = match feature {
        "versioning" => caps.versioning,
        "object_lock" | "retention" => caps.object_lock,
        "tagging" => caps.tagging,
        "select" | "sql" => caps.select,
        "notifications" | "watch" => caps.notifications,
        _ => false,
    };

    if supported {
        Ok(())
    } else {
        Err(Error::UnsupportedFeature(format!(
            "The backend does not support '{feature}'. Use --force to attempt anyway."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_require_capability_versioning() {
        let caps = Capabilities {
            versioning: true,
            ..Default::default()
        };
        assert!(require_capability(&caps, "versioning").is_ok());

        let caps = Capabilities {
            versioning: false,
            ..Default::default()
        };
        assert!(require_capability(&caps, "versioning").is_err());
    }

    #[test]
    fn test_require_capability_unknown() {
        let caps = Capabilities::default();
        assert!(require_capability(&caps, "unknown_feature").is_err());
    }
}
