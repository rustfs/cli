//! Pool commands for cluster expansion and pool status.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, PoolDecommissionInfo, PoolStatus, PoolTarget};

/// Pool subcommands
#[derive(Subcommand, Debug)]
pub enum PoolCommands {
    /// List server pools
    List(ListArgs),

    /// Show server pool status
    Status(StatusArgs),
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Alias name of the server
    pub alias: String,
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

#[derive(Serialize)]
struct PoolListOutput {
    pools: Vec<PoolStatus>,
}

/// Execute a pool subcommand
pub async fn execute(cmd: PoolCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        PoolCommands::List(args) => execute_list(args, formatter).await,
        PoolCommands::Status(args) => execute_status(args, formatter).await,
    }
}

async fn execute_list(args: ListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.list_pools().await {
        Ok(pools) => {
            if formatter.is_json() {
                formatter.json(&PoolListOutput { pools });
            } else {
                print_pool_list(&pools, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to list pools: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_status(args: StatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    if let Some(pool) = args.pool {
        let target = PoolTarget {
            pool,
            by_id: args.by_id,
        };
        match client.pool_status(target).await {
            Ok(status) => {
                if formatter.is_json() {
                    formatter.json(&status);
                } else {
                    print_pool_status(&status, formatter);
                }
                ExitCode::Success
            }
            Err(e) => {
                formatter.error(&format!("Failed to get pool status: {e}"));
                ExitCode::GeneralError
            }
        }
    } else {
        match client.list_pools().await {
            Ok(pools) => {
                if formatter.is_json() {
                    formatter.json(&PoolListOutput { pools });
                } else {
                    print_pool_list(&pools, formatter);
                }
                ExitCode::Success
            }
            Err(e) => {
                formatter.error(&format!("Failed to get pool status: {e}"));
                ExitCode::GeneralError
            }
        }
    }
}

pub(super) fn print_pool_list(pools: &[PoolStatus], formatter: &Formatter) {
    formatter.println(&formatter.style_name("Pools:"));
    if pools.is_empty() {
        formatter.println("  No pools found.");
        return;
    }

    for pool in pools {
        print_pool_status(pool, formatter);
    }
}

pub(super) fn print_pool_status(pool: &PoolStatus, formatter: &Formatter) {
    let decommission = pool.decommission.as_ref();
    let lifecycle_state = pool_lifecycle_state(pool);
    let decommission_state = pool_decommission_state(pool);
    let rebalance_state = status_or_default(&pool.rebalance_status, "none");
    let used = decommission
        .map(|info| info.total_size.saturating_sub(info.current_size))
        .or_else(|| (pool.total_size > 0).then_some(pool.used_size))
        .unwrap_or_default();
    let total = decommission
        .map(|info| info.total_size)
        .or_else(|| (pool.total_size > 0).then_some(pool.total_size))
        .unwrap_or_default();

    formatter.println(&format!(
        "  Pool {}: {}",
        pool.id,
        formatter.style_url(&pool.cmd_line)
    ));
    formatter.println(&format!(
        "    Status:       {}",
        style_state(lifecycle_state, formatter)
    ));
    formatter.println(&format!(
        "    Decommission: {}",
        style_state(decommission_state, formatter)
    ));
    formatter.println(&format!(
        "    Rebalance:    {}",
        style_state(rebalance_state, formatter)
    ));

    if total > 0 {
        formatter.println(&format!(
            "    Usage:        {} / {}",
            formatter.style_size(&format_bytes(used)),
            format_bytes(total)
        ));
    }

    if let Some(info) = decommission
        && (info.objects_decommissioned > 0
            || info.objects_decommissioned_failed > 0
            || info.bytes_decommissioned > 0
            || info.bytes_decommissioned_failed > 0)
    {
        formatter.println(&format!(
            "    Progress:     {}, {} failed; {}, {} failed bytes",
            info.objects_decommissioned,
            info.objects_decommissioned_failed,
            format_bytes(info.bytes_decommissioned),
            format_bytes(info.bytes_decommissioned_failed)
        ));
    }
}

fn pool_lifecycle_state(pool: &PoolStatus) -> &str {
    if !pool.status.is_empty() {
        return pool.status.as_str();
    }

    match decommission_state(pool.decommission.as_ref()) {
        "complete" => "decommissioned",
        "failed" | "canceled" => "blocked",
        _ => "active",
    }
}

fn pool_decommission_state(pool: &PoolStatus) -> &str {
    if !pool.decommission_status.is_empty() {
        return pool.decommission_status.as_str();
    }

    decommission_state(pool.decommission.as_ref())
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

pub(super) fn style_state(state: &str, formatter: &Formatter) -> String {
    match state {
        "active" | "complete" | "completed" | "decommissioned" => formatter.style_size(state),
        "blocked" | "failed" => formatter.theme().error.apply_to(state).to_string(),
        "canceled" | "queued" | "stopping" | "stopped" => {
            formatter.theme().warning.apply_to(state).to_string()
        }
        "decommissioning" | "running" | "started" => formatter.style_name(state),
        _ => formatter.style_date(state),
    }
}

fn status_or_default<'a>(status: &'a str, default: &'static str) -> &'a str {
    if status.is_empty() { default } else { status }
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.2} TiB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decommission_state() {
        assert_eq!(decommission_state(None), "none");
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo::default())),
            "none"
        );
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo {
                queued: true,
                ..Default::default()
            })),
            "queued"
        );
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo {
                start_time: Some("2026-05-06T00:00:00Z".to_string()),
                ..Default::default()
            })),
            "running"
        );
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo {
                complete: true,
                ..Default::default()
            })),
            "complete"
        );
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo {
                failed: true,
                ..Default::default()
            })),
            "failed"
        );
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo {
                canceled: true,
                ..Default::default()
            })),
            "canceled"
        );
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 1024), "1.00 TiB");
    }
}
