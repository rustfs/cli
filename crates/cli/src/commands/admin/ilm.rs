//! ILM administration commands.

use clap::{Args, Subcommand};
use rc_core::Error;
use rc_core::admin::{
    AdminApi, ManualTransitionJobResponse, ManualTransitionRunRequest, ManualTransitionRunResponse,
};
use serde::Serialize;
use tokio::time::{Duration, Instant, sleep};

use super::{emit_observability_error, get_admin_client};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const MAX_MANUAL_TRANSITION_OBJECTS: u64 = 100_000;
const MAX_MANUAL_TRANSITION_DURATION_SECONDS: u64 = 3600;

#[derive(Subcommand, Debug)]
pub enum IlmCommands {
    /// Manage lifecycle transition operations
    #[command(subcommand)]
    Transition(TransitionCommands),
}

#[derive(Subcommand, Debug)]
pub enum TransitionCommands {
    /// Run bounded lifecycle transition evaluation for existing objects
    Run(ManualTransitionRunArgs),

    /// Show durable lifecycle transition job status
    Status(ManualTransitionJobArgs),

    /// Request cancellation for a durable lifecycle transition job
    Cancel(ManualTransitionJobArgs),

    /// Wait until a durable lifecycle transition job reaches a terminal state
    Wait(ManualTransitionWaitArgs),
}

#[derive(Args, Debug)]
pub struct ManualTransitionRunArgs {
    /// Alias name of the server
    pub alias: String,

    /// Bucket to evaluate
    pub bucket: String,

    /// Limit evaluation to this object key prefix
    #[arg(long, default_value = "")]
    pub prefix: String,

    /// Limit evaluation to lifecycle transitions targeting this storage tier
    #[arg(long)]
    pub tier: Option<String>,

    /// Report eligible objects without enqueueing transition tasks
    #[arg(long)]
    pub dry_run: bool,

    /// Start a durable background job and return its job endpoints
    #[arg(long = "async")]
    pub async_mode: bool,

    /// Maximum number of object versions to scan
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=MAX_MANUAL_TRANSITION_OBJECTS))]
    pub max_objects: u64,

    /// Best-effort seconds budget checked between listed object versions
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_MANUAL_TRANSITION_DURATION_SECONDS))]
    pub max_duration_seconds: Option<u64>,
}

#[derive(Args, Debug)]
pub struct ManualTransitionJobArgs {
    /// Alias name of the server
    pub alias: String,

    /// Durable manual transition job ID returned by `transition run --async`
    pub job_id: String,
}

#[derive(Args, Debug)]
pub struct ManualTransitionWaitArgs {
    /// Alias name of the server
    pub alias: String,

    /// Durable manual transition job ID returned by `transition run --async`
    pub job_id: String,

    /// Seconds between status polls
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=60))]
    pub poll_interval_seconds: u64,

    /// Maximum seconds to wait before returning an error
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ManualTransitionRunSuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: &'a ManualTransitionRunResponse,
}

#[derive(Debug, Serialize)]
struct ManualTransitionJobSuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: &'a ManualTransitionJobResponse,
}

pub async fn execute(command: IlmCommands, formatter: &Formatter) -> ExitCode {
    match command {
        IlmCommands::Transition(TransitionCommands::Run(args)) => {
            execute_manual_transition_run(args, formatter).await
        }
        IlmCommands::Transition(TransitionCommands::Status(args)) => {
            execute_manual_transition_status(args, formatter).await
        }
        IlmCommands::Transition(TransitionCommands::Cancel(args)) => {
            execute_manual_transition_cancel(args, formatter).await
        }
        IlmCommands::Transition(TransitionCommands::Wait(args)) => {
            execute_manual_transition_wait(args, formatter).await
        }
    }
}

async fn execute_manual_transition_run(
    args: ManualTransitionRunArgs,
    formatter: &Formatter,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let request = ManualTransitionRunRequest {
        bucket: args.bucket,
        prefix: args.prefix,
        tier: args.tier,
        dry_run: args.dry_run,
        max_objects: args.max_objects,
        max_duration_seconds: args.max_duration_seconds,
    };

    let result = if args.async_mode {
        client.run_manual_transition_async(request).await
    } else {
        client.run_manual_transition(request).await
    };

    match result {
        Ok(response) => {
            if formatter.is_json() {
                formatter.json(&ManualTransitionRunSuccessOutput {
                    schema_version: 3,
                    output_type: "manual_transition_run",
                    status: "success",
                    data: &response,
                });
            } else {
                print_manual_transition_run(&response, formatter);
            }
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "manual_transition_run",
            "admin.ilm-transition-run",
            "Failed to run manual transition",
            &error,
            formatter,
        ),
    }
}

