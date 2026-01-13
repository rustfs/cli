//! Output formatting utilities
//!
//! This module provides formatters for CLI output in both human-readable
//! and JSON formats. It also handles progress bars and colored output.

mod formatter;
mod progress;

// These exports will be used in Phase 2+ when commands are implemented
#[allow(unused_imports)]
pub use formatter::Formatter;
#[allow(unused_imports)]
pub use progress::ProgressBar;

/// Output configuration derived from CLI flags
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct OutputConfig {
    /// Use JSON output format
    pub json: bool,
    /// Disable colored output
    pub no_color: bool,
    /// Disable progress bar
    pub no_progress: bool,
    /// Suppress non-error output
    pub quiet: bool,
}
