//! Output formatter for human-readable and JSON output
//!
//! Ensures consistent output formatting across all commands.
//! JSON output follows the schema defined in schemas/output_v1.json.

use console::Style;
use serde::Serialize;

use super::OutputConfig;
use crate::exit_code::ExitCode;

const USAGE_SUGGESTION: &str =
    "Run the command with --help to review the expected arguments and flags.";
const NETWORK_SUGGESTION: &str =
    "Retry the command. If the problem persists, verify the endpoint and network connectivity.";
const AUTH_SUGGESTION: &str =
    "Verify the alias credentials and permissions, then retry the command.";
const NOT_FOUND_SUGGESTION: &str = "Check the alias, bucket, or object path and retry the command.";
const CONFLICT_SUGGESTION: &str =
    "Review the target resource state and retry with the appropriate overwrite or ignore flag.";
const UNSUPPORTED_SUGGESTION: &str =
    "Retry with --force only if you want to bypass capability detection.";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct JsonErrorOutput {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<JsonErrorDetails>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct JsonErrorDetails {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
    retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ErrorDescriptor {
    code: Option<ExitCode>,
    error_type: &'static str,
    message: String,
    suggestion: Option<String>,
    retryable: bool,
}

impl ErrorDescriptor {
    fn from_message(message: &str) -> Self {
        let message = message.to_string();
        let (error_type, retryable, suggestion) = infer_error_metadata(&message);

        Self {
            code: None,
            error_type,
            message,
            suggestion,
            retryable,
        }
    }

    fn from_code(code: ExitCode, message: &str) -> Self {
        let (error_type, retryable, suggestion) = defaults_for_exit_code(code);

        Self {
            code: Some(code),
            error_type,
            message: message.to_string(),
            suggestion: suggestion.map(str::to_string),
            retryable,
        }
    }

    fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    fn to_json_output(&self) -> JsonErrorOutput {
        JsonErrorOutput {
            error: self.message.clone(),
            code: self.code.map(ExitCode::as_i32),
            details: Some(JsonErrorDetails {
                error_type: self.error_type,
                message: self.message.clone(),
                suggestion: self.suggestion.clone(),
                retryable: self.retryable,
            }),
        }
    }
}

fn defaults_for_exit_code(code: ExitCode) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::Success => ("success", false, None),
        ExitCode::GeneralError => ("general_error", false, None),
        ExitCode::UsageError => ("usage_error", false, Some(USAGE_SUGGESTION)),
        ExitCode::NetworkError => ("network_error", true, Some(NETWORK_SUGGESTION)),
        ExitCode::AuthError => ("auth_error", false, Some(AUTH_SUGGESTION)),
        ExitCode::NotFound => ("not_found", false, Some(NOT_FOUND_SUGGESTION)),
        ExitCode::Conflict => ("conflict", false, Some(CONFLICT_SUGGESTION)),
        ExitCode::UnsupportedFeature => {
            ("unsupported_feature", false, Some(UNSUPPORTED_SUGGESTION))
        }
        ExitCode::Interrupted => (
            "interrupted",
            true,
            Some("Retry the command if you still need the operation to complete."),
        ),
    }
}

fn infer_error_metadata(message: &str) -> (&'static str, bool, Option<String>) {
    let normalized = message.to_ascii_lowercase();

    if normalized.contains("not found") || normalized.contains("does not exist") {
        return ("not_found", false, Some(NOT_FOUND_SUGGESTION.to_string()));
    }

    if normalized.contains("access denied")
        || normalized.contains("unauthorized")
        || normalized.contains("forbidden")
        || normalized.contains("authentication")
        || normalized.contains("credentials")
    {
        return ("auth_error", false, Some(AUTH_SUGGESTION.to_string()));
    }

    if normalized.contains("invalid")
        || normalized.contains("cannot be empty")
        || normalized.contains("must be")
        || normalized.contains("must specify")
        || normalized.contains("expected:")
        || normalized.contains("use -r/--recursive")
    {
        return ("usage_error", false, Some(USAGE_SUGGESTION.to_string()));
    }

    if normalized.contains("already exists")
        || normalized.contains("conflict")
        || normalized.contains("precondition")
        || normalized.contains("destination exists")
    {
        return ("conflict", false, Some(CONFLICT_SUGGESTION.to_string()));
    }

    if normalized.contains("does not support")
        || normalized.contains("unsupported")
        || normalized.contains("not yet supported")
    {
        return (
            "unsupported_feature",
            false,
            Some(UNSUPPORTED_SUGGESTION.to_string()),
        );
    }

    if normalized.contains("timeout")
        || normalized.contains("network")
        || normalized.contains("connection")
        || normalized.contains("temporarily unavailable")
        || normalized.contains("failed to create s3 client")
    {
        return ("network_error", true, Some(NETWORK_SUGGESTION.to_string()));
    }

    ("general_error", false, None)
}

