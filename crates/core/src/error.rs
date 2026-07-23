//! Error types for rc-core
//!
//! Provides a unified error type that can be converted to appropriate exit codes.

use thiserror::Error;

/// Outcome of cleaning up a multipart upload after a copy failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartAbortStatus {
    /// The service accepted the abort request.
    Succeeded,
    /// The abort request failed; the upload may require later cleanup.
    Failed,
}

impl std::fmt::Display for MultipartAbortStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Succeeded => formatter.write_str("succeeded"),
            Self::Failed => formatter.write_str("failed"),
        }
    }
}

/// Result type alias for rc-core operations
pub type Result<T> = std::result::Result<T, Error>;

/// Error types for rc-core operations
#[derive(Error, Debug)]
pub enum Error {
    /// Configuration file error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Invalid path format
    #[error("Invalid path: {0}")]
    InvalidPath(String),

    /// Alias not found
    #[error("Alias not found: {0}")]
    AliasNotFound(String),

    /// Alias already exists
    #[error("Alias already exists: {0}")]
    AliasExists(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// TOML parsing error
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    /// TOML serialization error
    #[error("TOML serialization error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    /// JSON error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// URL parsing error
    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// Authentication error
    #[error("Authentication failed: {0}")]
    Auth(String),

    /// Resource not found
    #[error("Not found: {0}")]
    NotFound(String),

    /// A requested historical object version does not exist.
    #[error("Object version '{version_id}' not found: {path}")]
    VersionNotFound {
        /// Object path.
        path: String,
        /// Requested version identifier.
        version_id: String,
    },

    /// A read or metadata request selected a delete marker instead of object data.
    #[error("Object version '{version_id}' is a delete marker: {path}")]
    DeleteMarker {
        /// Object path.
        path: String,
        /// Delete marker version identifier.
        version_id: String,
    },

    /// Object Lock governance retention rejected a delete request.
    #[error("Governance retention denied deletion: {path} (version: {version_id:?})")]
    GovernanceDenied {
        /// Object path.
        path: String,
        /// Requested version identifier, when one was supplied.
        version_id: Option<String>,
    },

    /// Network error (retryable)
    #[error("Network error: {0}")]
    Network(String),

    /// Conflict error
    #[error("Conflict: {0}")]
    Conflict(String),

    /// Feature not supported by backend
    #[error("Unsupported feature: {0}")]
    UnsupportedFeature(String),

    /// General error
    #[error("{0}")]
    General(String),

    /// Request rejected locally before any network operation was attempted
    #[error("{0}")]
    RequestRejected(String),

    /// Operation stopped through an explicit cooperative cancellation request.
    #[error("Operation interrupted: {0}")]
    Interrupted(String),
}

impl Error {
    /// Get the appropriate exit code for this error
    pub const fn exit_code(&self) -> i32 {
        match self {
            Error::InvalidPath(_) => 2, // UsageError
            Error::Config(_) => 2,      // UsageError
            Error::Network(_) => 3,     // NetworkError
            Error::Auth(_) => 4,        // AuthError
            Error::NotFound(_)
            | Error::VersionNotFound { .. }
            | Error::DeleteMarker { .. }
            | Error::AliasNotFound(_) => 5, // NotFound
            Error::Conflict(_) | Error::GovernanceDenied { .. } | Error::AliasExists(_) => 6, // Conflict
            Error::UnsupportedFeature(_) => 7, // UnsupportedFeature
            Error::Interrupted(_) => 130,      // Interrupted
            Error::RequestRejected(_) => 1,    // GeneralError
            _ => 1,                            // GeneralError
        }
    }

