//! Error types for rc-core
//!
//! Provides a unified error type that can be converted to appropriate exit codes.

use thiserror::Error;

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
}

impl Error {
    /// Get the appropriate exit code for this error
    pub const fn exit_code(&self) -> i32 {
        match self {
            Error::InvalidPath(_) => 2,                        // UsageError
            Error::Config(_) => 2,                             // UsageError
            Error::Network(_) => 3,                            // NetworkError
            Error::Auth(_) => 4,                               // AuthError
            Error::NotFound(_) | Error::AliasNotFound(_) => 5, // NotFound
            Error::Conflict(_) | Error::AliasExists(_) => 6,   // Conflict
            Error::UnsupportedFeature(_) => 7,                 // UnsupportedFeature
            _ => 1,                                            // GeneralError
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
        assert_eq!(Error::General("test".into()).exit_code(), 1);
    }

    #[test]
    fn test_error_display() {
        let err = Error::AliasNotFound("minio".into());
        assert_eq!(err.to_string(), "Alias not found: minio");

        let err = Error::InvalidPath("/bad/path".into());
        assert_eq!(err.to_string(), "Invalid path: /bad/path");
    }
}
