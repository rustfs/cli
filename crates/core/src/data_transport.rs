//! Selection and failure classification for an optional object data transport.

use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

/// Object payload transport selected independently of S3 management operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum DataTransportMode {
    /// Use the existing S3 HTTP body implementation.
    #[default]
    Http,
    /// Try an installed external transport, with safe HTTP fallback.
    Auto,
    /// Require an installed external transport for object payloads.
    Rdma,
}

impl FromStr for DataTransportMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "http" => Ok(Self::Http),
            "auto" => Ok(Self::Auto),
            "rdma" => Ok(Self::Rdma),
            _ => Err("data transport must be http, auto, or rdma".to_string()),
        }
    }
}

/// Business outcome classification. Transport timeouts never imply rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataTransferState {
    /// The provider proves that the operation was not accepted and IO was untouched.
    NotAccepted,
    /// The operation failed with an authoritative business outcome.
    ConfirmedFailure,
    /// Acceptance or the final business outcome could not be established.
    OutcomeUnknown,
}

impl std::fmt::Display for DataTransferState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotAccepted => "not accepted",
            Self::ConfirmedFailure => "confirmed failure",
            Self::OutcomeUnknown => "outcome unknown",
        })
    }
}

static GLOBAL_MODE: AtomicU8 = AtomicU8::new(DataTransportMode::Http as u8);

/// Set process-wide CLI policy before constructing any clients.
pub fn set_global_data_transport_mode(mode: DataTransportMode) {
    GLOBAL_MODE.store(mode as u8, Ordering::Relaxed);
}

/// Snapshot process-wide policy when constructing a client.
pub fn global_data_transport_mode() -> DataTransportMode {
    match GLOBAL_MODE.load(Ordering::Relaxed) {
        1 => DataTransportMode::Auto,
        2 => DataTransportMode::Rdma,
        _ => DataTransportMode::Http,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_explicit_and_defaults_to_http() {
        assert_eq!(DataTransportMode::default(), DataTransportMode::Http);
        assert_eq!("auto".parse(), Ok(DataTransportMode::Auto));
        assert_eq!("rdma".parse(), Ok(DataTransportMode::Rdma));
        assert!("udp".parse::<DataTransportMode>().is_err());
    }

    #[test]
    fn external_failures_never_enter_string_based_retry() {
        for state in [
            DataTransferState::NotAccepted,
            DataTransferState::ConfirmedFailure,
            DataTransferState::OutcomeUnknown,
        ] {
            let error = crate::Error::DataTransfer {
                state,
                message: "temporary timeout".to_string(),
                source: None,
            };
            assert!(!crate::is_retryable_error(&error));
        }
    }

    #[test]
    fn authoritative_error_retains_exit_code_and_cleanup_without_retry() {
        let error = crate::Error::DataTransfer {
            state: DataTransferState::ConfirmedFailure,
            message: "S3 request failed".to_string(),
            source: Some(Box::new(crate::Error::Auth("access denied".to_string()))),
        }
        .with_multipart_copy_context("upload-id", crate::MultipartAbortStatus::Succeeded);
        assert_eq!(error.exit_code(), 4);
        assert!(!crate::is_retryable_error(&error));
        assert!(std::error::Error::source(&error).is_some());
        let error = crate::Error::DataTransfer {
            state: DataTransferState::OutcomeUnknown,
            message: "temporary timeout".to_string(),
            source: Some(Box::new(crate::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "temporary timeout",
            )))),
        };
        assert!(!crate::is_retryable_error(&error));
    }
}
