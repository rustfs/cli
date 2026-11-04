//! Bucket-level controls that reject public S3 access policies.

use serde::{Deserialize, Serialize};

/// S3 Public Access Block settings for one bucket.
///
/// Every flag is explicit so a partial configuration cannot silently weaken
/// the requested access boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicAccessBlockConfiguration {
    /// Reject bucket and object ACLs that grant public access.
    pub block_public_acls: bool,
    /// Ignore public grants in existing bucket and object ACLs.
    pub ignore_public_acls: bool,
    /// Reject bucket policies that grant public access.
    pub block_public_policy: bool,
    /// Restrict access to buckets with public policies to authorized AWS services and principals.
    pub restrict_public_buckets: bool,
}

impl PublicAccessBlockConfiguration {
    /// Return the restrictive configuration used for bootstrap-created buckets.
    pub const fn secure_default() -> Self {
        Self {
            block_public_acls: true,
            ignore_public_acls: true,
            block_public_policy: true,
            restrict_public_buckets: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secure_default_blocks_all_public_access_paths() {
        assert_eq!(
            PublicAccessBlockConfiguration::secure_default(),
            PublicAccessBlockConfiguration {
                block_public_acls: true,
                ignore_public_acls: true,
                block_public_policy: true,
                restrict_public_buckets: true,
            }
        );
    }

    #[test]
    fn configuration_requires_every_flag() {
        let error = serde_json::from_value::<PublicAccessBlockConfiguration>(json!({
            "block_public_acls": true,
            "ignore_public_acls": true,
            "block_public_policy": true
        }))
        .expect_err("missing flag must not default to permissive access");

        assert!(error.to_string().contains("restrict_public_buckets"));
    }

    #[test]
    fn configuration_rejects_unknown_flags() {
        let error = serde_json::from_value::<PublicAccessBlockConfiguration>(json!({
            "block_public_acls": true,
            "ignore_public_acls": true,
            "block_public_policy": true,
            "restrict_public_buckets": true,
            "unknown": true
        }))
        .expect_err("unknown flags must fail closed");

        assert!(error.to_string().contains("unknown field"));
    }
}
