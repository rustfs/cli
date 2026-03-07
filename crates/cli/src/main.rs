//! rc - Rust S3 CLI Client
//!
//! A command-line interface for S3-compatible object storage services.
//! Designed for RustFS and other S3-compatible backends.

use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod commands;
mod exit_code;
mod output;

use commands::Cli;

fn build_env_filter(debug: bool, env_is_set: bool) -> EnvFilter {
    if debug && !env_is_set {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let env_is_set = std::env::var_os(EnvFilter::DEFAULT_ENV).is_some();
    let env_filter = build_env_filter(cli.debug, env_is_set);

    // Initialize tracing subscriber for logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();

    let exit_code = commands::execute(cli).await;

    std::process::exit(exit_code.as_i32());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_uses_debug_filter_when_env_missing() {
        let filter = build_env_filter(true, false);
        assert_eq!(filter.to_string(), "debug");
    }
}