/// Color theme for styled output (exa/eza inspired)
#[derive(Debug, Clone)]
pub struct Theme {
    /// Directory names - blue + bold
    pub dir: Style,
    /// File names - default
    pub file: Style,
    /// File sizes - green
    pub size: Style,
    /// Timestamps - dim/dark gray
    pub date: Style,
    /// Property keys (stat output) - cyan
    pub key: Style,
    /// URLs/endpoints - cyan + underline
    pub url: Style,
    /// Alias/bucket names - bold
    pub name: Style,
    /// Success messages - green
    pub success: Style,
    /// Error messages - red
    pub error: Style,
    /// Warning messages - yellow
    pub warning: Style,
    /// Tree branch characters - dim
    pub tree_branch: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            dir: Style::new().blue().bold(),
            file: Style::new(),
            size: Style::new().green(),
            date: Style::new().dim(),
            key: Style::new().cyan(),
            url: Style::new().cyan().underlined(),
            name: Style::new().bold(),
            success: Style::new().green(),
            error: Style::new().red(),
            warning: Style::new().yellow(),
            tree_branch: Style::new().dim(),
        }
    }
}

impl Theme {
    /// Returns a theme with no styling (for no-color mode)
    pub fn plain() -> Self {
        Self {
            dir: Style::new(),
            file: Style::new(),
            size: Style::new(),
            date: Style::new(),
            key: Style::new(),
            url: Style::new(),
            name: Style::new(),
            success: Style::new(),
            error: Style::new(),
            warning: Style::new(),
            tree_branch: Style::new(),
        }
    }
}

/// Formatter for CLI output
///
/// Handles both human-readable and JSON output formats based on configuration.
/// When JSON mode is enabled, all output is strict JSON without colors or progress.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Formatter {
    config: OutputConfig,
    theme: Theme,
}

#[allow(dead_code)]
impl Formatter {
    /// Create a new formatter with the given configuration
    pub fn new(config: OutputConfig) -> Self {
        let theme = if config.no_color || config.json {
            Theme::plain()
        } else {
            Theme::default()
        };
        Self { config, theme }
    }

    /// Check if JSON output mode is enabled
    pub fn is_json(&self) -> bool {
        self.config.json
    }

    /// Check if quiet mode is enabled
    pub fn is_quiet(&self) -> bool {
        self.config.quiet
    }

    /// Check if colors are enabled
    pub fn colors_enabled(&self) -> bool {
        !self.config.no_color && !self.config.json
    }

    /// Get the current theme
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Get a clone of the output configuration
    pub fn output_config(&self) -> OutputConfig {
        self.config.clone()
    }

    // ========== Style helper methods ==========

    /// Style a directory name (blue + bold)
    pub fn style_dir(&self, text: &str) -> String {
        self.theme.dir.apply_to(text).to_string()
    }

    /// Style a file name (default)
    pub fn style_file(&self, text: &str) -> String {
        self.theme.file.apply_to(text).to_string()
    }

    /// Style a file size (green)
    pub fn style_size(&self, text: &str) -> String {
        self.theme.size.apply_to(text).to_string()
    }

    /// Style a timestamp/date (dim)
    pub fn style_date(&self, text: &str) -> String {
        self.theme.date.apply_to(text).to_string()
    }

    /// Style a property key (cyan)
    pub fn style_key(&self, text: &str) -> String {
        self.theme.key.apply_to(text).to_string()
    }

    /// Style a URL/endpoint (cyan + underline)
    pub fn style_url(&self, text: &str) -> String {
        self.theme.url.apply_to(text).to_string()
    }

    /// Style an alias/bucket name (bold)
    pub fn style_name(&self, text: &str) -> String {
        self.theme.name.apply_to(text).to_string()
    }

