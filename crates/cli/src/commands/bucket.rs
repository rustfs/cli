//! bucket command group - Noun-first bucket workflows.
//!
//! Provides an agent-friendly bucket-oriented entrypoint while delegating to the
//! existing command implementations.

use clap::{Args, Subcommand};

use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

use super::{anonymous, cors, encryption, event, ilm, ls, mb, quota, rb, replicate, version};

const BUCKET_AFTER_HELP: &str = "\
Examples:
  rc bucket list local/
  rc bucket create local/my-bucket
  rc bucket remove local/my-bucket
  rc bucket event list local/my-bucket
  rc bucket cors list local/my-bucket
  rc bucket encryption info local/my-bucket
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

    /// Manage bucket CORS rules
    #[command(subcommand)]
    Cors(cors::CorsCommands),

    /// Manage bucket default encryption
    Encryption(encryption::EncryptionArgs),

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
    // Boxing each branch keeps this dispatcher future below Windows' 1 MiB main-thread stack.
    match args.command {
        BucketCommands::List(args) => Box::pin(ls::execute(args, output_config)).await,
        BucketCommands::Create(args) => Box::pin(mb::execute(args, output_config)).await,
        BucketCommands::Remove(args) => Box::pin(rb::execute(args, output_config)).await,
        BucketCommands::Event(cmd) => {
            Box::pin(event::execute(
                event::EventArgs { command: cmd },
                output_config,
            ))
            .await
        }
        BucketCommands::Cors(cmd) => {
            Box::pin(cors::execute(
                cors::CorsArgs { command: cmd },
                output_config,
            ))
            .await
        }
        BucketCommands::Encryption(args) => {
            Box::pin(encryption::execute(args, output_config)).await
        }
        BucketCommands::Version(cmd) => {
            Box::pin(version::execute(
                version::VersionArgs { command: cmd },
                output_config,
            ))
            .await
        }
        BucketCommands::Quota(cmd) => {
            Box::pin(quota::execute(
                quota::QuotaArgs { command: cmd },
                output_config,
            ))
            .await
        }
        BucketCommands::Anonymous(cmd) => {
            Box::pin(anonymous::execute(
                anonymous::AnonymousArgs { command: cmd },
                output_config,
            ))
            .await
        }
        BucketCommands::Lifecycle(args) => Box::pin(ilm::execute(args, output_config)).await,
        BucketCommands::Replication(args) => {
            Box::pin(replicate::execute(args, output_config)).await
        }
    }
}
