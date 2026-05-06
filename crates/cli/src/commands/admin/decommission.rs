//! Decommission commands for retiring server pools.

use clap::Subcommand;
use serde::Serialize;

use super::{get_admin_client, pool};
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, PoolStatus, PoolTarget};

/// Decommission subcommands
#[derive(Subcommand, Debug)]
pub enum DecommissionCommands {
    /// Start decommissioning a pool
    Start(StartArgs),

    /// Show decommissioning status
    Status(StatusArgs),

    /// Cancel decommissioning a pool
    Cancel(CancelArgs),
}

#[derive(clap::Args, Debug)]
pub struct StartArgs {
    /// Alias name of the server
    pub alias: String,

    /// Pool command line, comma-separated pool command lines, or zero-based pool ID with --by-id
    pub pool: String,

    /// Interpret POOL as zero-based pool ID values
    #[arg(long)]
    pub by_id: bool,
}

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Alias name of the server
    pub alias: String,

    /// Pool command line, or zero-based pool ID with --by-id
    pub pool: Option<String>,

    /// Interpret POOL as a zero-based pool ID
    #[arg(long)]
    pub by_id: bool,
}

#[derive(clap::Args, Debug)]
pub struct CancelArgs {
    /// Alias name of the server
    pub alias: String,

    /// Pool command line, or zero-based pool ID with --by-id
    pub pool: String,

    /// Interpret POOL as a zero-based pool ID
    #[arg(long)]
    pub by_id: bool,
}

#[derive(Serialize)]
struct DecommissionOperationOutput {
    success: bool,
    message: String,
    pool: String,
}

#[derive(Serialize)]
struct DecommissionStatusListOutput {
    pools: Vec<PoolStatus>,
}

/// Execute a decommission subcommand
pub async fn execute(cmd: DecommissionCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        DecommissionCommands::Start(args) => execute_start(args, formatter).await,
        DecommissionCommands::Status(args) => execute_status(args, formatter).await,
        DecommissionCommands::Cancel(args) => execute_cancel(args, formatter).await,
    }
}

async fn execute_start(args: StartArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let target = PoolTarget {
        pool: args.pool,
        by_id: args.by_id,
    };

    match client.decommission_start(target.clone()).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&DecommissionOperationOutput {
                    success: true,
                    message: "Decommission started successfully".to_string(),
                    pool: target.pool,
                });
            } else {
                formatter.success(&format!(
                    "Decommission started successfully for `{}`.",
                    target.pool
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to start decommission: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_status(args: StatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    if let Some(pool_name) = args.pool {
        let target = PoolTarget {
            pool: pool_name,
            by_id: args.by_id,
        };
        match client.pool_status(target).await {
            Ok(status) => {
                if formatter.is_json() {
                    formatter.json(&status);
                } else {
                    pool::print_pool_status(&status, formatter);
                }
                ExitCode::Success
            }
            Err(e) => {
                formatter.error(&format!("Failed to get decommission status: {e}"));
                ExitCode::GeneralError
            }
        }
    } else {
        match client.list_pools().await {
            Ok(pools) => {
                if formatter.is_json() {
                    formatter.json(&DecommissionStatusListOutput { pools });
                } else {
                    pool::print_pool_list(&pools, formatter);
                }
                ExitCode::Success
            }
            Err(e) => {
                formatter.error(&format!("Failed to get decommission status: {e}"));
                ExitCode::GeneralError
            }
        }
    }
}

async fn execute_cancel(args: CancelArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let target = PoolTarget {
        pool: args.pool,
        by_id: args.by_id,
    };

    match client.decommission_cancel(target.clone()).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&DecommissionOperationOutput {
                    success: true,
                    message: "Decommission canceled successfully".to_string(),
                    pool: target.pool,
                });
            } else {
                formatter.success(&format!(
                    "Decommission canceled successfully for `{}`.",
                    target.pool
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to cancel decommission: {e}"));
            ExitCode::GeneralError
        }
    }
}
