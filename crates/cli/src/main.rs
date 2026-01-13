//! rc - Rust S3 CLI Client
//!
//! A command-line interface for S3-compatible object storage services.
//! Designed for RustFS and other S3-compatible backends.

use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod commands;
mod exit_code;
mod output;

use commands::Cli;

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber for logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let exit_code = commands::execute(cli).await;

    std::process::exit(exit_code.as_i32());
}
