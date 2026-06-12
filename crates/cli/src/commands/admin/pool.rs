//! Pool commands for cluster expansion and pool status.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, ClusterInfo, PoolDecommissionInfo, PoolStatus, PoolTarget};

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
        match client.cluster_info().await {
            Ok(info) => {
                let pools = pool_statuses_from_cluster_info(&info);
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

fn pool_statuses_from_cluster_info(info: &ClusterInfo) -> Vec<PoolStatus> {
    let mut pools: Vec<PoolStatus> = info
        .pools
        .as_ref()
        .map(|pools| {
            pools
                .iter()
                .filter_map(|(pool_id, sets)| {
                    let id = usize::try_from(*pool_id).ok()?;
                    let raw_capacity = sets.values().map(|set| set.raw_capacity).sum::<u64>();
                    let raw_usage = sets.values().map(|set| set.raw_usage).sum::<u64>();

                    Some(PoolStatus {
                        id,
                        cmd_line: format!("pool {pool_id}"),
                        decommission: Some(PoolDecommissionInfo {
                            total_size: raw_capacity,
                            current_size: raw_capacity.saturating_sub(raw_usage),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    if pools.is_empty() {
        pools = info
            .servers
            .as_ref()
            .map(|servers| {
                let mut pool_ids = servers
                    .iter()
                    .flat_map(|server| &server.disks)
                    .filter_map(|disk| usize::try_from(disk.pool_index).ok())
                    .collect::<Vec<_>>();
                pool_ids.sort_unstable();
                pool_ids.dedup();
                pool_ids
                    .into_iter()
                    .map(|id| PoolStatus {
                        id,
                        cmd_line: format!("pool {id}"),
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    pools
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
    let state = decommission_state(decommission);
    let used = decommission
        .map(|info| info.total_size.saturating_sub(info.current_size))
        .unwrap_or_default();
    let total = decommission.map(|info| info.total_size).unwrap_or_default();

    formatter.println(&format!(
        "  Pool {}: {}",
        pool.id,
        formatter.style_url(&pool.cmd_line)
    ));
    formatter.println(&format!(
        "    Decommission: {}",
        style_state(state, formatter)
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

fn decommission_state(decommission: Option<&PoolDecommissionInfo>) -> &'static str {
    let Some(info) = decommission else {
        return "not started";
    };

    if info.complete {
        "complete"
    } else if info.failed {
        "failed"
    } else if info.canceled {
        "canceled"
    } else if info.start_time.is_some() {
        "running"
    } else {
        "not started"
    }
}

fn style_state(state: &str, formatter: &Formatter) -> String {
    match state {
        "complete" => formatter.style_size(state),
        "failed" => formatter.theme().error.apply_to(state).to_string(),
        "canceled" => formatter.theme().warning.apply_to(state).to_string(),
        "running" => formatter.style_name(state),
        _ => formatter.style_date(state),
    }
}

fn format_bytes(bytes: u64) -> String {
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
        assert_eq!(decommission_state(None), "not started");
        assert_eq!(
            decommission_state(Some(&PoolDecommissionInfo::default())),
            "not started"
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
