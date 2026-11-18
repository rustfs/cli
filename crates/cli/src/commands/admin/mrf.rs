//! Node-bound responsibility observations; none of these actions verifies object content.

use clap::{Args, Subcommand};
use rc_core::admin::recovery::*;
use rc_core::{Error, Result};
use std::collections::HashSet;

use super::{get_admin_client, recovery};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
pub enum MrfCommands {
    /// Observe one bounded page of node summaries (idle never means data verified)
    Status(PageArgs),
    /// Observe one page of retained responsibilities; does not scan all pages
    List(ListArgs),
    /// Read one responsibility, including Active entries omitted by the list
    Get(TargetArgs),
    /// Refresh bucket identity without reading object payloads
    Refresh(RefreshArgs),
    /// Submit an incarnation-fenced recheck; query the same ID for its later state
    Recheck(RecheckArgs),
    /// Accept unverified risk with an audit record; retain the responsibility
    AcceptRisk(AcceptArgs),
}

#[derive(Args, Debug)]
pub struct PageArgs {
    pub alias: String,
    /// Opaque continuation from the same observation endpoint
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[command(flatten)]
    pub page: PageArgs,
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=256))]
    pub limit: u16,
}

#[derive(Args, Debug)]
pub struct TargetArgs {
    pub alias: String,
    /// Node ID returned by an MRF observation; never an arbitrary URL
    #[arg(long)]
    pub node_id: String,
    #[arg(long)]
    pub responsibility_id: String,
}

#[derive(Args, Debug)]
pub struct RefreshArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub expected_bucket_incarnation: Option<String>,
}

#[derive(Args, Debug)]
pub struct RecheckArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long)]
    pub expected_bucket_incarnation: String,
}

#[derive(Args, Debug)]
pub struct AcceptArgs {
    #[command(flatten)]
    pub target: RecheckArgs,
    /// Stable UUID for reconciliation; reuse it only with the identical request
    #[arg(long)]
    pub request_id: String,
    #[arg(long)]
    pub reason: String,
    #[arg(long)]
    pub reference: String,
    #[arg(long, required = true)]
    pub acknowledge_unverified_data_risk: bool,
    #[arg(long)]
    pub acknowledge_unknown_source_incarnation: bool,
    #[arg(long)]
    pub acknowledge_bucket_incarnation_mismatch: bool,
}

impl MrfCommands {
    fn alias(&self) -> &str {
        match self {
            Self::Status(args) => &args.alias,
            Self::List(args) => &args.page.alias,
            Self::Get(args) => &args.alias,
            Self::Refresh(args) => &args.target.alias,
            Self::Recheck(args) => &args.target.alias,
            Self::AcceptRisk(args) => &args.target.target.alias,
        }
    }

    fn action(&self) -> Result<Option<MrfAction>> {
        let (target, incarnation, action) = match self {
            Self::Refresh(args) => (
                &args.target,
                args.expected_bucket_incarnation.as_ref(),
                "refresh",
            ),
            Self::Recheck(args) => (
                &args.target,
                Some(&args.expected_bucket_incarnation),
                "recheck",
            ),
            Self::AcceptRisk(args) => (
                &args.target.target,
                Some(&args.target.expected_bucket_incarnation),
                "acceptUnverifiedRisk",
            ),
            _ => return Ok(None),
        };
        validate_recovery_uuid(&target.responsibility_id)?;
        if let Some(incarnation) = incarnation {
            validate_recovery_uuid(incarnation)?;
        }
        let mut request = MrfActionRequest {
            action: action.into(),
            responsibility_id: target.responsibility_id.clone(),
            expected_bucket_incarnation_id: incarnation.cloned(),
            acknowledge_unverified_data_risk: None,
            acknowledge_unknown_source_incarnation: None,
            acknowledge_bucket_incarnation_mismatch: None,
            reason: None,
            reference: None,
            request_id: None,
        };
        if let Self::AcceptRisk(args) = self {
            validate_recovery_uuid(&args.request_id)?;
            if !args.acknowledge_unverified_data_risk
                || args.reason.trim().is_empty()
                || args.reason.len() > 1024
                || args.reference.trim().is_empty()
                || args.reference.len() > 256
            {
                return Err(Error::InvalidPath("risk acknowledgment, reason (1..1024 bytes), and reference (1..256 bytes) are required".into()));
            }
            request.acknowledge_unverified_data_risk = Some(true);
            request.acknowledge_unknown_source_incarnation =
                Some(args.acknowledge_unknown_source_incarnation);
            request.acknowledge_bucket_incarnation_mismatch =
                Some(args.acknowledge_bucket_incarnation_mismatch);
            request.reason = Some(args.reason.clone());
            request.reference = Some(args.reference.clone());
            request.request_id = Some(args.request_id.clone());
        }
        Ok(Some(MrfAction {
            node_id: target.node_id.clone(),
            request,
        }))
    }
}

