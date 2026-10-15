//! rc CLI library
//!
//! This module exports the CLI components for use in integration tests.

pub mod commands;
pub(crate) mod confirm;
pub mod exit_code;
mod launcher;
pub use launcher::run;
pub mod output;
pub(crate) mod private_file;
pub(crate) mod secret_input;