async fn execute_manual_transition_status(
    args: ManualTransitionJobArgs,
    formatter: &Formatter,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.manual_transition_job_status(&args.job_id).await {
        Ok(response) => {
            print_manual_transition_job("manual_transition_job_status", &response, formatter);
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "manual_transition_job_status",
            "admin.ilm-transition-job",
            "Failed to get manual transition job status",
            &error,
            formatter,
        ),
    }
}

async fn execute_manual_transition_cancel(
    args: ManualTransitionJobArgs,
    formatter: &Formatter,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.cancel_manual_transition_job(&args.job_id).await {
        Ok(response) => {
            print_manual_transition_job("manual_transition_job_cancel", &response, formatter);
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "manual_transition_job_cancel",
            "admin.ilm-transition-job",
            "Failed to cancel manual transition job",
            &error,
            formatter,
        ),
    }
}

async fn execute_manual_transition_wait(
    args: ManualTransitionWaitArgs,
    formatter: &Formatter,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let deadline = args
        .timeout_seconds
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let poll_interval = Duration::from_secs(args.poll_interval_seconds);

    loop {
        match client.manual_transition_job_status(&args.job_id).await {
            Ok(response) if is_terminal_job_status(&response.status) => {
                print_manual_transition_job("manual_transition_job_wait", &response, formatter);
                return wait_terminal_exit_code(&response.status);
            }
            Ok(_) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                let error = Error::General(format!(
                    "Timed out waiting for manual transition job '{}' to finish",
                    args.job_id
                ));
                return emit_observability_error(
                    "manual_transition_job_wait",
                    "admin.ilm-transition-job",
                    "Manual transition job wait timed out",
                    &error,
                    formatter,
                );
            }
            Ok(_) => sleep(poll_interval).await,
            Err(error) => {
                return emit_observability_error(
                    "manual_transition_job_wait",
                    "admin.ilm-transition-job",
                    "Failed while waiting for manual transition job",
                    &error,
                    formatter,
                );
            }
        }
    }
}

fn print_manual_transition_run(response: &ManualTransitionRunResponse, formatter: &Formatter) {
    let report = &response.report;
    formatter.println(&formatter.style_name("Manual Transition Run"));
    formatter.println("");
    formatter.println(&format!(
        "State:          {}",
        formatter.sanitize_text(&response.state)
    ));
    formatter.println(&format!(
        "Mode:           {}",
        formatter.sanitize_text(&response.mode)
    ));
    if let Some(job_id) = &response.job_id {
        formatter.println(&format!(
            "Job ID:         {}",
            formatter.sanitize_text(job_id)
        ));
    }
    if let Some(status_endpoint) = &response.status_endpoint {
        formatter.println(&format!(
            "Status:         {}",
            formatter.sanitize_text(status_endpoint)
        ));
    }
    if let Some(cancel_endpoint) = &response.cancel_endpoint {
        formatter.println(&format!(
            "Cancel:         {}",
            formatter.sanitize_text(cancel_endpoint)
        ));
    }
    formatter.println(&format!(
        "Bucket:         {}",
        formatter.sanitize_text(&report.bucket)
    ));
    formatter.println(&format!(
        "Prefix:         {}",
        formatter.sanitize_text(value_or_all(&report.prefix))
    ));
    formatter.println(&format!(
        "Tier:           {}",
        formatter.sanitize_text(report.tier.as_deref().map(value_or_all).unwrap_or("all"))
    ));
    formatter.println(&format!("Dry run:        {}", report.dry_run));
    formatter.println(&format!(
        "Lifecycle:      {}",
        report.lifecycle_config_found
    ));
    formatter.println(&format!("Scanned:        {}", report.scanned));
    formatter.println(&format!("Eligible:       {}", report.eligible));
    formatter.println(&format!("Enqueued:       {}", report.enqueued));
    formatter.println(&format!("Dry-run count:  {}", report.dry_run_eligible));
    formatter.println(&format!(
        "Not due:        {}",
        report.skipped_not_transition
    ));
    formatter.println(&format!(
        "Already moved:  {}",
        report.skipped_already_transitioned
    ));
    formatter.println(&format!(
        "Queue pressure: {}",
        report.skipped_queue_full + report.skipped_queue_closed + report.skipped_queue_timeout
    ));
    formatter.println(&format!("Limit reached:  {}", report.truncated_by_limit));
    formatter.println(&format!("Duration hit:   {}", report.truncated_by_duration));
    if let Some(continuation_token) = &report.continuation_token {
        formatter.println(&format!(
            "Continuation:   {}",
            formatter.sanitize_text(continuation_token)
        ));
    }
}