pub async fn execute(command: MrfCommands, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(command.alias(), formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_with_api(&command, &client, formatter).await
}

async fn execute_with_api(
    command: &MrfCommands,
    api: &dyn RecoveryApi,
    formatter: &Formatter,
) -> ExitCode {
    let action = match command.action() {
        Ok(action) => action,
        Err(error) => return recovery::error("mrf_observation", &error, false, formatter),
    };
    let result = if let Some(action) = &action {
        api.mrf_action(action).await
    } else {
        match command {
            MrfCommands::Status(args) => api.mrf_summary(args.cursor.as_deref()).await,
            MrfCommands::List(args) => api.mrf_list(args.page.cursor.as_deref(), args.limit).await,
            MrfCommands::Get(args) => api.mrf_get(&args.node_id, &args.responsibility_id).await,
            _ => unreachable!("actions were handled above"),
        }
    };
    match result {
        Ok(observation) => {
            let (code, status, message) = outcome(&observation, action.as_ref());
            recovery::output("mrf_observation", status, message, &observation, formatter);
            if !formatter.is_json() {
                print_observation(&observation, formatter);
            }
            code
        }
        Err(error) => {
            let unknown = action.is_some()
                && !matches!(
                    error,
                    Error::InvalidPath(_) | Error::Auth(_) | Error::UnsupportedFeature(_)
                );
            if unknown {
                recovery::error(
                    "mrf_observation",
                    &Error::General(format!(
                        "Action outcome is unknown: {error}. Query the same node and responsibility; reconcile risk acceptance using the same request ID and identical fields."
                    )),
                    true,
                    formatter,
                )
            } else {
                recovery::error("mrf_observation", &error, false, formatter)
            }
        }
    }
}

fn outcome(
    observation: &MrfObservation,
    action: Option<&MrfAction>,
) -> (ExitCode, &'static str, &'static str) {
    if observation.contract_version != 1 || observation.nodes.is_empty() {
        return (
            ExitCode::GeneralError,
            "unknown",
            "Unsupported or empty observation; data remains unverified.",
        );
    }
    if observation
        .nodes
        .iter()
        .all(|node| node.state == "unsupported")
    {
        return (
            ExitCode::UnsupportedFeature,
            "unsupported",
            "Selected nodes do not support MRF management.",
        );
    }
    if !observation.page_complete
        || observation.nodes.iter().any(|node| {
            node.state != "ok"
                || node.reply.is_none()
                || matches!(node.reply, Some(MrfReply::Unknown | MrfReply::Error { .. }))
        })
    {
        return (
            ExitCode::GeneralError,
            if action.is_some() {
                "unknown"
            } else {
                "partial"
            },
            "Observation is incomplete or the action was not confirmed. Inspect node outcomes before retrying.",
        );
    }
    if let Some(request) = action {
        let expected = match request.request.action.as_str() {
            "refresh" => "refreshed",
            "recheck" => "active",
            _ => "operatorAcceptedUnverified",
        };
        let confirmed = observation.nodes.len() == 1
            && observation.nodes[0].node_id == request.node_id
            && matches!(
                &observation.nodes[0].reply, Some(MrfReply::Action { action }) if action.responsibility_id.eq_ignore_ascii_case(&request.request.responsibility_id)
                    && action.state == expected && !action.data_verified && action.durable_responsibility_retained
            );
        return if confirmed {
            (
                ExitCode::Success,
                "accepted",
                "Action accepted; the responsibility is retained and content is not verified. A recheck is only submitted, not completed.",
            )
        } else {
            (
                ExitCode::GeneralError,
                "unknown",
                "Action receipt did not match the request; query the same responsibility before reconciling.",
            )
        };
    }
    if observation.nodes.iter().any(|node| matches!(&node.reply, Some(MrfReply::Get { observation }) if observation.responsibility.is_none())) {
        return (ExitCode::GeneralError, "not_observed", "Responsibility not observed in this runtime. Its final outcome and content integrity are not confirmed.");
    }
    if observation.next_cursor.is_some() {
        return (
            ExitCode::GeneralError,
            "partial",
            "Bounded page observed; more pages remain. Continue explicitly with nextCursor. This is not a frozen snapshot or a complete export.",
        );
    }
    (
        ExitCode::Success,
        "observed",
        "Requested page observed. Idle, absence, and risk acceptance never establish data integrity.",
    )
}

fn print_observation(observation: &MrfObservation, formatter: &Formatter) {
    formatter.println(&format!(
        "Nodes in this response: {}/{}; observed at {}..{} ms",
        observation.nodes.len(),
        observation.expected_nodes,
        observation.observation_started_at_ms,
        observation.observation_finished_at_ms
    ));
    let mut known_slots = HashSet::new();
    let mut unknown = 0;
    for node in &observation.nodes {
        formatter.println(&format!(
            "\nNode {} ({}) — {}",
            formatter.sanitize_text(&node.node_id),
            formatter.sanitize_text(&node.host),
            formatter.sanitize_text(&node.state)
        ));
        match &node.reply {
            Some(MrfReply::Summary { summary }) => {
                formatter.println(
                    "ACTIVE  HELD  UNKNOWN SOURCE  CHANGED BUCKET  ACCEPTED UNVERIFIED  QUEUED",
                );
                formatter.println(&format!(
                    "{}  {}  {}  {}  {}  {}",
                    summary.active_responsibility_count,
                    summary.held_count,
                    summary.legacy_generation_unknown_count,
                    summary.bucket_incarnation_changed_count,
                    summary.operator_accepted_count,
                    summary.best_effort_queue_depth
                ));
                formatter.println(&format!(
                    "Sampled at: {} ms; checkpoint readable: {}",
                    summary.sampled_at_ms, summary.checkpoint_readable
                ));
            }
            Some(MrfReply::List { snapshot, .. }) => {
                formatter.println("RESPONSIBILITY ID  STATUS  BUCKET / OBJECT  VERSION SLOT");
                for row in &snapshot.responsibilities {
                    print_row(row, formatter);
                    if let Some(source) = &row.source_bucket_incarnation_id {
                        known_slots.insert((
                            source.clone(),
                            row.bucket.clone(),
                            row.object.clone(),
                            row.version_id.clone(),
                        ));
                    } else {
                        unknown += 1;
                    }
                }
            }
            Some(MrfReply::Get { observation }) => {
                if let Some(row) = &observation.responsibility {
                    print_row(row, formatter);
                }
            }
            Some(MrfReply::Error { code }) => {
                formatter.println(&format!("Reason: {}", formatter.sanitize_text(code)))
            }
            Some(MrfReply::Action { action }) => formatter.println(&format!(
                "State: {}",
                formatter.sanitize_text(&action.state)
            )),
            _ => {}
        }
    }
    if !known_slots.is_empty() || unknown > 0 {
        formatter.println(&format!("Observed known-source object/version slots in this page: {}; unknown-source responsibilities (not grouped): {unknown}", known_slots.len()));
    }
    if let Some(cursor) = &observation.next_cursor {
        formatter.println(&format!("nextCursor: {}", formatter.sanitize_text(cursor)));
    }
}

fn print_row(row: &MrfResponsibility, formatter: &Formatter) {
    formatter.println(&format!(
        "{}  {}  {} / {}  {}",
        formatter.sanitize_text(&row.responsibility_id),
        formatter.sanitize_text(&row.status),
        formatter.sanitize_text(&row.bucket),
        formatter.sanitize_text(&row.object),
        formatter.sanitize_text(
            row.version_id
                .as_deref()
                .unwrap_or("null slot (not current version)")
        )
    ));
    formatter.println(&format!(
        "  Source incarnation: {}; observed incarnation: {}",
        formatter.sanitize_text(
            row.source_bucket_incarnation_id
                .as_deref()
                .unwrap_or("unknown")
        ),
        formatter.sanitize_text(row.bucket_incarnation_id.as_deref().unwrap_or("unknown"))
    ));
}

#[cfg(test)]
mod tests;
