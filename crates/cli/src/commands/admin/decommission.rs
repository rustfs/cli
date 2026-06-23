//! Decommission commands for retiring server pools.

use clap::Subcommand;
use serde::Serialize;

use super::{get_admin_client, pool};
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, DecommissionPoolStatus, PoolDecommissionInfo, PoolTarget};

/// Decommission subcommands
#[derive(Subcommand, Debug)]
pub enum DecommissionCommands {
    /// Start decommissioning a pool
    Start(StartArgs),

    /// Show decommissioning status
    Status(StatusArgs),

    /// Cancel decommissioning a pool
    Cancel(CancelArgs),

    /// Clear failed or canceled decommissioning metadata for a pool
    Clear(ClearArgs),
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

#[derive(clap::Args, Debug)]
pub struct ClearArgs {
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

/// Execute a decommission subcommand
pub async fn execute(cmd: DecommissionCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        DecommissionCommands::Start(args) => execute_start(args, formatter).await,
        DecommissionCommands::Status(args) => execute_status(args, formatter).await,
        DecommissionCommands::Cancel(args) => execute_cancel(args, formatter).await,
        DecommissionCommands::Clear(args) => execute_clear(args, formatter).await,
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

    let target = args.pool.map(|pool| PoolTarget {
        pool,
        by_id: args.by_id,
    });

    match client.decommission_status(target).await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&status);
            } else {
                print_decommission_status(&status.pools, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get decommission status: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn print_decommission_status(pools: &[DecommissionPoolStatus], formatter: &Formatter) {
    formatter.println(&formatter.style_name("Decommission:"));
    if pools.is_empty() {
        formatter.println("  No decommission status found.");
        return;
    }

    for item in pools {
        let decommission = item.decommission.as_ref();
        let operation_status = status_or_default(&item.status, decommission_state(decommission));
        let pool_status = status_or_default(&item.pool_status, "unknown");

        formatter.println(&format!(
            "  Pool {}: {}",
            item.id,
            formatter.style_url(&item.cmd_line)
        ));
        formatter.println(&format!(
            "    Status:       {}",
            pool::style_state(operation_status, formatter)
        ));
        formatter.println(&format!(
            "    Pool status:  {}",
            pool::style_state(pool_status, formatter)
        ));

        if let Some(info) = decommission {
            let used = info.total_size.saturating_sub(info.current_size);
            if info.total_size > 0 {
                formatter.println(&format!(
                    "    Usage:        {} / {}",
                    formatter.style_size(&pool::format_bytes(used)),
                    pool::format_bytes(info.total_size)
                ));
            }

            if info.objects_decommissioned > 0
                || info.objects_decommissioned_failed > 0
                || info.bytes_decommissioned > 0
                || info.bytes_decommissioned_failed > 0
            {
                formatter.println(&format!(
                    "    Progress:     {}, {} failed; {}, {} failed bytes",
                    info.objects_decommissioned,
                    info.objects_decommissioned_failed,
                    pool::format_bytes(info.bytes_decommissioned),
                    pool::format_bytes(info.bytes_decommissioned_failed)
                ));
            }
        }
    }
}

fn status_or_default<'a>(status: &'a str, default: &'a str) -> &'a str {
    if status.is_empty() { default } else { status }
}

fn decommission_state(decommission: Option<&PoolDecommissionInfo>) -> &'static str {
    let Some(info) = decommission else {
        return "none";
    };

    if info.complete {
        "complete"
    } else if info.failed {
        "failed"
    } else if info.canceled {
        "canceled"
    } else if info.queued {
        "queued"
    } else if info.start_time.is_some() {
        "running"
    } else {
        "none"
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

async fn execute_clear(args: ClearArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let target = PoolTarget {
        pool: args.pool,
        by_id: args.by_id,
    };

    match client.decommission_clear(target.clone()).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&DecommissionOperationOutput {
                    success: true,
                    message: "Decommission cleared successfully".to_string(),
                    pool: target.pool,
                });
            } else {
                formatter.success(&format!(
                    "Decommission cleared successfully for `{}`.",
                    target.pool
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to clear decommission: {e}"));
            ExitCode::GeneralError
        }
    }
}
