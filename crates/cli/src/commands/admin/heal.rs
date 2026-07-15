//! Heal command for cluster healing operations
//!
//! Commands for checking heal status, starting/stopping heal operations.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{
    AdminApi, HealRuntimeState, HealScanMode, HealStartRequest, HealStatus, HealTaskRequest,
};

const HEAL_STOP_SUCCESS_MESSAGE: &str = "Heal operation stopped successfully";
const HEAL_STOP_STILL_RUNNING_MESSAGE: &str =
    "Heal stop request was accepted, but heal status is still in progress";

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

    /// Bucket for token-scoped manual heal task status
    #[arg(short, long)]
    pub bucket: Option<String>,

    /// Object prefix for token-scoped manual heal task status
    #[arg(short, long)]
    pub prefix: Option<String>,

    /// Client token returned by heal start
    #[arg(long)]
    pub client_token: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct StartArgs {
    /// Alias name of the server
    pub alias: String,

    /// Bucket to heal; omit to recursively heal all buckets
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

    /// Bucket for token-scoped manual heal task stop
    #[arg(short, long)]
    pub bucket: Option<String>,

    /// Object prefix for token-scoped manual heal task stop
    #[arg(short, long)]
    pub prefix: Option<String>,

    /// Client token returned by heal start
    #[arg(long)]
    pub client_token: Option<String>,
}

/// JSON output for heal status
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealStatusOutput {
    heal_id: String,
    healing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<HealRuntimeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    bucket: String,
    object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scan_mode: Option<HealScanMode>,
    scan_cycle: u64,
    heal_queue_length: u64,
    heal_active_tasks: u64,
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
            state: status.state,
            summary: status.summary.clone(),
            detail: status.detail.clone(),
            bucket: status.bucket.clone(),
            object: status.object.clone(),
            scan_mode: status.scan_mode,
            scan_cycle: status.scan_cycle,
            heal_queue_length: status.heal_queue_length,
            heal_active_tasks: status.heal_active_tasks,
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

fn has_heal_status_details(status: &HealStatus) -> bool {
    status.healing
        || status.state.is_some()
        || !status.heal_id.is_empty()
        || status.summary.is_some()
        || status.detail.is_some()
        || !status.bucket.is_empty()
        || !status.object.is_empty()
        || status.scan_mode.is_some()
        || status.scan_cycle > 0
        || status.heal_queue_length > 0
        || status.heal_active_tasks > 0
        || status.items_scanned > 0
        || status.items_healed > 0
        || status.items_failed > 0
        || status.bytes_scanned > 0
        || status.bytes_healed > 0
        || status.started.is_some()
        || status.last_update.is_some()
}

/// JSON output for heal operations
#[derive(Serialize)]
struct HealOperationOutput {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "clientToken")]
    client_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<HealStatusOutput>,
}

fn heal_stop_output(status: &HealStatus) -> (ExitCode, HealOperationOutput) {
    let success = !status.healing;
    let message = if success {
        HEAL_STOP_SUCCESS_MESSAGE
    } else {
        HEAL_STOP_STILL_RUNNING_MESSAGE
    };

    (
        if success {
            ExitCode::Success
        } else {
            ExitCode::GeneralError
        },
        HealOperationOutput {
            success,
            message: message.to_string(),
            client_token: None,
            status: has_heal_status_details(status).then(|| HealStatusOutput::from(status)),
        },
    )
}

