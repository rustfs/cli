//! Exit code definitions for rc CLI
//!
//! This file is protected by CI. Any modifications require the Breaking Change process:
//! 1. Update version number
//! 2. Provide migration plan
//! 3. Update CHANGELOG
//! 4. Mark PR as BREAKING

/// Exit codes for the rc CLI application.
///
/// These codes follow a consistent convention to allow scripts and automation
/// to handle different error scenarios appropriately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// Operation completed successfully
    Success = 0,

    /// General/unspecified error
    GeneralError = 1,

    /// User input error: invalid arguments, malformed path, etc.
    UsageError = 2,

    /// Retryable network error: timeout, connection reset, 503, etc.
    NetworkError = 3,

    /// Authentication or permission failure
    AuthError = 4,

    /// Resource not found: bucket or object does not exist
    NotFound = 5,

    /// Conflict or precondition failure: version conflict, if-match failed, etc.
    Conflict = 6,

    /// Backend does not support this feature
    UnsupportedFeature = 7,

    /// Operation was interrupted (e.g., Ctrl+C)
    Interrupted = 130,
}

impl ExitCode {
    /// Convert exit code to i32 for use with std::process::exit
    #[inline]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Create exit code from i32 value
    ///
    /// Returns None if the value doesn't correspond to a known exit code.
    pub const fn from_i32(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Success),
            1 => Some(Self::GeneralError),
            2 => Some(Self::UsageError),
            3 => Some(Self::NetworkError),
            4 => Some(Self::AuthError),
            5 => Some(Self::NotFound),
            6 => Some(Self::Conflict),
            7 => Some(Self::UnsupportedFeature),
            130 => Some(Self::Interrupted),
            _ => None,
        }
    }

    /// Get a human-readable description of the exit code
    pub const fn description(self) -> &'static str {
        match self {
            Self::Success => "Operation completed successfully",
            Self::GeneralError => "General error",
            Self::UsageError => "Invalid arguments or path format",
            Self::NetworkError => "Network error (retryable)",
            Self::AuthError => "Authentication or permission failure",
            Self::NotFound => "Resource not found",
            Self::Conflict => "Conflict or precondition failure",
            Self::UnsupportedFeature => "Feature not supported by backend",
            Self::Interrupted => "Operation interrupted",
        }
    }
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> Self {
        code.as_i32()
    }
}

impl std::fmt::Display for ExitCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.description(), self.as_i32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_values() {
        assert_eq!(ExitCode::Success.as_i32(), 0);
        assert_eq!(ExitCode::GeneralError.as_i32(), 1);
        assert_eq!(ExitCode::UsageError.as_i32(), 2);
        assert_eq!(ExitCode::NetworkError.as_i32(), 3);
        assert_eq!(ExitCode::AuthError.as_i32(), 4);
        assert_eq!(ExitCode::NotFound.as_i32(), 5);
        assert_eq!(ExitCode::Conflict.as_i32(), 6);
        assert_eq!(ExitCode::UnsupportedFeature.as_i32(), 7);
        assert_eq!(ExitCode::Interrupted.as_i32(), 130);
    }

    #[test]
    fn test_exit_code_from_i32() {
        assert_eq!(ExitCode::from_i32(0), Some(ExitCode::Success));
        assert_eq!(ExitCode::from_i32(1), Some(ExitCode::GeneralError));
        assert_eq!(ExitCode::from_i32(2), Some(ExitCode::UsageError));
        assert_eq!(ExitCode::from_i32(3), Some(ExitCode::NetworkError));
        assert_eq!(ExitCode::from_i32(4), Some(ExitCode::AuthError));
        assert_eq!(ExitCode::from_i32(5), Some(ExitCode::NotFound));
        assert_eq!(ExitCode::from_i32(6), Some(ExitCode::Conflict));
        assert_eq!(ExitCode::from_i32(7), Some(ExitCode::UnsupportedFeature));
        assert_eq!(ExitCode::from_i32(130), Some(ExitCode::Interrupted));
        assert_eq!(ExitCode::from_i32(99), None);
    }

    #[test]
    fn test_exit_code_into_i32() {
        let code: i32 = ExitCode::Success.into();
        assert_eq!(code, 0);

        let code: i32 = ExitCode::NotFound.into();
        assert_eq!(code, 5);
    }

    #[test]
    fn test_exit_code_display() {
        let display = format!("{}", ExitCode::Success);
        assert!(display.contains("0"));
        assert!(display.contains("successfully"));

        let display = format!("{}", ExitCode::NotFound);
        assert!(display.contains("5"));
        assert!(display.contains("not found"));
    }
}