fn value_or_all(value: &str) -> &str {
    if value.is_empty() { "all" } else { value }
}

fn print_manual_transition_job(
    output_type: &'static str,
    response: &ManualTransitionJobResponse,
    formatter: &Formatter,
) {
    if formatter.is_json() {
        formatter.json(&ManualTransitionJobSuccessOutput {
            schema_version: 3,
            output_type,
            status: "success",
            data: response,
        });
        return;
    }

    let report = &response.report;
    let queue = &response.queue_snapshot;
    formatter.println(&formatter.style_name("Manual Transition Job"));
    formatter.println("");
    formatter.println(&format!(
        "Status:         {}",
        formatter.sanitize_text(&response.status)
    ));
    formatter.println(&format!(
        "Mode:           {}",
        formatter.sanitize_text(&response.mode)
    ));
    formatter.println(&format!(
        "Job ID:         {}",
        formatter.sanitize_text(&response.job_id)
    ));
    formatter.println(&format!(
        "Bucket:         {}",
        formatter.sanitize_text(&response.bucket)
    ));
    formatter.println(&format!(
        "Prefix:         {}",
        formatter.sanitize_text(value_or_all(&response.prefix))
    ));
    formatter.println(&format!(
        "Tier:           {}",
        formatter.sanitize_text(response.tier.as_deref().map(value_or_all).unwrap_or("all"))
    ));
    formatter.println(&format!("Dry run:        {}", response.dry_run));
    formatter.println(&format!("Cancel asked:   {}", response.cancel_requested));
    formatter.println(&format!(
        "Created ns:     {}",
        response.created_at_unix_nanos
    ));
    formatter.println(&format!(
        "Updated ns:     {}",
        response.updated_at_unix_nanos
    ));
    if let Some(completed_at) = response.completed_at_unix_nanos {
        formatter.println(&format!("Completed ns:   {completed_at}"));
    }
    if let Some(reason) = &response.failure_reason {
        formatter.println(&format!(
            "Failure:        {}",
            formatter.sanitize_text(reason)
        ));
    }
    formatter.println("");
    formatter.println(&formatter.style_name("Report"));
    formatter.println(&format!("Scanned:        {}", report.scanned));
    formatter.println(&format!("Eligible:       {}", report.eligible));
    formatter.println(&format!("Enqueued:       {}", report.enqueued));
    formatter.println(&format!("Completed:      {}", report.transition_completed));
    formatter.println(&format!("Failed:         {}", report.transition_failed));
    formatter.println(&format!(
        "Already moved:  {}",
        report.skipped_already_transitioned
    ));
    formatter.println(&format!("Cancelled:      {}", report.cancelled));
    formatter.println(&format!("Limit reached:  {}", report.truncated_by_limit));
    formatter.println(&format!("Duration hit:   {}", report.truncated_by_duration));
    if let Some(continuation_token) = &report.continuation_token {
        formatter.println(&format!(
            "Continuation:   {}",
            formatter.sanitize_text(continuation_token)
        ));
    }
    formatter.println("");
    formatter.println(&formatter.style_name("Queue"));
    formatter.println(&format!("Queued:         {}", queue.queued));
    formatter.println(&format!("Active:         {}", queue.active));
    formatter.println(&format!("Workers:        {}", queue.workers));
    formatter.println(&format!("Capacity:       {}", queue.queue_capacity));
}

fn is_terminal_job_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "partial" | "failed" | "cancelled" | "unknown"
    )
}

fn wait_terminal_exit_code(status: &str) -> ExitCode {
    if matches!(status, "completed" | "partial") {
        ExitCode::Success
    } else {
        ExitCode::GeneralError
    }
}
