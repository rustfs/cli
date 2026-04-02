//! bucket command group - Noun-first bucket workflows.
//!
//! Provides an agent-friendly bucket-oriented entrypoint while delegating to the
//! existing command implementations.

use clap::{Args, Subcommand};

use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

use super::{anonymous, event, ilm, ls, mb, quota, rb, replicate, version};

const BUCKET_AFTER_HELP: &str = "\
Examples:
  rc bucket list local/
  rc bucket create local/my-bucket
  rc bucket remove local/my-bucket
  rc bucket event list local/my-bucket
  rc bucket replication status local/my-bucket";

/// Manage bucket-oriented workflows
#[derive(Args, Debug)]
#[command(after_help = BUCKET_AFTER_HELP)]
pub struct BucketArgs {
    #[command(subcommand)]
    pub command: BucketCommands,
}

#[derive(Subcommand, Debug)]
pub enum BucketCommands {
    /// List buckets for an alias, or objects within a bucket path
    #[command(alias = "ls")]
    List(ls::LsArgs),

    /// Create a bucket
    Create(mb::MbArgs),

    /// Remove a bucket
    Remove(rb::RbArgs),

    /// Manage bucket notification rules
    #[command(subcommand)]
    Event(event::EventCommands),

    /// Manage bucket versioning
    #[command(subcommand)]
    Version(version::VersionCommands),

    /// Manage bucket quota
    #[command(subcommand)]
    Quota(quota::QuotaCommands),

    /// Manage anonymous bucket access
    #[command(subcommand)]
    Anonymous(anonymous::AnonymousCommands),

    /// Manage bucket lifecycle rules, tiers, and restores
    #[command(name = "lifecycle")]
    Lifecycle(ilm::IlmArgs),

    /// Manage bucket replication rules and status
    #[command(name = "replication")]
    Replication(replicate::ReplicateArgs),
}

/// Execute the bucket command group
pub async fn execute(args: BucketArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        BucketCommands::List(args) => ls::execute(args, output_config).await,
        BucketCommands::Create(args) => mb::execute(args, output_config).await,
        BucketCommands::Remove(args) => rb::execute(args, output_config).await,
        BucketCommands::Event(cmd) => {
            event::execute(event::EventArgs { command: cmd }, output_config).await
        }
        BucketCommands::Version(cmd) => {
            version::execute(version::VersionArgs { command: cmd }, output_config).await
        }
        BucketCommands::Quota(cmd) => {
            quota::execute(quota::QuotaArgs { command: cmd }, output_config).await
        }
        BucketCommands::Anonymous(cmd) => {
            anonymous::execute(anonymous::AnonymousArgs { command: cmd }, output_config).await
        }
        BucketCommands::Lifecycle(args) => ilm::execute(args, output_config).await,
        BucketCommands::Replication(args) => replicate::execute(args, output_config).await,
    }
}