    /// Add multipart cleanup diagnostics without changing the primary error class.
    pub fn with_multipart_copy_context(
        self,
        upload_id: &str,
        abort_status: MultipartAbortStatus,
    ) -> Self {
        // Upload IDs originate at the service boundary. Escaping and bounding
        // them prevents terminal control sequences or oversized diagnostics.
        let safe_upload_id = upload_id
            .chars()
            .flat_map(char::escape_default)
            .take(256)
            .collect::<String>();
        let context = format!("multipart upload ID: {safe_upload_id}, abort: {abort_status}");
        match self {
            Self::Auth(message) => Self::Auth(format!("{message}; {context}")),
            Self::Network(message) => Self::Network(format!("{message}; {context}")),
            Self::NotFound(message) => Self::NotFound(format!("{message}; {context}")),
            Self::VersionNotFound { path, version_id } => Self::VersionNotFound {
                path: format!("{path} ({context})"),
                version_id,
            },
            Self::DeleteMarker { path, version_id } => Self::DeleteMarker {
                path: format!("{path} ({context})"),
                version_id,
            },
            Self::GovernanceDenied { path, version_id } => Self::GovernanceDenied {
                path: format!("{path} ({context})"),
                version_id,
            },
            Self::Conflict(message) => Self::Conflict(format!("{message}; {context}")),
            Self::UnsupportedFeature(message) => {
                Self::UnsupportedFeature(format!("{message}; {context}"))
            }
            Self::InvalidPath(message) => Self::InvalidPath(format!("{message}; {context}")),
            Self::Config(message) => Self::Config(format!("{message}; {context}")),
            Self::AliasNotFound(message) => Self::AliasNotFound(format!("{message}; {context}")),
            Self::AliasExists(message) => Self::AliasExists(format!("{message}; {context}")),
            Self::RequestRejected(message) => {
                Self::RequestRejected(format!("{message}; {context}"))
            }
            Self::Interrupted(message) => Self::Interrupted(format!("{message}; {context}")),
            source => Self::General(format!("{source}; {context}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_exit_codes() {
        assert_eq!(Error::InvalidPath("test".into()).exit_code(), 2);
        assert_eq!(Error::Config("test".into()).exit_code(), 2);
        assert_eq!(Error::Network("test".into()).exit_code(), 3);
        assert_eq!(Error::Auth("test".into()).exit_code(), 4);
        assert_eq!(Error::NotFound("test".into()).exit_code(), 5);
        assert_eq!(Error::AliasNotFound("test".into()).exit_code(), 5);
        assert_eq!(Error::Conflict("test".into()).exit_code(), 6);
        assert_eq!(Error::AliasExists("test".into()).exit_code(), 6);
        assert_eq!(Error::UnsupportedFeature("test".into()).exit_code(), 7);
        assert_eq!(Error::RequestRejected("test".into()).exit_code(), 1);
        assert_eq!(Error::General("test".into()).exit_code(), 1);
    }

    #[test]
    fn test_error_display() {
        let err = Error::AliasNotFound("myalias".into());
        assert_eq!(err.to_string(), "Alias not found: myalias");

        let err = Error::InvalidPath("/bad/path".into());
        assert_eq!(err.to_string(), "Invalid path: /bad/path");
    }

    #[test]
    fn versioning_errors_are_distinct_and_stable() {
        let missing = Error::VersionNotFound {
            path: "local/bucket/object.txt".to_string(),
            version_id: "v1".to_string(),
        };
        let marker = Error::DeleteMarker {
            path: "local/bucket/object.txt".to_string(),
            version_id: "v2".to_string(),
        };
        let governance = Error::GovernanceDenied {
            path: "local/bucket/object.txt".to_string(),
            version_id: Some("v3".to_string()),
        };

        assert_eq!(missing.exit_code(), 5);
        assert!(missing.to_string().contains("version 'v1'"));
        assert_eq!(marker.exit_code(), 5);
        assert!(marker.to_string().contains("delete marker"));
        assert_eq!(governance.exit_code(), 6);
        assert!(governance.to_string().contains("Governance retention"));
    }

    #[test]
    fn multipart_copy_failure_preserves_primary_classification() {
        let error = Error::Auth("access denied".to_string())
            .with_multipart_copy_context("upload-123", MultipartAbortStatus::Succeeded);

        assert_eq!(error.exit_code(), 4);
        assert!(error.to_string().contains("upload-123"));
        assert!(error.to_string().contains("abort: succeeded"));

        let interrupted = Error::Interrupted("cancelled".to_string())
            .with_multipart_copy_context("upload-456", MultipartAbortStatus::Succeeded);
        assert_eq!(interrupted.exit_code(), 130);
        assert!(interrupted.to_string().contains("upload-456"));
    }
}
