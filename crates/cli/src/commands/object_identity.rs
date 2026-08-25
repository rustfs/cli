//! Shared source-identity metadata for copy-style commands.
//!
//! A remote-to-remote transfer that streams through the client cannot preserve
//! the source ETag: the destination computes its own, and multipart completion
//! makes it differ even when the bytes are identical. Commands therefore record
//! the source ETag in user metadata so a later run can recognize an unchanged
//! object. `mirror`, `cp`, and `diff` must agree on this key and on how it is
//! read back, otherwise one command re-copies what another already migrated.

use std::collections::HashMap;

use rc_core::ObjectAttributes;

/// User-metadata key holding the source ETag of a client-streamed copy.
///
/// Stored without the `x-amz-meta-` prefix; the S3 layer adds it on the wire.
pub(super) const SOURCE_IDENTITY_METADATA_KEY: &str = "rc-source-etag";

/// Read the recorded source ETag from object user metadata.
///
/// Backends differ on whether they echo the `x-amz-meta-` prefix and on header
/// case, so both are normalized. An empty value is treated as absent because it
/// cannot identify a source object.
pub(super) fn identity_etag_from_metadata(
    metadata: Option<&HashMap<String, String>>,
) -> Option<String> {
    metadata.and_then(|metadata| {
        metadata.iter().find_map(|(key, value)| {
            let normalized = key.to_ascii_lowercase();
            let key = normalized
                .strip_prefix("x-amz-meta-")
                .unwrap_or(normalized.as_str());
            (key == SOURCE_IDENTITY_METADATA_KEY && !value.is_empty()).then(|| value.clone())
        })
    })
}

/// Record `identity_etag` on a destination write.
///
/// This is bookkeeping owned by `rc` rather than user data, so it is applied
/// even when the caller replaces user metadata. Without it, a later incremental
/// run could not tell a faithful copy from a changed object.
pub(super) fn set_source_identity(attributes: &mut ObjectAttributes, identity_etag: &str) {
    if identity_etag.is_empty() {
        return;
    }
    attributes.user_metadata.insert(
        SOURCE_IDENTITY_METADATA_KEY.to_string(),
        identity_etag.to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_etag_is_read_through_prefix_and_case_variants() {
        for key in [
            "rc-source-etag",
            "Rc-Source-Etag",
            "x-amz-meta-rc-source-etag",
            "X-Amz-Meta-Rc-Source-Etag",
        ] {
            let metadata = HashMap::from([(key.to_string(), "source-etag".to_string())]);
            assert_eq!(
                identity_etag_from_metadata(Some(&metadata)).as_deref(),
                Some("source-etag"),
                "{key} should resolve the identity ETag"
            );
        }
    }

    #[test]
    fn absent_empty_and_unrelated_metadata_have_no_identity() {
        assert_eq!(identity_etag_from_metadata(None), None);
        assert_eq!(
            identity_etag_from_metadata(Some(&HashMap::new())),
            None,
            "empty metadata has no identity"
        );
        assert_eq!(
            identity_etag_from_metadata(Some(&HashMap::from([(
                "rc-source-etag".to_string(),
                String::new(),
            )]))),
            None,
            "an empty value cannot identify a source object"
        );
        assert_eq!(
            identity_etag_from_metadata(Some(&HashMap::from([(
                "owner".to_string(),
                "storage".to_string(),
            )]))),
            None
        );
    }

    #[test]
    fn set_source_identity_records_the_key_without_the_wire_prefix() {
        let mut attributes = ObjectAttributes::default();
        set_source_identity(&mut attributes, "source-etag");

        assert_eq!(
            attributes.user_metadata.get(SOURCE_IDENTITY_METADATA_KEY),
            Some(&"source-etag".to_string())
        );
        assert_eq!(
            identity_etag_from_metadata(Some(&attributes.user_metadata)).as_deref(),
            Some("source-etag"),
            "what one command writes another must be able to read"
        );
    }

    #[test]
    fn set_source_identity_preserves_unrelated_user_metadata() {
        let mut attributes = ObjectAttributes {
            user_metadata: HashMap::from([("owner".to_string(), "storage".to_string())]),
            ..ObjectAttributes::default()
        };
        set_source_identity(&mut attributes, "source-etag");

        assert_eq!(
            attributes.user_metadata.get("owner"),
            Some(&"storage".to_string())
        );
        assert_eq!(attributes.user_metadata.len(), 2);
    }

    #[test]
    fn set_source_identity_ignores_an_empty_etag() {
        let mut attributes = ObjectAttributes::default();
        set_source_identity(&mut attributes, "");

        assert!(
            attributes.user_metadata.is_empty(),
            "an empty ETag must not create an unreadable identity entry"
        );
    }
}
