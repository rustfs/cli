//! Info command for cluster information
//!
//! Commands for displaying cluster status, disk info, and server information.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, ClusterInfo, DiskInfo, ServerInfo};

/// Info subcommands
#[derive(Subcommand, Debug)]
pub enum InfoCommands {
    /// Display cluster overview information
    #[command(name = "cluster")]
    Cluster(ClusterArgs),

    /// Display server information
    #[command(name = "server")]
    Server(ServerArgs),

    /// Display disk information
    #[command(name = "disk")]
    Disk(DiskArgs),
}

#[derive(clap::Args, Debug)]
pub struct ClusterArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct ServerArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct DiskArgs {
    /// Alias name of the server
    pub alias: String,

    /// Show only offline disks
    #[arg(long)]
    pub offline: bool,

    /// Show only healing disks
    #[arg(long)]
    pub healing: bool,
}

/// JSON output for cluster info
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClusterOutput {
    mode: String,
    deployment_id: String,
    region: String,
    servers: usize,
    online_disks: usize,
    offline_disks: usize,
    total_capacity: u64,
    used_capacity: u64,
    buckets: u64,
    objects: u64,
}

/// JSON output for server list
#[derive(Serialize)]
struct ServerListOutput {
    servers: Vec<ServerOutput>,
}

/// JSON output for a single server
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServerOutput {
    endpoint: String,
    state: String,
    version: String,
    uptime: u64,
    disks: usize,
    online_disks: usize,
    offline_disks: usize,
}

impl From<&ServerInfo> for ServerOutput {
    fn from(server: &ServerInfo) -> Self {
        let online = server
            .disks
            .iter()
            .filter(|d| d.state == "online" || d.state == "ok")
            .count();
        let offline = server.disks.iter().filter(|d| d.state == "offline").count();

        Self {
            endpoint: server.endpoint.clone(),
            state: server.state.clone(),
            version: server.version.clone(),
            uptime: server.uptime,
            disks: server.disks.len(),
            online_disks: online,
            offline_disks: offline,
        }
    }
}

/// JSON output for disk list
#[derive(Serialize)]
struct DiskListOutput {
    disks: Vec<DiskOutput>,
}

/// JSON output for a single disk
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiskOutput {
    endpoint: String,
    path: String,
    state: String,
    uuid: String,
    total_space: u64,
    used_space: u64,
    available_space: u64,
    healing: bool,
    pool_index: i32,
    set_index: i32,
    disk_index: i32,
}

impl From<&DiskInfo> for DiskOutput {
    fn from(disk: &DiskInfo) -> Self {
        Self {
            endpoint: disk.endpoint.clone(),
            path: disk.drive_path.clone(),
            state: disk.state.clone(),
            uuid: disk.uuid.clone(),
            total_space: disk.total_space,
            used_space: disk.used_space,
            available_space: disk.available_space,
            healing: disk.healing,
            pool_index: disk.pool_index,
            set_index: disk.set_index,
            disk_index: disk.disk_index,
        }
    }
}

/// Execute an info subcommand
pub async fn execute(cmd: InfoCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        InfoCommands::Cluster(args) => execute_cluster(args, formatter).await,
        InfoCommands::Server(args) => execute_server(args, formatter).await,
        InfoCommands::Disk(args) => execute_disk(args, formatter).await,
    }
}