    /// Style tree branch characters (dim)
    pub fn style_tree_branch(&self, text: &str) -> String {
        self.theme.tree_branch.apply_to(text).to_string()
    }

    // ========== Output methods ==========

    /// Output a value
    ///
    /// In JSON mode, serializes the value to JSON.
    /// In human mode, uses the Display implementation.
    pub fn output<T: Serialize + std::fmt::Display>(&self, value: &T) {
        if self.config.quiet {
            return;
        }

        if self.config.json {
            // JSON output: strict, no colors, no extra formatting
            match serde_json::to_string_pretty(value) {
                Ok(json) => println!("{json}"),
                Err(e) => eprintln!("Error serializing output: {e}"),
            }
        } else {
            println!("{value}");
        }
    }

    /// Output a success message
    pub fn success(&self, message: &str) {
        if self.config.quiet {
            return;
        }

        if self.config.json {
            // In JSON mode, success is indicated by exit code, not message
            return;
        }

        let checkmark = self.theme.success.apply_to("✓");
        println!("{checkmark} {message}");
    }

    /// Output an error message
    ///
    /// Errors are always printed, even in quiet mode.
    pub fn error(&self, message: &str) {
        self.emit_error(ErrorDescriptor::from_message(message));
    }

    /// Output an error message with an explicit exit code.
    pub fn error_with_code(&self, code: ExitCode, message: &str) {
        self.emit_error(ErrorDescriptor::from_code(code, message));
    }

    /// Output an error message with an explicit exit code and recovery suggestion.
    pub fn error_with_suggestion(&self, code: ExitCode, message: &str, suggestion: &str) {
        self.emit_error(ErrorDescriptor::from_code(code, message).with_suggestion(suggestion));
    }

    /// Print an error and return the provided exit code.
    pub fn fail(&self, code: ExitCode, message: &str) -> ExitCode {
        self.error_with_code(code, message);
        code
    }

    /// Print an error with a recovery hint and return the provided exit code.
    pub fn fail_with_suggestion(
        &self,
        code: ExitCode,
        message: &str,
        suggestion: &str,
    ) -> ExitCode {
        self.error_with_suggestion(code, message, suggestion);
        code
    }

    fn emit_error(&self, descriptor: ErrorDescriptor) {
        if self.config.json {
            let error = descriptor.to_json_output();
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error).unwrap_or_else(|_| descriptor.message.clone())
            );
        } else {
            let cross = self.theme.error.apply_to("✗");
            eprintln!("{cross} {}", descriptor.message);
        }
    }

    /// Output a warning message
    pub fn warning(&self, message: &str) {
        if self.config.quiet || self.config.json {
            return;
        }

        let warn_icon = self.theme.warning.apply_to("⚠");
        eprintln!("{warn_icon} {message}");
    }

    /// Output JSON directly
    ///
    /// Used when you want to output a pre-built JSON structure.
    pub fn json<T: Serialize>(&self, value: &T) {
        match serde_json::to_string_pretty(value) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("Error serializing output: {e}"),
        }
    }

    /// Print a line of text (respects quiet mode)
    pub fn println(&self, message: &str) {
        if self.config.quiet {
            return;
        }
        println!("{message}");
    }
}

