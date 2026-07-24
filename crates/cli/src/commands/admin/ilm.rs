//! ILM administration commands.

use clap::{Args, Subcommand};
use rc_core::admin::{AdminApi, ManualTransitionRunRequest, ManualTransitionRunResponse};
use serde::Serialize;

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

    /// Maximum number of object versions to scan
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=MAX_MANUAL_TRANSITION_OBJECTS))]
    pub max_objects: u64,

    /// Best-effort seconds budget checked between listed object versions
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_MANUAL_TRANSITION_DURATION_SECONDS))]
    pub max_duration_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ManualTransitionRunSuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: &'a ManualTransitionRunResponse,
}

pub async fn execute(command: IlmCommands, formatter: &Formatter) -> ExitCode {
    match command {
        IlmCommands::Transition(TransitionCommands::Run(args)) => {
            execute_manual_transition_run(args, formatter).await
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

    match client.run_manual_transition(request).await {
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
}

fn value_or_all(value: &str) -> &str {
    if value.is_empty() { "all" } else { value }
}