async fn execute_cluster(args: ClusterArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.cluster_info().await {
        Ok(info) => {
            if formatter.is_json() {
                let output = ClusterOutput {
                    mode: info
                        .mode
                        .clone()
                        .unwrap_or_else(|| "standalone".to_string()),
                    deployment_id: info.deployment_id.clone().unwrap_or_default(),
                    region: info
                        .region
                        .clone()
                        .unwrap_or_else(|| "us-east-1".to_string()),
                    servers: info.servers.as_ref().map(|s| s.len()).unwrap_or(0),
                    online_disks: info.online_disks(),
                    offline_disks: info.offline_disks(),
                    total_capacity: info.total_capacity(),
                    used_capacity: info.used_capacity(),
                    buckets: info.buckets.as_ref().map(|b| b.count).unwrap_or(0),
                    objects: info.objects.as_ref().map(|o| o.count).unwrap_or(0),
                };
                formatter.json(&output);
            } else {
                print_cluster_info(&info, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get cluster info: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn print_cluster_info(info: &ClusterInfo, formatter: &Formatter) {
    let mode = info.mode.as_deref().unwrap_or("standalone");
    let deployment_id = info.deployment_id.as_deref().unwrap_or("unknown");
    let region = info.region.as_deref().unwrap_or("us-east-1");

    formatter.println(&format!(
        "{}  {}",
        formatter.style_name("Cluster Information"),
        formatter.style_date(&format!("({})", mode))
    ));
    formatter.println("");

    // Basic info
    formatter.println(&format!(
        "  Deployment ID: {}",
        formatter.style_name(deployment_id)
    ));
    formatter.println(&format!("  Region:        {}", region));

    // Server info
    let server_count = info.servers.as_ref().map(|s| s.len()).unwrap_or(0);
    formatter.println(&format!("  Servers:       {}", server_count));

    // Disk info
    let online = info.online_disks();
    let offline = info.offline_disks();
    let total = online + offline;
    if offline > 0 {
        formatter.println(&format!(
            "  Disks:         {} ({} online, {} offline)",
            total,
            formatter.style_size(&online.to_string()),
            formatter.style_date(&offline.to_string())
        ));
    } else {
        formatter.println(&format!(
            "  Disks:         {} ({} online)",
            total,
            formatter.style_size(&online.to_string())
        ));
    }

    // Storage info
    let total_bytes = info.total_capacity();
    let used_bytes = info.used_capacity();
    if total_bytes > 0 {
        let usage_pct = (used_bytes as f64 / total_bytes as f64 * 100.0) as u8;
        formatter.println(&format!(
            "  Storage:       {} / {} ({}%)",
            format_bytes(used_bytes),
            format_bytes(total_bytes),
            usage_pct
        ));
    }

    // Object info
    if let Some(ref buckets) = info.buckets {
        formatter.println(&format!("  Buckets:       {}", buckets.count));
    }
    if let Some(ref objects) = info.objects {
        formatter.println(&format!("  Objects:       {}", objects.count));
    }

    // Backend info
    if let Some(ref backend) = info.backend {
        formatter.println("");
        formatter.println(&format!(
            "  Backend:       {}",
            formatter.style_name(&backend.backend_type.to_string())
        ));
        if let Some(parity) = backend.standard_sc_parity {
            formatter.println(&format!("  EC Parity:     {}", parity));
        }
    }
}

async fn execute_server(args: ServerArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.cluster_info().await {
        Ok(info) => {
            let servers = info.servers.unwrap_or_default();

            if formatter.is_json() {
                let output = ServerListOutput {
                    servers: servers.iter().map(ServerOutput::from).collect(),
                };
                formatter.json(&output);
            } else if servers.is_empty() {
                formatter.println("No servers found.");
            } else {
                formatter.println(&format!(
                    "{} ({} servers)",
                    formatter.style_name("Server List"),
                    servers.len()
                ));
                formatter.println("");

                for server in &servers {
                    let state_icon = if server.state == "online" {
                        formatter.style_size("●")
                    } else {
                        formatter.style_date("○")
                    };
                    let endpoint = formatter.style_name(&server.endpoint);
                    let version = formatter.style_date(&server.version);
                    let uptime = format_duration(server.uptime);

                    formatter.println(&format!("{} {} [{}]", state_icon, endpoint, version));
                    formatter.println(&format!(
                        "    Uptime: {} | Disks: {}",
                        uptime,
                        server.disks.len()
                    ));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get server info: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_disk(args: DiskArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.cluster_info().await {
        Ok(info) => {
            let mut disks: Vec<&DiskInfo> = info
                .servers
                .as_ref()
                .map(|servers| servers.iter().flat_map(|s| &s.disks).collect())
                .unwrap_or_default();

            // Filter by options
            if args.offline {
                disks.retain(|d| d.state == "offline");
            }
            if args.healing {
                disks.retain(|d| d.healing);
            }

            if formatter.is_json() {
                let output = DiskListOutput {
                    disks: disks.iter().map(|d| DiskOutput::from(*d)).collect(),
                };
                formatter.json(&output);
            } else if disks.is_empty() {
                formatter.println("No disks found matching criteria.");
            } else {
                formatter.println(&format!(
                    "{} ({} disks)",
                    formatter.style_name("Disk List"),
                    disks.len()
                ));
                formatter.println("");

                for disk in disks {
                    let state_icon = match disk.state.as_str() {
                        "online" | "ok" => formatter.style_size("●"),
                        "offline" => formatter.style_date("○"),
                        _ => formatter.style_date("?"),
                    };

                    let healing_badge = if disk.healing {
                        format!(" {}", formatter.style_date("[healing]"))
                    } else {
                        String::new()
                    };

                    let path = formatter.style_name(&disk.drive_path);
                    let location = format!(
                        "pool:{} set:{} disk:{}",
                        disk.pool_index, disk.set_index, disk.disk_index
                    );

                    formatter.println(&format!(
                        "{} {}{} ({})",
                        state_icon, path, healing_badge, location
                    ));

                    if disk.total_space > 0 {
                        let usage_pct =
                            (disk.used_space as f64 / disk.total_space as f64 * 100.0) as u8;
                        formatter.println(&format!(
                            "    {} / {} ({}%)",
                            format_bytes(disk.used_space),
                            format_bytes(disk.total_space),
                            usage_pct
                        ));
                    }
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get disk info: {e}"));
            ExitCode::GeneralError
        }
    }
}

/// Format bytes into human-readable form
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    const PB: u64 = TB * 1024;

    if bytes >= PB {
        format!("{:.2} PiB", bytes as f64 / PB as f64)
    } else if bytes >= TB {
        format!("{:.2} TiB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format duration in seconds to human-readable form
fn format_duration(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;

    if days > 0 {
        format!("{}d {}h {}m", days, hours, minutes)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_output_serialization_keys() {
        let output = ClusterOutput {
            mode: "distributed".to_string(),
            deployment_id: "deploy-1".to_string(),
            region: "us-east-1".to_string(),
            servers: 4,
            online_disks: 8,
            offline_disks: 1,
            total_capacity: 100,
            used_capacity: 50,
            buckets: 3,
            objects: 42,
        };

        let value = serde_json::to_value(&output).expect("serialize cluster output");
        assert!(value.get("deploymentId").is_some());
        assert!(value.get("onlineDisks").is_some());
        assert!(value.get("usedCapacity").is_some());
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 1024), "1.00 TiB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0m");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(86400), "1d 0h 0m");
        assert_eq!(format_duration(90061), "1d 1h 1m");
    }

    #[test]
    fn test_server_output_from() {
        let server = ServerInfo {
            endpoint: "http://localhost:9000".to_string(),
            state: "online".to_string(),
            version: "1.0.0".to_string(),
            uptime: 3600,
            disks: vec![
                DiskInfo {
                    state: "online".to_string(),
                    ..Default::default()
                },
                DiskInfo {
                    state: "offline".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let output = ServerOutput::from(&server);
        assert_eq!(output.endpoint, "http://localhost:9000");
        assert_eq!(output.state, "online");
        assert_eq!(output.disks, 2);
        assert_eq!(output.online_disks, 1);
        assert_eq!(output.offline_disks, 1);
    }

    #[test]
    fn test_disk_output_from() {
        let disk = DiskInfo {
            endpoint: "http://localhost:9000".to_string(),
            drive_path: "/data/disk1".to_string(),
            state: "online".to_string(),
            uuid: "test-uuid".to_string(),
            total_space: 1000000000,
            used_space: 500000000,
            available_space: 500000000,
            healing: false,
            pool_index: 0,
            set_index: 1,
            disk_index: 2,
            ..Default::default()
        };

        let output = DiskOutput::from(&disk);
        assert_eq!(output.path, "/data/disk1");
        assert_eq!(output.state, "online");
        assert!(!output.healing);
        assert_eq!(output.pool_index, 0);
        assert_eq!(output.set_index, 1);
        assert_eq!(output.disk_index, 2);
    }
}