impl Default for Formatter {
    fn default() -> Self {
        Self::new(OutputConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatter_default() {
        let formatter = Formatter::default();
        assert!(!formatter.is_json());
        assert!(!formatter.is_quiet());
        assert!(formatter.colors_enabled());
    }

    #[test]
    fn test_formatter_json_mode() {
        let config = OutputConfig {
            json: true,
            ..Default::default()
        };
        let formatter = Formatter::new(config);
        assert!(formatter.is_json());
        assert!(!formatter.colors_enabled()); // Colors disabled in JSON mode
    }

    #[test]
    fn test_formatter_no_color() {
        let config = OutputConfig {
            no_color: true,
            ..Default::default()
        };
        let formatter = Formatter::new(config);
        assert!(!formatter.colors_enabled());
    }

    #[test]
    fn test_error_descriptor_from_code_sets_defaults() {
        let descriptor =
            ErrorDescriptor::from_code(ExitCode::NetworkError, "Failed to create S3 client");

        assert_eq!(descriptor.code, Some(ExitCode::NetworkError));
        assert_eq!(descriptor.error_type, "network_error");
        assert!(descriptor.retryable);
        assert_eq!(descriptor.suggestion.as_deref(), Some(NETWORK_SUGGESTION));
    }

    #[test]
    fn test_error_descriptor_from_message_infers_not_found() {
        let descriptor = ErrorDescriptor::from_message("Alias 'local' not found");
        let json = descriptor.to_json_output();

        assert_eq!(json.error, "Alias 'local' not found");
        assert_eq!(json.code, None);
        let details = json.details.expect("details should be present");
        assert_eq!(details.error_type, "not_found");
        assert!(!details.retryable);
        assert_eq!(details.suggestion.as_deref(), Some(NOT_FOUND_SUGGESTION));
    }

    #[test]
    fn test_error_descriptor_from_message_prefers_usage_for_invalid_permission() {
        let descriptor = ErrorDescriptor::from_message("Invalid permission 'download'");
        let json = descriptor.to_json_output();

        let details = json.details.expect("details should be present");
        assert_eq!(details.error_type, "usage_error");
        assert!(!details.retryable);
        assert_eq!(details.suggestion.as_deref(), Some(USAGE_SUGGESTION));
    }

    #[test]
    fn test_error_descriptor_from_message_prefers_auth_for_invalid_credentials() {
        let descriptor = ErrorDescriptor::from_message("Invalid credentials for alias 'local'");
        let json = descriptor.to_json_output();

        let details = json.details.expect("details should be present");
        assert_eq!(details.error_type, "auth_error");
        assert!(!details.retryable);
        assert_eq!(details.suggestion.as_deref(), Some(AUTH_SUGGESTION));
    }

    #[test]
    fn test_error_descriptor_from_message_classifies_other_auth_failures() {
        for message in [
            "Unauthorized request for alias 'local'",
            "Forbidden: bucket access denied",
            "Authentication failed for alias 'local'",
        ] {
            let descriptor = ErrorDescriptor::from_message(message);
            let json = descriptor.to_json_output();
            let details = json.details.expect("details should be present");

            assert_eq!(details.error_type, "auth_error", "message: {message}");
            assert!(!details.retryable, "message: {message}");
            assert_eq!(
                details.suggestion.as_deref(),
                Some(AUTH_SUGGESTION),
                "message: {message}"
            );
        }
    }

    #[test]
    fn test_error_descriptor_from_message_infers_conflict() {
        let descriptor = ErrorDescriptor::from_message("Destination exists: report.json");
        let json = descriptor.to_json_output();

        let details = json.details.expect("details should be present");
        assert_eq!(details.error_type, "conflict");
        assert!(!details.retryable);
        assert_eq!(details.suggestion.as_deref(), Some(CONFLICT_SUGGESTION));
    }

    #[test]
    fn test_error_descriptor_from_message_infers_unsupported_feature() {
        let descriptor = ErrorDescriptor::from_message(
            "Cross-alias S3-to-S3 copy not yet supported. Use download + upload.",
        );
        let json = descriptor.to_json_output();

        let details = json.details.expect("details should be present");
        assert_eq!(details.error_type, "unsupported_feature");
        assert!(!details.retryable);
        assert_eq!(details.suggestion.as_deref(), Some(UNSUPPORTED_SUGGESTION));
    }

    #[test]
    fn test_error_descriptor_from_message_infers_retryable_network_error() {
        let descriptor =
            ErrorDescriptor::from_message("Service temporarily unavailable while connecting");
        let json = descriptor.to_json_output();

        let details = json.details.expect("details should be present");
        assert_eq!(details.error_type, "network_error");
        assert!(details.retryable);
        assert_eq!(details.suggestion.as_deref(), Some(NETWORK_SUGGESTION));
    }

    #[test]
    fn test_error_with_suggestion_overrides_default_hint() {
        let descriptor = ErrorDescriptor::from_code(
            ExitCode::UnsupportedFeature,
            "Backend does not support notifications.",
        )
        .with_suggestion("Retry with --force to bypass capability detection.");

        let json = descriptor.to_json_output();
        let details = json.details.expect("details should be present");

        assert_eq!(details.error_type, "unsupported_feature");
        assert_eq!(
            details.suggestion.as_deref(),
            Some("Retry with --force to bypass capability detection.")
        );
    }
}
