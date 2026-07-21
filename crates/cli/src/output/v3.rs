//! Shared JSON output v3 envelopes for newly versioned command families.

use serde::Serialize;

use crate::exit_code::ExitCode;

const VERSIONED_OBJECTS_FAMILY: &str = "versioned_objects";

#[derive(Debug, Serialize)]
pub struct V3SuccessEnvelope<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    family: &'static str,
    status: &'static str,
    data: T,
}

impl<T> V3SuccessEnvelope<T> {
    pub fn versioned_objects(data: T) -> Self {
        Self {
            schema_version: 3,
            family: VERSIONED_OBJECTS_FAMILY,
            status: "success",
            data,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct V3ErrorEnvelope {
    schema_version: u8,
    #[serde(rename = "type")]
    family: &'static str,
    status: &'static str,
    error: V3ErrorDetail,
}

impl V3ErrorEnvelope {
    pub fn versioned_objects(
        code: ExitCode,
        message: impl Into<String>,
        capability: Option<&str>,
    ) -> Self {
        Self {
            schema_version: 3,
            family: VERSIONED_OBJECTS_FAMILY,
            status: "error",
            error: V3ErrorDetail::from_exit_code(code, message.into(), capability),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct V3PartialErrorEnvelope<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    family: &'static str,
    status: &'static str,
    error: V3ErrorDetail,
    data: T,
}

impl<T> V3PartialErrorEnvelope<T> {
    pub fn versioned_objects(
        code: ExitCode,
        message: impl Into<String>,
        capability: Option<&str>,
        data: T,
    ) -> Self {
        Self {
            schema_version: 3,
            family: VERSIONED_OBJECTS_FAMILY,
            status: "error",
            error: V3ErrorDetail::from_exit_code(code, message.into(), capability),
            data,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum V3ErrorDetail {
    Standard(V3StandardError),
    Unsupported(V3UnsupportedError),
}

impl V3ErrorDetail {
    fn from_exit_code(code: ExitCode, message: String, capability: Option<&str>) -> Self {
        let (error_type, retryable, suggestion) = match code {
            ExitCode::UnsupportedFeature => {
                return Self::Unsupported(V3UnsupportedError {
                    error_type: "unsupported_feature",
                    message,
                    retryable: false,
                    capability: capability.unwrap_or("versioned_objects").to_string(),
                    server: None,
                    suggestion: Some(
                        "Verify that the target RustFS version supports this operation."
                            .to_string(),
                    ),
                });
            }
            ExitCode::Success => ("general_error", false, None),
            ExitCode::GeneralError => ("general_error", false, None),
            ExitCode::UsageError => (
                "usage_error",
                false,
                Some("Review the command arguments and retry.".to_string()),
            ),
            ExitCode::NetworkError => (
                "network_error",
                true,
                Some("Verify the endpoint and network connectivity, then retry.".to_string()),
            ),
            ExitCode::AuthError => (
                "auth_error",
                false,
                Some("Verify the alias credentials and permissions, then retry.".to_string()),
            ),
            ExitCode::NotFound => (
                "not_found",
                false,
                Some("Check the bucket, object key, and version ID, then retry.".to_string()),
            ),
            ExitCode::Conflict => (
                "conflict",
                false,
                Some("Review the version state or retention policy, then retry.".to_string()),
            ),
            ExitCode::Interrupted => (
                "interrupted",
                true,
                Some("Retry if the operation still needs to complete.".to_string()),
            ),
        };

        Self::Standard(V3StandardError {
            error_type,
            message,
            retryable,
            suggestion,
        })
    }
}

#[derive(Debug, Serialize)]
struct V3StandardError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
struct V3UnsupportedError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: String,
    server: Option<String>,
    suggestion: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_success_uses_the_v3_envelope() {
        let value = serde_json::to_value(V3SuccessEnvelope::versioned_objects(
            serde_json::json!({ "operation": "copy" }),
        ))
        .expect("serialize v3 success envelope");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["type"], "versioned_objects");
        assert_eq!(value["status"], "success");
    }

    #[test]
    fn versioned_errors_use_stable_error_kinds() {
        let value = serde_json::to_value(V3ErrorEnvelope::versioned_objects(
            ExitCode::AuthError,
            "Access denied",
            None,
        ))
        .expect("serialize v3 error envelope");

        assert_eq!(value["status"], "error");
        assert_eq!(value["error"]["type"], "auth_error");
        assert_eq!(value["error"]["retryable"], false);
    }
}
