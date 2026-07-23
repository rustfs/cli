use std::collections::HashMap;

use rc_core::{
    ChecksumAlgorithm, ChecksumRequest, Error, LegalHoldStatus, MetadataDirective,
    ObjectAttributes, ObjectChecksum, ObjectEncryptionRequest, ObjectTransferMetadata,
    ObjectWriteEncryption, ObjectWriteOptions, SseCustomerKey, TaggingDirective,
    TransferCopyOptions, TransferReadOptions,
};

#[test]
fn customer_key_requires_exactly_32_bytes_and_redacts_debug_output() {
    let raw_key = b"0123456789abcdef0123456789abcdef".to_vec();
    let key = SseCustomerKey::new(raw_key.clone()).expect("32-byte key should be accepted");

    assert_eq!(key.expose_secret(), raw_key.as_slice());
    let debug = format!("{key:?}");
    assert!(debug.contains("REDACTED"));
    assert!(!debug.contains("0123456789abcdef"));

    assert!(matches!(
        SseCustomerKey::new(vec![0; 31]),
        Err(Error::InvalidPath(_))
    ));
    assert!(matches!(
        SseCustomerKey::new(vec![0; 33]),
        Err(Error::InvalidPath(_))
    ));
}

#[test]
fn write_options_reject_ambiguous_or_empty_values() {
    let empty_storage_class = ObjectWriteOptions {
        storage_class: Some("  ".to_string()),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(
        empty_storage_class.validate(),
        Err(Error::InvalidPath(_))
    ));

    let mut metadata = HashMap::new();
    metadata.insert(String::new(), "value".to_string());
    let empty_metadata_key = ObjectWriteOptions {
        attributes: Some(ObjectAttributes {
            user_metadata: metadata,
            ..ObjectAttributes::default()
        }),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(
        empty_metadata_key.validate(),
        Err(Error::InvalidPath(_))
    ));

    let empty_checksum = ObjectWriteOptions {
        checksum: Some(ChecksumRequest::Precomputed(ObjectChecksum {
            algorithm: ChecksumAlgorithm::Sha256,
            value: String::new(),
        })),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(
        empty_checksum.validate(),
        Err(Error::InvalidPath(_))
    ));

    let empty_kms_key = ObjectWriteOptions {
        encryption: Some(ObjectWriteEncryption::Managed(
            ObjectEncryptionRequest::SseKms {
                key_id: " ".to_string(),
            },
        )),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(
        empty_kms_key.validate(),
        Err(Error::InvalidPath(_))
    ));
}

#[test]
fn attributes_reject_invalid_http_header_names_and_values() {
    for attributes in [
        ObjectAttributes {
            cache_control: Some("max-age=60\r\nx-injected: true".to_string()),
            ..ObjectAttributes::default()
        },
        ObjectAttributes {
            user_metadata: HashMap::from([("bad key".to_string(), "value".to_string())]),
            ..ObjectAttributes::default()
        },
        ObjectAttributes {
            user_metadata: HashMap::from([(
                "safe-key".to_string(),
                "value\nx-injected: true".to_string(),
            )]),
            ..ObjectAttributes::default()
        },
    ] {
        assert!(matches!(attributes.validate(), Err(Error::InvalidPath(_))));
    }
}

#[test]
fn object_tags_enforce_s3_count_and_character_limits() {
    let eleven_tags = (0..11)
        .map(|index| (format!("key-{index}"), String::new()))
        .collect();
    let too_many = ObjectWriteOptions {
        tags: Some(eleven_tags),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(too_many.validate(), Err(Error::InvalidPath(_))));

    for tags in [
        HashMap::from([("k".repeat(129), String::new())]),
        HashMap::from([("key".to_string(), "v".repeat(257))]),
    ] {
        let options = ObjectWriteOptions {
            tags: Some(tags),
            ..ObjectWriteOptions::default()
        };
        assert!(matches!(options.validate(), Err(Error::InvalidPath(_))));
    }

    let boundary = ObjectWriteOptions {
        tags: Some(HashMap::from([("k".repeat(128), "v".repeat(256))])),
        ..ObjectWriteOptions::default()
    };
    boundary
        .validate()
        .expect("maximum tag key and value lengths should be accepted");
}

#[test]
fn copy_options_require_explicit_replace_payloads() {
    let missing_metadata = TransferCopyOptions {
        metadata_directive: Some(MetadataDirective::Replace),
        ..TransferCopyOptions::default()
    };
    assert!(matches!(
        missing_metadata.validate(),
        Err(Error::InvalidPath(_))
    ));

    let missing_tags = TransferCopyOptions {
        tagging_directive: Some(TaggingDirective::Replace),
        ..TransferCopyOptions::default()
    };
    assert!(matches!(
        missing_tags.validate(),
        Err(Error::InvalidPath(_))
    ));

    let explicit_empty_replacements = TransferCopyOptions {
        metadata_directive: Some(MetadataDirective::Replace),
        tagging_directive: Some(TaggingDirective::Replace),
        destination: ObjectWriteOptions {
            attributes: Some(ObjectAttributes::default()),
            tags: Some(HashMap::new()),
            ..ObjectWriteOptions::default()
        },
        ..TransferCopyOptions::default()
    };
    explicit_empty_replacements
        .validate()
        .expect("explicit empty replacement payloads should be valid");
}

