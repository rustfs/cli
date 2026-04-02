//! object command group - Noun-first object workflows.
//!
//! Provides an agent-friendly object-oriented entrypoint while delegating to the
//! existing command implementations.

use clap::{Args, Subcommand};

use crate::exit_code::ExitCode;
use crate::output::OutputConfig;

use super::{cat, cp, find, head, ls, mv, rm, share, stat, tree};

const OBJECT_AFTER_HELP: &str = "\
Examples:
  rc object list local/my-bucket/logs/
  rc object copy ./report.json local/my-bucket/reports/
  rc object show local/my-bucket/report.json
  rc object remove local/my-bucket/report.json --dry-run
  rc object share local/my-bucket/report.json --expire 1d";

/// Manage object-oriented workflows
#[derive(Args, Debug)]
#[command(after_help = OBJECT_AFTER_HELP)]
pub struct ObjectArgs {
    #[command(subcommand)]
    pub command: ObjectCommands,
}

#[derive(Subcommand, Debug)]
pub enum ObjectCommands {
    /// List objects within a bucket path
    #[command(alias = "ls")]
    List(ls::LsArgs),

    /// Copy objects between local and remote locations
    Copy(cp::CpArgs),

    /// Move objects between local and remote locations
    Move(mv::MvArgs),

    /// Remove objects
    Remove(rm::RmArgs),

    /// Show object metadata
    Stat(stat::StatArgs),

    /// Print the full object body
    Show(cat::CatArgs),

    /// Print the first lines or bytes of an object
    Head(head::HeadArgs),

    /// Search for objects by filters
    Find(find::FindArgs),

    /// Display objects in a tree view
    Tree(tree::TreeArgs),

    /// Generate a presigned object URL
    Share(share::ShareArgs),
}

/// Execute the object command group
pub async fn execute(args: ObjectArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        ObjectCommands::List(args) => ls::execute(args, output_config).await,
        ObjectCommands::Copy(args) => cp::execute(args, output_config).await,
        ObjectCommands::Move(args) => mv::execute(args, output_config).await,
        ObjectCommands::Remove(args) => rm::execute(args, output_config).await,
        ObjectCommands::Stat(args) => stat::execute(args, output_config).await,
        ObjectCommands::Show(args) => cat::execute(args, output_config).await,
        ObjectCommands::Head(args) => head::execute(args, output_config).await,
        ObjectCommands::Find(args) => find::execute(args, output_config).await,
        ObjectCommands::Tree(args) => tree::execute(args, output_config).await,
        ObjectCommands::Share(args) => share::execute(args, output_config).await,
    }
}
