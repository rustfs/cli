//! CLI command definitions and execution
//!
//! This module contains all CLI commands and their implementations.
//! Commands are organized by functionality and follow the pattern established
//! in the command implementation template.

use clap::{Parser, Subcommand};

use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

mod alias;
mod cat;
pub mod cp;
mod head;
mod ls;
mod mb;
mod mv;
mod pipe;
mod rb;
mod rm;
mod stat;

/// rc - Rust S3 CLI Client
///
/// A command-line interface for S3-compatible object storage services.
/// Supports RustFS, AWS S3, and other S3-compatible backends.
#[derive(Parser, Debug)]
#[command(name = "rc")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    /// Output format: human-readable or JSON
    #[arg(long, global = true, default_value = "false")]
    pub json: bool,

    /// Disable colored output
    #[arg(long, global = true, default_value = "false")]
    pub no_color: bool,

    /// Disable progress bar
    #[arg(long, global = true, default_value = "false")]
    pub no_progress: bool,

    /// Suppress non-error output
    #[arg(short, long, global = true, default_value = "false")]
    pub quiet: bool,

    /// Enable debug logging
    #[arg(long, global = true, default_value = "false")]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage storage service aliases
    #[command(subcommand)]
    Alias(alias::AliasCommands),

    // Phase 2: Basic commands
    /// List buckets and objects
    Ls(ls::LsArgs),

    /// Create a bucket
    Mb(mb::MbArgs),

    /// Remove a bucket
    Rb(rb::RbArgs),

    /// Display object contents
    Cat(cat::CatArgs),

    /// Display first N lines of an object
    Head(head::HeadArgs),

    /// Show object metadata
    Stat(stat::StatArgs),

    // Phase 3: Transfer commands
    /// Copy objects (local<->S3, S3<->S3)
    Cp(cp::CpArgs),

    /// Move objects (copy + delete source)
    Mv(mv::MvArgs),

    /// Remove objects
    Rm(rm::RmArgs),

    /// Stream stdin to an object
    Pipe(pipe::PipeArgs),
    // Phase 4: Advanced commands
    // /// Find objects matching criteria
    // Find(find::FindArgs),
    // /// Show differences between locations
    // Diff(diff::DiffArgs),
    // /// Mirror objects between locations
    // Mirror(mirror::MirrorArgs),
    // /// Display objects in tree format
    // Tree(tree::TreeArgs),
    // /// Generate presigned URLs
    // Share(share::ShareArgs),

    // Phase 5: Optional commands (capability-dependent)
    // /// Manage bucket versioning
    // Version(version::VersionArgs),
    // /// Manage object retention
    // Retention(retention::RetentionArgs),
    // /// Manage object tags
    // Tag(tag::TagArgs),
    // /// Watch for object events
    // Watch(watch::WatchArgs),
    // /// Run S3 Select queries
    // Sql(sql::SqlArgs),
}

/// Execute the CLI command and return an exit code
pub async fn execute(cli: Cli) -> ExitCode {
    let output_config = OutputConfig {
        json: cli.json,
        no_color: cli.no_color,
        no_progress: cli.no_progress,
        quiet: cli.quiet,
    };

    match cli.command {
        Commands::Alias(cmd) => alias::execute(cmd, cli.json).await,
        Commands::Ls(args) => ls::execute(args, output_config).await,
        Commands::Mb(args) => mb::execute(args, output_config).await,
        Commands::Rb(args) => rb::execute(args, output_config).await,
        Commands::Cat(args) => cat::execute(args, output_config).await,
        Commands::Head(args) => head::execute(args, output_config).await,
        Commands::Stat(args) => stat::execute(args, output_config).await,
        Commands::Cp(args) => cp::execute(args, output_config).await,
        Commands::Mv(args) => mv::execute(args, output_config).await,
        Commands::Rm(args) => rm::execute(args, output_config).await,
        Commands::Pipe(args) => pipe::execute(args, output_config).await,
    }
}
