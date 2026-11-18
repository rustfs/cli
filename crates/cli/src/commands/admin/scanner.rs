//! Scanner health diagnostics and cycle recovery.

use clap::{Args, Subcommand};
use rc_core::admin::{ObservabilityApi, ScannerRecoveryApi, ScannerStatus};
use serde::Serialize;

use super::{emit_observability_error, get_admin_client};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
pub enum ScannerCommands {
    /// Display scanner health, freshness, and current cycle details
    Status(ScannerStatusArgs),
    /// Reset scanner recovery state
    #[command(subcommand)]
    Reset(ScannerResetCommands),
}

#[derive(Subcommand, Debug)]
pub enum ScannerResetCommands {
    /// Reset cycle recovery state for a full rescan
    Cycle(ScannerCycleResetArgs),
}

#[derive(Args, Debug)]
pub struct ScannerCycleResetArgs {
    /// Alias name of the server
    pub alias: String,

    /// Confirm the cycle reset (requires admin:ConfigUpdate)
    #[arg(long, required = true)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct ScannerStatusArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(Debug, Serialize)]
struct ScannerSuccessOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct ScannerOutput<'a> {
    health: rc_core::admin::ScannerHealth,
    #[serde(flatten)]
    status: &'a ScannerStatus,
}

pub async fn execute(command: ScannerCommands, formatter: &Formatter) -> ExitCode {
    match command {
        ScannerCommands::Status(args) => execute_status(args, formatter).await,
        ScannerCommands::Reset(ScannerResetCommands::Cycle(args)) => {
            execute_cycle_reset(args, formatter).await
        }
    }
}

async fn execute_status(args: ScannerStatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.scanner_status().await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&ScannerSuccessOutput {
                    schema_version: 3,
                    output_type: "scanner_status",
                    status: "success",
                    data: ScannerOutput {
                        health: status.health(),
                        status: &status,
                    },
                });
            } else {
                print_status(&status, formatter);
            }
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "scanner_status",
            "admin.scanner-status",
            "Failed to get scanner status",
            &error,
            formatter,
        ),
    }
}

async fn execute_cycle_reset(args: ScannerCycleResetArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.reset_scanner_cycle().await {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&ScannerSuccessOutput {
                    schema_version: 3,
                    output_type: "scanner_cycle_reset",
                    status: "success",
                    data: result,
                });
            } else {
                formatter.println("Scanner cycle state reset (full-rescan).");
            }
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "scanner_cycle_reset",
            "admin.scanner-cycle-reset",
            "Failed to reset scanner cycle state",
            &error,
            formatter,
        ),
    }
}

fn print_status(status: &ScannerStatus, formatter: &Formatter) {
    formatter.println(&formatter.style_name("Scanner Status"));
    formatter.println("");
    formatter.println(&format!("Health:         {}", status.health()));
    formatter.println(&format!("Enabled:        {}", status.enabled));
    formatter.println(&format!("Freshness:      {}", status.freshness.state));
    formatter.println(&format!("Current cycle:  {}", status.metrics.current_cycle));
    formatter.println(&format!(
        "Last result:    {}",
        value_or_unknown(&status.metrics.last_cycle_result)
    ));
    formatter.println(&format!(
        "Collected at:   {}",
        value_or_unknown(&status.metrics.collected_at)
    ));
    if let Some(reason) = status
        .freshness
        .reason
        .as_deref()
        .or(status.disabled_reason.as_deref())
        .filter(|reason| !reason.is_empty())
    {
        formatter.println(&format!(
            "Reason:         {}",
            formatter.sanitize_text(reason)
        ));
    }
    if let Some(recovery) = status.extra.get("cycle_recovery") {
        if let Some(state) = recovery["state"].as_str() {
            formatter.println(&format!(
                "Recovery state:  {}",
                formatter.sanitize_text(state)
            ));
        }
        if let Some(reason) = recovery["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
        {
            formatter.println(&format!(
                "Recovery reason: {}",
                formatter.sanitize_text(reason)
            ));
        }
    }
}

fn value_or_unknown(value: &str) -> &str {
    if value.is_empty() { "unknown" } else { value }
}
