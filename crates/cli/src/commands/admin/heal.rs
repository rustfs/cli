//! Heal command for cluster healing operations
//!
//! Commands for checking heal status, starting/stopping heal operations.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, HealScanMode, HealStartRequest, HealStatus};

/// Heal subcommands
#[derive(Subcommand, Debug)]
pub enum HealCommands {
    /// Display current heal status
    Status(StatusArgs),

    /// Start a heal operation
    Start(StartArgs),

    /// Stop a running heal operation
    Stop(StopArgs),
}

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct StartArgs {
    /// Alias name of the server
    pub alias: String,

    /// Specific bucket to heal (default: all buckets)
    #[arg(short, long)]
    pub bucket: Option<String>,

    /// Object prefix to heal
    #[arg(short, long)]
    pub prefix: Option<String>,

    /// Scan mode (normal or deep)
    #[arg(long, default_value = "normal")]
    pub scan_mode: String,

    /// Remove dangling objects/parts
    #[arg(long)]
    pub remove: bool,

    /// Recreate missing data
    #[arg(long)]
    pub recreate: bool,

    /// Dry run mode - show what would be healed without actually healing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub struct StopArgs {
    /// Alias name of the server
    pub alias: String,
}

/// JSON output for heal status
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealStatusOutput {
    heal_id: String,
    healing: bool,
    bucket: String,
    object: String,
    items_scanned: u64,
    items_healed: u64,
    items_failed: u64,
    bytes_scanned: u64,
    bytes_healed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_update: Option<String>,
}

impl From<&HealStatus> for HealStatusOutput {
    fn from(status: &HealStatus) -> Self {
        Self {
            heal_id: status.heal_id.clone(),
            healing: status.healing,
            bucket: status.bucket.clone(),
            object: status.object.clone(),
            items_scanned: status.items_scanned,
            items_healed: status.items_healed,
            items_failed: status.items_failed,
            bytes_scanned: status.bytes_scanned,
            bytes_healed: status.bytes_healed,
            started: status.started.clone(),
            last_update: status.last_update.clone(),
        }
    }
}

/// JSON output for heal operations
#[derive(Serialize)]
struct HealOperationOutput {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<HealStatusOutput>,
}

/// Execute a heal subcommand
pub async fn execute(cmd: HealCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        HealCommands::Status(args) => execute_status(args, formatter).await,
        HealCommands::Start(args) => execute_start(args, formatter).await,
        HealCommands::Stop(args) => execute_stop(args, formatter).await,
    }
}

async fn execute_status(args: StatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.heal_status().await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&HealStatusOutput::from(&status));
            } else {
                print_heal_status(&status, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get heal status: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn print_heal_status(status: &HealStatus, formatter: &Formatter) {
    let healing_status = if status.healing {
        formatter.style_size("In Progress")
    } else {
        formatter.style_date("Idle")
    };

    formatter.println(&format!(
        "{} {}",
        formatter.style_name("Heal Status:"),
        healing_status
    ));
    formatter.println("");

    if !status.heal_id.is_empty() {
        formatter.println(&format!("  Heal ID:       {}", status.heal_id));
    }

    if status.healing {
        if !status.bucket.is_empty() {
            formatter.println(&format!(
                "  Current:       {}/{}",
                status.bucket, status.object
            ));
        }

        formatter.println(&format!(
            "  Items:         {} scanned, {} healed, {} failed",
            status.items_scanned, status.items_healed, status.items_failed
        ));

        formatter.println(&format!(
            "  Data:          {} scanned, {} healed",
            format_bytes(status.bytes_scanned),
            format_bytes(status.bytes_healed)
        ));

        if let Some(ref started) = status.started {
            formatter.println(&format!("  Started:       {}", started));
        }
        if let Some(ref last_update) = status.last_update {
            formatter.println(&format!("  Last Update:   {}", last_update));
        }
    } else {
        formatter.println("  No heal operation currently running.");
    }
}

async fn execute_start(args: StartArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    // Parse scan mode
    let scan_mode = match args.scan_mode.parse::<HealScanMode>() {
        Ok(mode) => mode,
        Err(e) => {
            formatter.error(&format!("Invalid scan mode: {e}"));
            return ExitCode::UsageError;
        }
    };

    let request = HealStartRequest {
        bucket: args.bucket,
        prefix: args.prefix,
        scan_mode,
        remove: args.remove,
        recreate: args.recreate,
        dry_run: args.dry_run,
    };

    match client.heal_start(request).await {
        Ok(status) => {
            if formatter.is_json() {
                let output = HealOperationOutput {
                    success: true,
                    message: "Heal operation started successfully".to_string(),
                    status: Some(HealStatusOutput::from(&status)),
                };
                formatter.json(&output);
            } else {
                if args.dry_run {
                    formatter.success("Heal operation started (DRY RUN mode).");
                } else {
                    formatter.success("Heal operation started successfully.");
                }
                formatter.println("");
                print_heal_status(&status, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to start heal operation: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_stop(args: StopArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.heal_stop().await {
        Ok(()) => {
            if formatter.is_json() {
                let output = HealOperationOutput {
                    success: true,
                    message: "Heal operation stopped successfully".to_string(),
                    status: None,
                };
                formatter.json(&output);
            } else {
                formatter.success("Heal operation stopped successfully.");
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to stop heal operation: {e}"));
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

    if bytes >= TB {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
    }

    #[test]
    fn test_heal_operation_output_serialization() {
        let status = HealStatus {
            heal_id: "heal-123".to_string(),
            healing: true,
            bucket: "test-bucket".to_string(),
            object: "test/object.txt".to_string(),
            items_scanned: 1000,
            items_healed: 50,
            items_failed: 5,
            bytes_scanned: 1024 * 1024 * 100,
            bytes_healed: 1024 * 1024 * 5,
            started: Some("2024-01-01T10:00:00Z".to_string()),
            last_update: Some("2024-01-01T10:30:00Z".to_string()),
        };

        let output = HealOperationOutput {
            success: true,
            message: "Heal operation started successfully".to_string(),
            status: Some(HealStatusOutput::from(&status)),
        };

        let value = serde_json::to_value(&output).expect("serialize heal operation output");
        let status_value = value
            .get("status")
            .expect("status field exists")
            .as_object()
            .expect("status is object");
        assert!(status_value.get("healId").is_some());
        assert!(status_value.get("itemsScanned").is_some());
    }

    #[test]
    fn test_heal_status_output_from() {
        let status = HealStatus {
            heal_id: "heal-123".to_string(),
            healing: true,
            bucket: "test-bucket".to_string(),
            object: "test/object.txt".to_string(),
            items_scanned: 1000,
            items_healed: 50,
            items_failed: 5,
            bytes_scanned: 1024 * 1024 * 100,
            bytes_healed: 1024 * 1024 * 5,
            started: Some("2024-01-01T10:00:00Z".to_string()),
            last_update: Some("2024-01-01T10:30:00Z".to_string()),
        };

        let output = HealStatusOutput::from(&status);
        assert_eq!(output.heal_id, "heal-123");
        assert!(output.healing);
        assert_eq!(output.bucket, "test-bucket");
        assert_eq!(output.items_scanned, 1000);
        assert_eq!(output.items_healed, 50);
    }
}
