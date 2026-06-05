//! Bucket and object encryption domain types.

use serde::{Deserialize, Serialize};

/// Bucket default encryption configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BucketEncryption {
    /// Use S3-managed keys.
    SseS3,
    /// Use KMS-managed keys with an optional explicit key id.
    SseKms {
        #[serde(skip_serializing_if = "Option::is_none")]
        key_id: Option<String>,
    },
}

/// Object write encryption request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectEncryptionRequest {
    /// Use S3-managed keys.
    SseS3,
    /// Use KMS-managed keys with the provided key id.
    SseKms { key_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bucket_encryption_serializes_sse_s3() {
        let json = serde_json::to_value(&BucketEncryption::SseS3).expect("serialize sse-s3");
        assert_eq!(json, json!("sse-s3"));
    }

    #[test]
    fn bucket_encryption_serializes_sse_kms_key_id() {
        let json = serde_json::to_value(&BucketEncryption::SseKms {
            key_id: Some("kms-key".to_string()),
        })
        .expect("serialize sse-kms");
        assert_eq!(json, json!({ "sse-kms": { "key_id": "kms-key" } }));
    }

    #[test]
    fn bucket_encryption_serializes_sse_kms_without_key_id() {
        let json = serde_json::to_value(&BucketEncryption::SseKms { key_id: None })
            .expect("serialize sse-kms without key");
        assert_eq!(json, json!({ "sse-kms": {} }));
    }

    #[test]
    fn bucket_encryption_round_trips_sse_s3() {
        let value: BucketEncryption =
            serde_json::from_value(json!("sse-s3")).expect("deserialize sse-s3");
        assert_eq!(value, BucketEncryption::SseS3);
    }

    #[test]
    fn bucket_encryption_round_trips_sse_kms() {
        let value: BucketEncryption =
            serde_json::from_value(json!({ "sse-kms": { "key_id": "kms-key" } }))
                .expect("deserialize sse-kms");
        assert_eq!(
            value,
            BucketEncryption::SseKms {
                key_id: Some("kms-key".to_string()),
            }
        );
    }

    #[test]
    fn bucket_encryption_round_trips_sse_kms_without_key_id() {
        let value: BucketEncryption = serde_json::from_value(json!({ "sse-kms": {} }))
            .expect("deserialize sse-kms without key");
        assert_eq!(value, BucketEncryption::SseKms { key_id: None });
    }

    #[test]
    fn object_encryption_request_serializes_sse_s3() {
        let json =
            serde_json::to_value(&ObjectEncryptionRequest::SseS3).expect("serialize object sse-s3");
        assert_eq!(json, json!("sse-s3"));
    }

    #[test]
    fn object_encryption_request_serializes_sse_kms() {
        let json = serde_json::to_value(&ObjectEncryptionRequest::SseKms {
            key_id: "kms-key".to_string(),
        })
        .expect("serialize object sse-kms");
        assert_eq!(json, json!({ "sse-kms": { "key_id": "kms-key" } }));
    }

    #[test]
    fn object_encryption_request_round_trips_sse_s3() {
        let value: ObjectEncryptionRequest =
            serde_json::from_value(json!("sse-s3")).expect("deserialize object sse-s3");
        assert_eq!(value, ObjectEncryptionRequest::SseS3);
    }

    #[test]
    fn object_encryption_request_round_trips_sse_kms() {
        let value: ObjectEncryptionRequest =
            serde_json::from_value(json!({ "sse-kms": { "key_id": "kms-key" } }))
                .expect("deserialize object sse-kms");
        assert_eq!(
            value,
            ObjectEncryptionRequest::SseKms {
                key_id: "kms-key".to_string(),
            }
        );
    }
}
