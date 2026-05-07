//! Expansion workflow commands for post-expansion data rebalancing.

use clap::Subcommand;

use super::rebalance;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const EXPAND_AFTER_HELP: &str = "\
Examples:
  rc admin expand start local
  rc admin expand status local
  rc admin expand stop local

Notes:
  Add server pools by updating the RustFS service configuration and restarting
  the deployment first. This command manages the post-expansion rebalance step.";

/// Expansion workflow subcommands
#[derive(Subcommand, Debug)]
#[command(after_help = EXPAND_AFTER_HELP)]
pub enum ExpandCommands {
    /// Start post-expansion data rebalancing
    Start(rebalance::StartArgs),

    /// Show post-expansion rebalance status
    Status(rebalance::StatusArgs),

    /// Stop a running post-expansion rebalance
    Stop(rebalance::StopArgs),
}

/// Execute an expansion workflow subcommand
pub async fn execute(cmd: ExpandCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        ExpandCommands::Start(args) => {
            rebalance::execute_start_with_messages(
                args,
                formatter,
                "Expansion rebalance started successfully",
                "Failed to start expansion rebalance",
            )
            .await
        }
        ExpandCommands::Status(args) => rebalance::execute_status(args, formatter).await,
        ExpandCommands::Stop(args) => {
            rebalance::execute_stop_with_messages(
                args,
                formatter,
                "Expansion rebalance stopped successfully",
                "Failed to stop expansion rebalance",
            )
            .await
        }
    }
}
