//! Rebalance commands for post-expansion data movement.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, RebalanceCleanupWarnings, RebalancePoolStatus, RebalanceStatus};

/// Rebalance subcommands
#[derive(Subcommand, Debug)]
pub enum RebalanceCommands {
    /// Start a rebalance operation
    Start(StartArgs),

    /// Show rebalance status
    Status(StatusArgs),

    /// Stop a running rebalance operation
    Stop(StopArgs),
}

#[derive(clap::Args, Debug)]
pub struct StartArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct StopArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RebalanceOperationOutput {
    success: bool,
    message: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

/// Execute a rebalance subcommand
pub async fn execute(cmd: RebalanceCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        RebalanceCommands::Start(args) => execute_start(args, formatter).await,
        RebalanceCommands::Status(args) => execute_status(args, formatter).await,
        RebalanceCommands::Stop(args) => execute_stop(args, formatter).await,
    }
}

async fn execute_start(args: StartArgs, formatter: &Formatter) -> ExitCode {
    execute_start_with_messages(
        args,
        formatter,
        "Rebalance started successfully",
        "Failed to start rebalance",
    )
    .await
}

pub(super) async fn execute_start_with_messages(
    args: StartArgs,
    formatter: &Formatter,
    success_message: &str,
    failure_prefix: &str,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.rebalance_start().await {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&RebalanceOperationOutput {
                    success: true,
                    message: success_message.to_string(),
                    target: args.alias,
                    id: Some(result.id),
                });
            } else {
                formatter.success(&format!("{success_message}."));
                if !result.id.is_empty() {
                    formatter.println(&format!("  ID: {}", result.id));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("{failure_prefix}: {e}"));
            ExitCode::GeneralError
        }
    }
}

pub(super) async fn execute_status(args: StatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.rebalance_status().await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&status);
            } else {
                print_rebalance_status(&status, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get rebalance status: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_stop(args: StopArgs, formatter: &Formatter) -> ExitCode {
    execute_stop_with_messages(
        args,
        formatter,
        "Rebalance stopped successfully",
        "Failed to stop rebalance",
    )
    .await
}

pub(super) async fn execute_stop_with_messages(
    args: StopArgs,
    formatter: &Formatter,
    success_message: &str,
    failure_prefix: &str,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.rebalance_stop().await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&RebalanceOperationOutput {
                    success: true,
                    message: success_message.to_string(),
                    target: args.alias,
                    id: None,
                });
            } else {
                formatter.success(&format!("{success_message}."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("{failure_prefix}: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn print_rebalance_status(status: &RebalanceStatus, formatter: &Formatter) {
    formatter.println(&format!(
        "{} {}",
        formatter.style_name("Rebalance:"),
        if status.id.is_empty() {
            formatter.style_date("not started")
        } else {
            formatter.style_size("configured")
        }
    ));

    if !status.id.is_empty() {
        formatter.println(&format!("  ID: {}", status.id));
    }
    if let Some(stopped_at) = &status.stopped_at {
        formatter.println(&format!("  Stopped: {}", stopped_at));
    }

    if status.pools.is_empty() {
        formatter.println("  No pool status found.");
        return;
    }

    formatter.println("");
    formatter.println(&formatter.style_name("Per-pool usage:"));
    for pool in &status.pools {
        print_rebalance_pool(pool, formatter);
    }
}

fn print_rebalance_pool(pool: &RebalancePoolStatus, formatter: &Formatter) {
    let status = if pool.status.is_empty() {
        "idle"
    } else {
        pool.status.as_str()
    };

    formatter.println(&format!(
        "  Pool {}: {:.2}% used ({})",
        pool.id,
        pool.used * 100.0,
        style_status(status, formatter)
    ));

    if let Some(error) = &pool.last_error
        && !error.is_empty()
    {
        formatter.println(&format!("    Last error: {error}"));
    }

    if pool.cleanup_warnings.count > 0 {
        formatter.println(&format!(
            "    {} {}",
            formatter.theme().warning.apply_to("Cleanup warnings:"),
            format_cleanup_warnings(&pool.cleanup_warnings)
        ));
    }

    if let Some(progress) = &pool.progress {
        formatter.println(&format!(
            "    Progress:   {} moved, {} objects, {} versions",
            format_bytes(progress.bytes),
            progress.num_objects,
            progress.num_versions
        ));
        formatter.println(&format!(
            "    Remaining:  {} buckets, ETA {}",
            progress.remaining_buckets,
            format_duration(progress.eta)
        ));
        if !progress.bucket.is_empty() {
            formatter.println(&format!(
                "    Current:    {}/{}",
                progress.bucket, progress.object
            ));
        }
        formatter.println(&format!(
            "    Elapsed:    {}",
            format_duration(progress.elapsed)
        ));
    }
}

fn format_cleanup_warnings(warnings: &RebalanceCleanupWarnings) -> String {
    let mut details = vec![format!("{} warning(s)", warnings.count)];

    if let Some(message) = warnings
        .last_message
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        details.push(format!("last message: {message}"));
    }
    if let Some(bucket) = warnings
        .last_bucket
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        details.push(format!("bucket: {bucket}"));
    }
    if let Some(object) = warnings
        .last_object
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        details.push(format!("object: {object}"));
    }
    if let Some(at) = warnings
        .last_at
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        details.push(format!("at: {at}"));
    }

    details.join(", ")
}

fn style_status(status: &str, formatter: &Formatter) -> String {
    match status {
        "Started" | "running" => formatter.style_name(status),
        "Stopped" | "stopped" => formatter.theme().warning.apply_to(status).to_string(),
        "Completed" | "complete" => formatter.style_size(status),
        "idle" => formatter.style_date(status),
        _ => status.to_string(),
    }
}

fn format_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {secs}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
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
    use crate::output::OutputConfig;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(60), "1m 0s");
        assert_eq!(format_duration(65), "1m 5s");
        assert_eq!(format_duration(3600), "1h 0m 0s");
        assert_eq!(format_duration(3661), "1h 1m 1s");
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

    #[test]
    fn test_style_status_variants_without_color() {
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..Default::default()
        });

        assert_eq!(style_status("Started", &formatter), "Started");
        assert_eq!(style_status("running", &formatter), "running");
        assert_eq!(style_status("Stopped", &formatter), "Stopped");
        assert_eq!(style_status("stopped", &formatter), "stopped");
        assert_eq!(style_status("Completed", &formatter), "Completed");
        assert_eq!(style_status("complete", &formatter), "complete");
        assert_eq!(style_status("idle", &formatter), "idle");
        assert_eq!(style_status("Queued", &formatter), "Queued");
    }

    #[test]
    fn test_format_cleanup_warnings() {
        let warnings = RebalanceCleanupWarnings {
            count: 2,
            last_message: Some("cleanup warning".to_string()),
            last_bucket: Some("bucket-a".to_string()),
            last_object: Some("object-a".to_string()),
            last_at: Some("2026-06-12T00:00:00Z".to_string()),
        };

        assert_eq!(
            format_cleanup_warnings(&warnings),
            "2 warning(s), last message: cleanup warning, bucket: bucket-a, object: object-a, at: 2026-06-12T00:00:00Z"
        );
    }
}