enum HealStatusIndicator {
    Progress(&'static str),
    Date(&'static str),
}

fn heal_status_indicator(status: &HealStatus) -> HealStatusIndicator {
    match status.state {
        Some(HealRuntimeState::Disabled) => HealStatusIndicator::Date("Disabled"),
        Some(HealRuntimeState::Uninitialized) => HealStatusIndicator::Date("Uninitialized"),
        Some(HealRuntimeState::Active) => HealStatusIndicator::Progress("In Progress"),
        Some(HealRuntimeState::Idle) => HealStatusIndicator::Date("Idle"),
        Some(HealRuntimeState::Unknown) | None => match status.summary.as_deref() {
            Some("running") => HealStatusIndicator::Progress("In Progress"),
            Some("finished") => HealStatusIndicator::Progress("Finished"),
            Some("stopped") => HealStatusIndicator::Date("Stopped"),
            Some("notFound") => HealStatusIndicator::Date("Not Found"),
            _ if status.healing => HealStatusIndicator::Progress("In Progress"),
            _ => HealStatusIndicator::Date("Idle"),
        },
    }
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

    let task_request = match heal_task_request(
        args.bucket.clone(),
        args.prefix.clone(),
        args.client_token.clone(),
        formatter,
    ) {
        Ok(request) => request,
        Err(code) => return code,
    };

    let result = if let Some(request) = task_request {
        client.heal_task_status(request).await
    } else {
        client.heal_status().await
    };

    match result {
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
    let healing_status = match heal_status_indicator(status) {
        HealStatusIndicator::Progress(label) => formatter.style_size(label),
        HealStatusIndicator::Date(label) => formatter.style_date(label),
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

    if let Some(ref summary) = status.summary {
        formatter.println(&format!("  Summary:       {}", summary));
    }
    if let Some(ref detail) = status.detail {
        formatter.println(&format!("  Detail:        {}", detail));
    }

    if status.healing || status.summary.is_some() {
        if !status.bucket.is_empty() {
            formatter.println(&format!(
                "  Current:       {}/{}",
                status.bucket, status.object
            ));
        }

        if let Some(scan_mode) = status.scan_mode {
            formatter.println(&format!("  Scan Mode:     {scan_mode}"));
        }

        if status.scan_cycle > 0 {
            formatter.println(&format!("  Scan Cycle:    {}", status.scan_cycle));
        }

        formatter.println(&format!(
            "  Tasks:         {} queued, {} active",
            status.heal_queue_length, status.heal_active_tasks
        ));

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
        formatter.println("  No active heal operation.");
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
            let status_output =
                has_heal_status_details(&status).then(|| HealStatusOutput::from(&status));

            if formatter.is_json() {
                let output = HealOperationOutput {
                    success: true,
                    message: "Heal operation started successfully".to_string(),
                    client_token: (!status.heal_id.is_empty()).then(|| status.heal_id.clone()),
                    status: status_output,
                };
                formatter.json(&output);
            } else {
                if args.dry_run {
                    formatter.success("Heal operation started (DRY RUN mode).");
                } else {
                    formatter.success("Heal operation started successfully.");
                }
                if !status.heal_id.is_empty() {
                    formatter.println(&format!("  Client Token:  {}", status.heal_id));
                }
                if let Some(ref started) = status.started {
                    formatter.println(&format!("  Started:       {}", started));
                }
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

    let task_request = match heal_task_request(
        args.bucket.clone(),
        args.prefix.clone(),
        args.client_token.clone(),
        formatter,
    ) {
        Ok(request) => request,
        Err(code) => return code,
    };

    if let Some(request) = task_request {
        return match client.heal_task_stop(request).await {
            Ok(status) => {
                let (exit_code, mut output) = heal_stop_output(&status);
                output.client_token = (!status.heal_id.is_empty()).then(|| status.heal_id.clone());

                if formatter.is_json() {
                    formatter.json(&output);
                } else if output.success {
                    formatter.success(&format!("{}.", output.message));
                } else {
                    formatter.error(&format!("{}.", output.message));
                    formatter.println("");
                    print_heal_status(&status, formatter);
                }
                exit_code
            }
            Err(e) => {
                formatter.error(&format!("Failed to stop heal operation: {e}"));
                ExitCode::GeneralError
            }
        };
    }

    match client.heal_stop().await {
        Ok(()) => {
            let status = match client.heal_status().await {
                Ok(status) => status,
                Err(e) => {
                    formatter.error(&format!(
                        "Heal stop request was accepted, but failed to verify heal status: {e}"
                    ));
                    return ExitCode::GeneralError;
                }
            };
            let (exit_code, output) = heal_stop_output(&status);

            if formatter.is_json() {
                formatter.json(&output);
            } else if output.success {
                formatter.success(&format!("{}.", output.message));
            } else {
                formatter.error(&format!("{}.", output.message));
                formatter.println("");
                print_heal_status(&status, formatter);
            }
            exit_code
        }
        Err(e) => {
            formatter.error(&format!("Failed to stop heal operation: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn heal_task_request(
    bucket: Option<String>,
    prefix: Option<String>,
    client_token: Option<String>,
    formatter: &Formatter,
) -> Result<Option<HealTaskRequest>, ExitCode> {
    if prefix.as_deref().is_some_and(|prefix| !prefix.is_empty())
        && bucket.as_deref().is_none_or(|bucket| bucket.is_empty())
    {
        formatter.error("Heal task prefix requires --bucket.");
        return Err(ExitCode::UsageError);
    }

    let has_target = bucket.as_deref().is_some_and(|bucket| !bucket.is_empty());
    let has_token = client_token
        .as_deref()
        .is_some_and(|client_token| !client_token.is_empty());

    match (has_target, has_token) {
        (false, false) => Ok(None),
        (_, true) => Ok(Some(HealTaskRequest {
            bucket: bucket.unwrap_or_default(),
            prefix,
            client_token: client_token.expect("client token is present"),
        })),
        (true, false) => {
            formatter.error("Heal task request requires --client-token when --bucket is set.");
            Err(ExitCode::UsageError)
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
            scan_mode: Some(HealScanMode::Deep),
            scan_cycle: 42,
            heal_queue_length: 3,
            heal_active_tasks: 1,
            items_scanned: 1000,
            items_healed: 50,
            items_failed: 5,
            bytes_scanned: 1024 * 1024 * 100,
            bytes_healed: 1024 * 1024 * 5,
            started: Some("2024-01-01T10:00:00Z".to_string()),
            last_update: Some("2024-01-01T10:30:00Z".to_string()),
            ..Default::default()
        };

        let output = HealOperationOutput {
            success: true,
            message: "Heal operation started successfully".to_string(),
            client_token: Some("heal-123".to_string()),
            status: Some(HealStatusOutput::from(&status)),
        };

        let value = serde_json::to_value(&output).expect("serialize heal operation output");
        assert_eq!(value["clientToken"], "heal-123");
        let status_value = value
            .get("status")
            .expect("status field exists")
            .as_object()
            .expect("status is object");
        assert!(status_value.get("healId").is_some());
        assert!(status_value.get("scanMode").is_some());
        assert!(status_value.get("healQueueLength").is_some());
        assert!(status_value.get("healActiveTasks").is_some());
        assert!(status_value.get("itemsScanned").is_some());
    }

    #[test]
    fn test_heal_stop_output_reports_still_running_status() {
        let status = HealStatus {
            healing: true,
            heal_active_tasks: 1,
            started: Some("2026-06-15T10:11:18Z".to_string()),
            ..Default::default()
        };

        let (exit_code, output) = heal_stop_output(&status);

        assert_eq!(exit_code, ExitCode::GeneralError);
        assert!(!output.success);
        assert_eq!(output.message, HEAL_STOP_STILL_RUNNING_MESSAGE);
        assert!(output.status.is_some());
    }

    #[test]
    fn test_heal_stop_output_reports_success_for_idle_status() {
        let status = HealStatus::default();

        let (exit_code, output) = heal_stop_output(&status);

        assert_eq!(exit_code, ExitCode::Success);
        assert!(output.success);
        assert_eq!(output.message, HEAL_STOP_SUCCESS_MESSAGE);
        assert!(output.status.is_none());
    }

    #[test]
    fn test_heal_task_request_accepts_token_without_bucket() {
        let formatter = Formatter::default();

        let request = heal_task_request(None, None, Some("root-token".to_string()), &formatter)
            .expect("root token request should be valid")
            .expect("root token request should be task scoped");

        assert!(request.bucket.is_empty());
        assert!(request.prefix.is_none());
        assert_eq!(request.client_token, "root-token");
    }

    #[test]
    fn test_heal_task_request_rejects_bucket_without_token() {
        let formatter = Formatter::default();

        let result = heal_task_request(Some("logs".to_string()), None, None, &formatter);

        assert!(matches!(result, Err(ExitCode::UsageError)));
    }

    #[test]
    fn test_heal_task_request_rejects_prefix_without_bucket() {
        let formatter = Formatter::default();

        let result = heal_task_request(
            None,
            Some("2026/".to_string()),
            Some("root-token".to_string()),
            &formatter,
        );

        assert!(matches!(result, Err(ExitCode::UsageError)));
    }

    #[test]
    fn test_heal_status_output_from() {
        let status = HealStatus {
            heal_id: "heal-123".to_string(),
            healing: true,
            state: Some(HealRuntimeState::Active),
            bucket: "test-bucket".to_string(),
            object: "test/object.txt".to_string(),
            scan_mode: Some(HealScanMode::Deep),
            scan_cycle: 42,
            heal_queue_length: 3,
            heal_active_tasks: 1,
            items_scanned: 1000,
            items_healed: 50,
            items_failed: 5,
            bytes_scanned: 1024 * 1024 * 100,
            bytes_healed: 1024 * 1024 * 5,
            started: Some("2024-01-01T10:00:00Z".to_string()),
            last_update: Some("2024-01-01T10:30:00Z".to_string()),
            ..Default::default()
        };

        let output = HealStatusOutput::from(&status);
        assert_eq!(output.heal_id, "heal-123");
        assert!(output.healing);
        assert_eq!(output.state, Some(HealRuntimeState::Active));
        assert_eq!(output.bucket, "test-bucket");
        assert_eq!(output.scan_mode, Some(HealScanMode::Deep));
        assert_eq!(output.scan_cycle, 42);
        assert_eq!(output.heal_queue_length, 3);
        assert_eq!(output.heal_active_tasks, 1);
        assert_eq!(output.items_scanned, 1000);
        assert_eq!(output.items_healed, 50);
    }

    #[test]
    fn test_has_heal_status_details() {
        assert!(!has_heal_status_details(&HealStatus::default()));

        assert!(has_heal_status_details(&HealStatus {
            healing: true,
            ..Default::default()
        }));

        assert!(has_heal_status_details(&HealStatus {
            started: Some("2024-01-01T10:00:00Z".to_string()),
            ..Default::default()
        }));

        assert!(has_heal_status_details(&HealStatus {
            heal_active_tasks: 1,
            ..Default::default()
        }));

        assert!(has_heal_status_details(&HealStatus {
            state: Some(HealRuntimeState::Disabled),
            ..Default::default()
        }));
    }

    #[test]
    fn test_heal_status_indicator_reports_disabled_runtime() {
        let status = HealStatus {
            state: Some(HealRuntimeState::Disabled),
            ..Default::default()
        };

        assert!(matches!(
            heal_status_indicator(&status),
            HealStatusIndicator::Date("Disabled")
        ));
    }
}
