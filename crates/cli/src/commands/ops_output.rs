//! Shared schema-v3 error output for operational commands.

use rc_core::Error;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ErrorEnvelope {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: String,
    status: &'static str,
    error: ErrorBody,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum ErrorBody {
    Standard(StandardError),
    Unsupported(UnsupportedError),
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct StandardError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UnsupportedError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: String,
    server: Option<String>,
    suggestion: Option<String>,
}

pub(super) fn emit_error(
    formatter: &Formatter,
    family: &str,
    context: &str,
    error: &Error,
    capability: Option<&str>,
    server: Option<&str>,
    suggestion: Option<&str>,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("{context}: {error}");
    if formatter.is_json() {
        formatter.json_error(&error_envelope(
            family, error, message, capability, server, suggestion,
        ));
    } else if let Some(suggestion) = suggestion {
        formatter.error_with_suggestion(code, &message, suggestion);
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

pub(super) fn emit_message(
    formatter: &Formatter,
    family: &str,
    code: ExitCode,
    message: impl Into<String>,
    suggestion: Option<&str>,
) -> ExitCode {
    let message = message.into();
    let error = error_for_exit(code, message.clone());
    if formatter.is_json() {
        formatter.json_error(&error_envelope(
            family, &error, message, None, None, suggestion,
        ));
    } else if let Some(suggestion) = suggestion {
        formatter.error_with_suggestion(code, &message, suggestion);
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn error_envelope(
    family: &str,
    error: &Error,
    message: String,
    capability: Option<&str>,
    server: Option<&str>,
    suggestion: Option<&str>,
) -> ErrorEnvelope {
    let error = if matches!(error, Error::UnsupportedFeature(_)) {
        ErrorBody::Unsupported(UnsupportedError {
            error_type: "unsupported_feature",
            message,
            retryable: false,
            capability: capability.unwrap_or("unknown").to_string(),
            server: server.map(str::to_string),
            suggestion: suggestion.map(str::to_string),
        })
    } else {
        let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
        ErrorBody::Standard(StandardError {
            error_type: error_type(code),
            message,
            retryable: code == ExitCode::NetworkError || code == ExitCode::Interrupted,
            suggestion: suggestion.map(str::to_string),
        })
    };

    ErrorEnvelope {
        schema_version: 3,
        output_type: family.to_string(),
        status: "error",
        error,
    }
}

fn error_type(code: ExitCode) -> &'static str {
    match code {
        ExitCode::Success | ExitCode::GeneralError => "general_error",
        ExitCode::UsageError => "usage_error",
        ExitCode::NetworkError => "network_error",
        ExitCode::AuthError => "auth_error",
        ExitCode::NotFound => "not_found",
        ExitCode::Conflict => "conflict",
        ExitCode::UnsupportedFeature => "general_error",
        ExitCode::Interrupted => "interrupted",
    }
}

fn error_for_exit(code: ExitCode, message: String) -> Error {
    match code {
        ExitCode::UsageError => Error::InvalidPath(message),
        ExitCode::NetworkError => Error::Network(message),
        ExitCode::AuthError => Error::Auth(message),
        ExitCode::NotFound => Error::NotFound(message),
        ExitCode::Conflict => Error::Conflict(message),
        ExitCode::UnsupportedFeature => Error::UnsupportedFeature(message),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::Interrupted => {
            Error::General(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_error_uses_specialized_v3_shape() {
        let output = error_envelope(
            "usage",
            &Error::UnsupportedFeature("missing route".to_string()),
            "Fast path unavailable".to_string(),
            Some("admin.data-usage"),
            Some("rustfs"),
            Some("Retry with --fallback."),
        );
        let value = serde_json::to_value(output).expect("error output should serialize");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["type"], "usage");
        assert_eq!(value["error"]["type"], "unsupported_feature");
        assert_eq!(value["error"]["capability"], "admin.data-usage");
    }
}
