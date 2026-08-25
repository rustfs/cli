//! Output formatting utilities
//!
//! This module provides formatters for CLI output in both human-readable
//! and JSON formats. It also handles progress bars and colored output.

mod formatter;
mod progress;
pub mod qr;
mod v3;

// These exports will be used in Phase 2+ when commands are implemented
#[allow(unused_imports)]
pub use formatter::Formatter;
#[allow(unused_imports)]
pub use formatter::Theme;
#[allow(unused_imports)]
pub use progress::ProgressBar;
pub use v3::{V3ErrorEnvelope, V3PartialErrorEnvelope, V3SuccessEnvelope};

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
