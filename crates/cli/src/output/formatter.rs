//! Output formatter for human-readable and JSON output
//!
//! Ensures consistent output formatting across all commands.
//! JSON output follows the schema defined in schemas/output_v1.json.

use console::Style;
use serde::Serialize;

use super::OutputConfig;

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
        if self.config.json {
            let error = serde_json::json!({
                "error": message
            });
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error).unwrap_or_else(|_| message.to_string())
            );
        } else {
            let cross = self.theme.error.apply_to("✗");
            eprintln!("{cross} {message}");
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
}
