//! ilm command - Manage bucket lifecycle (ILM) rules, tiers, and restores
//!
//! Add, edit, list, remove, export, and import lifecycle rules;
//! manage remote storage tiers; restore transitioned objects.

pub mod restore;
pub mod rule;
pub mod tier;

use clap::{Args, Subcommand};

use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

/// Manage bucket lifecycle (ILM) configuration
#[derive(Args, Debug)]
pub struct IlmArgs {
    #[command(subcommand)]
    pub command: IlmCommands,
}

#[derive(Subcommand, Debug)]
pub enum IlmCommands {
    /// Manage lifecycle rules on a bucket
    #[command(subcommand)]
    Rule(rule::RuleCommands),

    /// Manage remote storage tiers
    #[command(subcommand)]
    Tier(tier::TierCommands),

    /// Restore a transitioned (archived) object
    Restore(restore::RestoreArgs),
}

/// Execute the ilm command
pub async fn execute(args: IlmArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        IlmCommands::Rule(cmd) => rule::execute(cmd, output_config).await,
        IlmCommands::Tier(cmd) => tier::execute(cmd, output_config).await,
        IlmCommands::Restore(args) => restore::execute(args, output_config).await,
    }
}
