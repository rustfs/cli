//! Output formatter for human-readable and JSON output
//!
//! Ensures consistent output formatting across all commands.
//! JSON output follows the schema defined in schemas/output_v1.json.

use serde::Serialize;

use super::OutputConfig;

/// Formatter for CLI output
///
/// Handles both human-readable and JSON output formats based on configuration.
/// When JSON mode is enabled, all output is strict JSON without colors or progress.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Formatter {
    config: OutputConfig,
}

#[allow(dead_code)]
impl Formatter {
    /// Create a new formatter with the given configuration
    pub fn new(config: OutputConfig) -> Self {
        Self { config }
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

        if self.colors_enabled() {
            println!("\x1b[32m✓\x1b[0m {message}");
        } else {
            println!("✓ {message}");
        }
    }

    /// Output an error message
    ///
    /// Errors are always printed, even in quiet mode.
    pub fn error(&self, message: &str) {
        if self.config.json {
            let error = serde_json::json!({
                "error": message
            });
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error).unwrap_or_else(|_| message.to_string())
            );
        } else if self.colors_enabled() {
            eprintln!("\x1b[31m✗\x1b[0m {message}");
        } else {
            eprintln!("✗ {message}");
        }
    }

    /// Output a warning message
    pub fn warning(&self, message: &str) {
        if self.config.quiet || self.config.json {
            return;
        }

        if self.colors_enabled() {
            eprintln!("\x1b[33m⚠\x1b[0m {message}");
        } else {
            eprintln!("⚠ {message}");
        }
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
}