#[test]
fn copy_options_reject_values_that_conflict_with_copy_directives() {
    let metadata_conflict = TransferCopyOptions {
        metadata_directive: Some(MetadataDirective::Copy),
        destination: ObjectWriteOptions {
            attributes: Some(ObjectAttributes::default()),
            ..ObjectWriteOptions::default()
        },
        ..TransferCopyOptions::default()
    };
    assert!(matches!(
        metadata_conflict.validate(),
        Err(Error::InvalidPath(_))
    ));

    let tag_conflict = TransferCopyOptions {
        tagging_directive: Some(TaggingDirective::Copy),
        destination: ObjectWriteOptions {
            tags: Some(HashMap::new()),
            ..ObjectWriteOptions::default()
        },
        ..TransferCopyOptions::default()
    };
    assert!(matches!(
        tag_conflict.validate(),
        Err(Error::InvalidPath(_))
    ));

    let implicit_metadata_replace = TransferCopyOptions {
        destination: ObjectWriteOptions {
            attributes: Some(ObjectAttributes::default()),
            ..ObjectWriteOptions::default()
        },
        ..TransferCopyOptions::default()
    };
    assert!(matches!(
        implicit_metadata_replace.validate(),
        Err(Error::InvalidPath(_))
    ));

    let implicit_tag_replace = TransferCopyOptions {
        destination: ObjectWriteOptions {
            tags: Some(HashMap::new()),
            ..ObjectWriteOptions::default()
        },
        ..TransferCopyOptions::default()
    };
    assert!(matches!(
        implicit_tag_replace.validate(),
        Err(Error::InvalidPath(_))
    ));
}

#[test]
fn read_options_reject_an_empty_version() {
    let options = TransferReadOptions {
        version_id: Some(String::new()),
        ..TransferReadOptions::default()
    };

    assert!(matches!(options.validate(), Err(Error::InvalidPath(_))));
}

#[test]
fn advanced_write_options_are_not_legacy_compatible() {
    let basic = ObjectWriteOptions {
        attributes: Some(ObjectAttributes {
            content_type: Some("text/plain".to_string()),
            ..ObjectAttributes::default()
        }),
        ..ObjectWriteOptions::default()
    };
    basic
        .legacy_put_arguments()
        .expect("content type should map to the legacy put API");

    let advanced = ObjectWriteOptions {
        legal_hold: Some(LegalHoldStatus::On),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(
        advanced.legacy_put_arguments(),
        Err(Error::UnsupportedFeature(_))
    ));
}

#[test]
fn customer_encryption_is_never_legacy_compatible() {
    let key = SseCustomerKey::new(vec![7; 32]).expect("valid key");
    let options = ObjectWriteOptions {
        encryption: Some(ObjectWriteEncryption::SseCustomer { key }),
        ..ObjectWriteOptions::default()
    };

    assert!(matches!(
        options.legacy_put_arguments(),
        Err(Error::UnsupportedFeature(_))
    ));
}

#[test]
fn nested_transfer_options_never_expose_customer_keys() {
    let raw_key = b"0123456789abcdef0123456789abcdef";
    let key = SseCustomerKey::new(raw_key.to_vec()).expect("valid key");
    let write = ObjectWriteOptions {
        encryption: Some(ObjectWriteEncryption::SseCustomer { key: key.clone() }),
        ..ObjectWriteOptions::default()
    };
    let copy = TransferCopyOptions {
        source: TransferReadOptions {
            customer_key: Some(key),
            ..TransferReadOptions::default()
        },
        destination: write.clone(),
        ..TransferCopyOptions::default()
    };

    for debug in [format!("{write:?}"), format!("{copy:?}")] {
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("0123456789abcdef"));
    }
}

#[test]
fn checksum_requests_distinguish_calculation_from_precomputed_values() {
    let calculate = ObjectWriteOptions {
        checksum: Some(ChecksumRequest::Calculate(ChecksumAlgorithm::Sha256)),
        ..ObjectWriteOptions::default()
    };
    calculate
        .validate()
        .expect("calculated checksum request should not require a value");

    let precomputed = ObjectWriteOptions {
        checksum: Some(ChecksumRequest::Precomputed(ObjectChecksum {
            algorithm: ChecksumAlgorithm::Sha256,
            value: "  ".to_string(),
        })),
        ..ObjectWriteOptions::default()
    };
    assert!(matches!(precomputed.validate(), Err(Error::InvalidPath(_))));

    for value in ["not-base64", "AA=="] {
        assert!(matches!(
            ObjectChecksum::new(ChecksumAlgorithm::Crc32, value),
            Err(Error::InvalidPath(_))
        ));
    }
    assert_eq!(
        ObjectChecksum::new(ChecksumAlgorithm::Crc32, "AAAAAA==")
            .expect("four encoded checksum bytes")
            .value,
        "AAAAAA=="
    );
}

#[test]
fn transfer_metadata_default_is_explicitly_empty() {
    let metadata = ObjectTransferMetadata::default();

    assert_eq!(metadata.attributes, ObjectAttributes::default());
    assert!(metadata.storage_class.is_none());
    assert!(metadata.checksums.is_empty());
}
