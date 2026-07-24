//! Policy management commands
//!
//! Commands for managing IAM policies and inspecting their entity mappings.

use clap::Subcommand;
use serde::Serialize;
use std::fs;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::Error;
use rc_core::admin::{
    AdminApi, CapabilityApi, CapabilityAvailability, GroupPolicyEntities,
    IAM_POLICY_DETACH_CAPABILITY, IAM_POLICY_ENTITIES_CAPABILITY, IamMutationApi, IamReadApi,
    PolicyDetachEntity, PolicyDetachRequest, PolicyDetachResult, PolicyEntitiesQuery,
    PolicyEntitiesResult, PolicyEntity, UserPolicyEntities,
};

/// Policy management subcommands
#[derive(Subcommand, Debug)]
pub enum PolicyCommands {
    /// List all policies
    #[command(name = "ls", alias = "list")]
    List(ListArgs),

    /// Create a new policy
    Create(CreateArgs),

    /// Get policy information
    Info(InfoArgs),

    /// Remove a policy
    #[command(name = "rm", alias = "remove")]
    Remove(RemoveArgs),

    /// Attach policy to a user or group
    Attach(AttachArgs),

    /// Detach one or more policies from exactly one user or group
    Detach(DetachArgs),

    /// Inspect policy mappings for users, groups, or policies
    Entities(EntitiesArgs),
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct CreateArgs {
    /// Alias name of the server
    pub alias: String,

    /// Policy name
    pub name: String,

    /// Path to policy JSON file
    pub policy_file: String,
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,

    /// Policy name
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Alias name of the server
    pub alias: String,

    /// Policy name to remove
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct AttachArgs {
    /// Alias name of the server
    pub alias: String,

    /// Policy name(s) to attach (comma-separated for multiple)
    pub policies: String,

    /// Target user access key
    #[arg(long, conflicts_with = "group")]
    pub user: Option<String>,

    /// Target group name
    #[arg(long, conflicts_with = "user")]
    pub group: Option<String>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct DetachArgs {
    /// Alias name of the server
    pub alias: String,

    /// Policy name to detach; accepts repeated values or comma-separated names
    #[arg(required = true, num_args = 1.., value_delimiter = ',')]
    pub policies: Vec<String>,

    /// Target user access key
    #[arg(long, required_unless_present = "group", conflicts_with = "group")]
    pub user: Option<String>,

    /// Target group name
    #[arg(long, required_unless_present = "user", conflicts_with = "user")]
    pub group: Option<String>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct EntitiesArgs {
    /// Alias name of the server
    pub alias: String,

    /// User access key to inspect; may be repeated
    #[arg(long)]
    pub user: Vec<String>,

    /// Group name to inspect; may be repeated
    #[arg(long)]
    pub group: Vec<String>,

    /// Policy name to inspect; may be repeated
    #[arg(long)]
    pub policy: Vec<String>,
}

/// JSON output for policy list
#[derive(Serialize)]
struct PolicyListOutput {
    policies: Vec<String>,
}

/// JSON output for policy info
#[derive(Serialize)]
struct PolicyInfoOutput {
    name: String,
    policy: serde_json::Value,
}

/// JSON output for policy operations
#[derive(Serialize)]
struct PolicyOperationOutput {
    success: bool,
    name: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct PolicyEntitiesSuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: PolicyEntitiesOutput<'a>,
}

#[derive(Debug, Serialize)]
struct PolicyEntitiesOutput<'a> {
    timestamp: jiff::Timestamp,
    user_mappings: Vec<UserPolicyEntitiesOutput<'a>>,
    group_mappings: Vec<GroupPolicyEntitiesOutput<'a>>,
    policy_mappings: Vec<PolicyEntitiesOutputRow<'a>>,
}

#[derive(Debug, Serialize)]
struct UserPolicyEntitiesOutput<'a> {
    user: &'a str,
    policies: &'a [String],
    member_of_mappings: Vec<GroupPolicyEntitiesOutput<'a>>,
}

#[derive(Debug, Serialize)]
struct GroupPolicyEntitiesOutput<'a> {
    group: &'a str,
    policies: &'a [String],
}

#[derive(Debug, Serialize)]
struct PolicyEntitiesOutputRow<'a> {
    policy: &'a str,
    users: &'a [String],
    groups: &'a [String],
}

#[derive(Debug, Serialize)]
struct PolicyEntitiesErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: PolicyEntitiesErrorBody,
}

#[derive(Debug, Serialize)]
struct PolicyEntitiesErrorBody {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<&'static str>,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct PolicyDetachSuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: PolicyDetachOutput<'a>,
}

#[derive(Debug, Serialize)]
struct PolicyDetachOutput<'a> {
    operation: &'static str,
    entity: PolicyDetachEntityOutput<'a>,
    changed: bool,
    attached: &'a [String],
    detached: &'a [String],
    unchanged: &'a [String],
    updated_at: jiff::Timestamp,
}

#[derive(Debug, Serialize)]
struct PolicyDetachEntityOutput<'a> {
    #[serde(rename = "type")]
    entity_type: PolicyDetachEntity,
    name: &'a str,
}

#[derive(Debug, Serialize)]
struct PolicyDetachErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: PolicyDetachErrorBody,
}

#[derive(Debug, Serialize)]
struct PolicyDetachErrorBody {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<&'static str>,
    suggestion: Option<&'static str>,
}

/// Execute a policy subcommand
pub async fn execute(cmd: PolicyCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        PolicyCommands::List(args) => execute_list(args, formatter).await,
        PolicyCommands::Create(args) => execute_create(args, formatter).await,
        PolicyCommands::Info(args) => execute_info(args, formatter).await,
        PolicyCommands::Remove(args) => execute_remove(args, formatter).await,
        PolicyCommands::Attach(args) => execute_attach(args, formatter).await,
        PolicyCommands::Detach(args) => execute_detach(args, formatter).await,
        PolicyCommands::Entities(args) => execute_entities(args, formatter).await,
    }
}

async fn execute_detach(args: DetachArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_detach_with_api(args, &client, &client, formatter).await
}

async fn execute_detach_with_api(
    args: DetachArgs,
    capabilities: &dyn CapabilityApi,
    iam: &dyn IamMutationApi,
    formatter: &Formatter,
) -> ExitCode {
    let (entity, entity_name) = match (args.user, args.group) {
        (Some(user), None) => (PolicyDetachEntity::User, user),
        (None, Some(group)) => (PolicyDetachEntity::Group, group),
        _ => {
            return emit_policy_detach_error(
                "Invalid policy detach target",
                &Error::InvalidPath(
                    "Specify exactly one target with --user or --group".to_string(),
                ),
                formatter,
            );
        }
    };
    let request = match PolicyDetachRequest::new(args.policies, entity, entity_name) {
        Ok(request) => request,
        Err(error) => {
            return emit_policy_detach_error("Invalid policy detach selectors", &error, formatter);
        }
    };

    let report = match capabilities.discover_capabilities(false).await {
        Ok(report) => report,
        Err(error) => {
            let error = sanitize_capability_discovery_error(&error);
            return emit_policy_detach_error(
                "Failed to discover IAM capabilities",
                &error,
                formatter,
            );
        }
    };
    match report.capability(IAM_POLICY_DETACH_CAPABILITY) {
        Some(capability) if capability.availability == CapabilityAvailability::Available => {}
        Some(capability) if capability.availability == CapabilityAvailability::PermissionDenied => {
            return emit_policy_detach_error(
                "IAM policy detach capability is unavailable",
                &Error::Auth(format!(
                    "{IAM_POLICY_DETACH_CAPABILITY} is permission-denied"
                )),
                formatter,
            );
        }
        Some(capability) => {
            return emit_policy_detach_error(
                "IAM policy detach capability is unavailable",
                &Error::UnsupportedFeature(format!(
                    "{} is {}{}",
                    IAM_POLICY_DETACH_CAPABILITY,
                    capability.availability,
                    capability
                        .reason
                        .as_deref()
                        .map(|reason| format!(": {reason}"))
                        .unwrap_or_default()
                )),
                formatter,
            );
        }
        None => {
            return emit_policy_detach_error(
                "IAM policy detach capability is unavailable",
                &Error::UnsupportedFeature(format!(
                    "{IAM_POLICY_DETACH_CAPABILITY} was not advertised by the server"
                )),
                formatter,
            );
        }
    }

    match iam.detach_policies(&request).await {
        Ok(result) => output_policy_detach(&result, formatter),
        Err(error) => emit_policy_detach_error("Failed to detach IAM policies", &error, formatter),
    }
}

fn output_policy_detach(result: &PolicyDetachResult, formatter: &Formatter) -> ExitCode {
    if formatter.is_json() {
        formatter.json(&policy_detach_success(result));
    } else {
        formatter.println(&format!(
            "Entity: {} {}",
            result.entity,
            formatter.style_name(&result.entity_name)
        ));
        formatter.println(&format!(
            "Detached: {}",
            display_names(&result.detached, formatter)
        ));
        formatter.println(&format!(
            "Unchanged: {}",
            display_names(&result.unchanged, formatter)
        ));
        formatter.println(&format!(
            "Attached: {}",
            display_names(&result.attached, formatter)
        ));
        formatter.println(&format!("Updated: {}", result.updated_at));
    }
    ExitCode::Success
}

fn policy_detach_success(result: &PolicyDetachResult) -> PolicyDetachSuccessOutput<'_> {
    PolicyDetachSuccessOutput {
        schema_version: 3,
        output_type: "iam_policy_detach",
        status: "success",
        data: PolicyDetachOutput {
            operation: "detach",
            entity: PolicyDetachEntityOutput {
                entity_type: result.entity,
                name: &result.entity_name,
            },
            changed: !result.detached.is_empty(),
            attached: &result.attached,
            detached: &result.detached,
            unchanged: &result.unchanged,
            updated_at: result.updated_at,
        },
    }
}

fn emit_policy_detach_error(context: &str, error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("{context}: {error}");
    if formatter.is_json() {
        let unsupported = code == ExitCode::UnsupportedFeature;
        formatter.json_error(&PolicyDetachErrorOutput {
            schema_version: 3,
            output_type: "iam_policy_detach",
            status: "error",
            error: PolicyDetachErrorBody {
                error_type: policy_entities_error_type(code),
                message,
                retryable: code == ExitCode::NetworkError,
                capability: unsupported.then_some(IAM_POLICY_DETACH_CAPABILITY),
                server: unsupported.then_some("rustfs"),
                suggestion: policy_detach_error_suggestion(code),
            },
        });
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

const fn policy_detach_error_suggestion(code: ExitCode) -> Option<&'static str> {
    match code {
        ExitCode::UsageError => Some("Review the policy names and target selector, then retry."),
        ExitCode::NetworkError => {
            Some("The detach is idempotent; verify connectivity and retry the same request safely.")
        }
        ExitCode::AuthError => {
            Some("Verify credentials and the attach-policy admin permission, then retry.")
        }
        ExitCode::NotFound => Some("Verify that the selected user or group still exists."),
        ExitCode::Conflict => Some("Refresh the entity mapping and retry the detach."),
        ExitCode::UnsupportedFeature => {
            Some("Use a RustFS version that advertises builtin IAM policy detach.")
        }
        ExitCode::Interrupted => {
            Some("Retry the same detach request; the operation is idempotent.")
        }
        ExitCode::Success | ExitCode::GeneralError => None,
    }
}

async fn execute_entities(args: EntitiesArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_entities_with_api(args, &client, &client, formatter).await
}

async fn execute_entities_with_api(
    args: EntitiesArgs,
    capabilities: &dyn CapabilityApi,
    iam: &dyn IamReadApi,
    formatter: &Formatter,
) -> ExitCode {
    let query = PolicyEntitiesQuery {
        users: args.user,
        groups: args.group,
        policies: args.policy,
    };
    if let Err(error) = query.validate() {
        return emit_policy_entities_error("Invalid IAM policy-entity query", &error, formatter);
    }

    let report = match capabilities.discover_capabilities(false).await {
        Ok(report) => report,
        Err(error) => {
            let error = sanitize_capability_discovery_error(&error);
            return emit_policy_entities_error(
                "Failed to discover IAM capabilities",
                &error,
                formatter,
            );
        }
    };
    match report.capability(IAM_POLICY_ENTITIES_CAPABILITY) {
        Some(capability) if capability.availability == CapabilityAvailability::Available => {}
        Some(capability) if capability.availability == CapabilityAvailability::PermissionDenied => {
            return emit_policy_entities_error(
                "IAM policy-entity capability is unavailable",
                &Error::Auth(format!(
                    "{} is permission-denied",
                    IAM_POLICY_ENTITIES_CAPABILITY
                )),
                formatter,
            );
        }
        Some(capability) => {
            return emit_policy_entities_error(
                "IAM policy-entity capability is unavailable",
                &Error::UnsupportedFeature(format!(
                    "{} is {}{}",
                    IAM_POLICY_ENTITIES_CAPABILITY,
                    capability.availability,
                    capability
                        .reason
                        .as_deref()
                        .map(|reason| format!(": {reason}"))
                        .unwrap_or_default()
                )),
                formatter,
            );
        }
        None => {
            return emit_policy_entities_error(
                "IAM policy-entity capability is unavailable",
                &Error::UnsupportedFeature(format!(
                    "{IAM_POLICY_ENTITIES_CAPABILITY} was not advertised by the server"
                )),
                formatter,
            );
        }
    }

    match iam.policy_entities(&query).await {
        Ok(result) => output_policy_entities(&result, formatter),
        Err(error) => {
            emit_policy_entities_error("Failed to inspect IAM policy entities", &error, formatter)
        }
    }
}

fn sanitize_capability_discovery_error(error: &Error) -> Error {
    match error.exit_code() {
        2 => Error::Config("RustFS capability discovery could not be configured".to_string()),
        3 => Error::Network("RustFS capability discovery request failed".to_string()),
        4 => Error::Auth("Permission denied during capability discovery".to_string()),
        5 => Error::NotFound("RustFS capability discovery endpoint was not found".to_string()),
        6 => Error::Conflict("RustFS capability discovery encountered a conflict".to_string()),
        7 => Error::UnsupportedFeature(
            "RustFS capability discovery is not supported by this server".to_string(),
        ),
        130 => Error::Interrupted("RustFS capability discovery was interrupted".to_string()),
        _ => Error::General("RustFS capability discovery failed".to_string()),
    }
}

fn output_policy_entities(result: &PolicyEntitiesResult, formatter: &Formatter) -> ExitCode {
    if formatter.is_json() {
        formatter.json(&policy_entities_success(result));
    } else {
        print_policy_entities(result, formatter);
    }
    ExitCode::Success
}

fn policy_entities_success(result: &PolicyEntitiesResult) -> PolicyEntitiesSuccessOutput<'_> {
    PolicyEntitiesSuccessOutput {
        schema_version: 3,
        output_type: "iam_policy_entities",
        status: "success",
        data: PolicyEntitiesOutput {
            timestamp: result.timestamp,
            user_mappings: result
                .user_mappings
                .iter()
                .map(user_policy_entities_output)
                .collect(),
            group_mappings: result
                .group_mappings
                .iter()
                .map(group_policy_entities_output)
                .collect(),
            policy_mappings: result
                .policy_mappings
                .iter()
                .map(|mapping| PolicyEntitiesOutputRow {
                    policy: &mapping.policy,
                    users: &mapping.users,
                    groups: &mapping.groups,
                })
                .collect(),
        },
    }
}

fn user_policy_entities_output(mapping: &UserPolicyEntities) -> UserPolicyEntitiesOutput<'_> {
    UserPolicyEntitiesOutput {
        user: &mapping.user,
        policies: &mapping.policies,
        member_of_mappings: mapping
            .member_of_mappings
            .iter()
            .map(group_policy_entities_output)
            .collect(),
    }
}

fn group_policy_entities_output(mapping: &GroupPolicyEntities) -> GroupPolicyEntitiesOutput<'_> {
    GroupPolicyEntitiesOutput {
        group: &mapping.group,
        policies: &mapping.policies,
    }
}

fn print_policy_entities(result: &PolicyEntitiesResult, formatter: &Formatter) {
    formatter.println(&format!("Timestamp: {}", result.timestamp));
    if result.user_mappings.is_empty()
        && result.group_mappings.is_empty()
        && result.policy_mappings.is_empty()
    {
        formatter.println("No matching policy entity mappings.");
        return;
    }

    if !result.user_mappings.is_empty() {
        formatter.println("");
        formatter.println("Users:");
        for mapping in &result.user_mappings {
            formatter.println(&format!("  {}", formatter.style_name(&mapping.user)));
            formatter.println(&format!(
                "    Direct policies: {}",
                display_names(&mapping.policies, formatter)
            ));
            for inherited in &mapping.member_of_mappings {
                formatter.println(&format!(
                    "    Via group {}: {}",
                    formatter.style_name(&inherited.group),
                    display_names(&inherited.policies, formatter)
                ));
            }
        }
    }

    if !result.group_mappings.is_empty() {
        formatter.println("");
        formatter.println("Groups:");
        for mapping in &result.group_mappings {
            formatter.println(&format!(
                "  {}: {}",
                formatter.style_name(&mapping.group),
                display_names(&mapping.policies, formatter)
            ));
        }
    }

    if !result.policy_mappings.is_empty() {
        formatter.println("");
        formatter.println("Policies:");
        for mapping in &result.policy_mappings {
            formatter.println(&format!("  {}", formatter.style_name(&mapping.policy)));
            formatter.println(&format!(
                "    Users:  {}",
                display_names(&mapping.users, formatter)
            ));
            formatter.println(&format!(
                "    Groups: {}",
                display_names(&mapping.groups, formatter)
            ));
        }
    }
}

fn display_names(names: &[String], formatter: &Formatter) -> String {
    if names.is_empty() {
        "-".to_string()
    } else {
        formatter.sanitize_text(&names.join(", "))
    }
}

fn emit_policy_entities_error(context: &str, error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("{context}: {error}");
    if formatter.is_json() {
        let unsupported = code == ExitCode::UnsupportedFeature;
        formatter.json_error(&PolicyEntitiesErrorOutput {
            schema_version: 3,
            output_type: "iam_policy_entities",
            status: "error",
            error: PolicyEntitiesErrorBody {
                error_type: policy_entities_error_type(code),
                message,
                retryable: code == ExitCode::NetworkError,
                capability: unsupported.then_some(IAM_POLICY_ENTITIES_CAPABILITY),
                server: unsupported.then_some("rustfs"),
                suggestion: policy_entities_error_suggestion(code),
            },
        });
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

const fn policy_entities_error_type(code: ExitCode) -> &'static str {
    match code {
        ExitCode::UsageError => "usage_error",
        ExitCode::NetworkError => "network_error",
        ExitCode::AuthError => "auth_error",
        ExitCode::NotFound => "not_found",
        ExitCode::Conflict => "conflict",
        ExitCode::UnsupportedFeature => "unsupported_feature",
        ExitCode::Interrupted => "interrupted",
        ExitCode::Success | ExitCode::GeneralError => "general_error",
    }
}

const fn policy_entities_error_suggestion(code: ExitCode) -> Option<&'static str> {
    match code {
        ExitCode::UsageError => Some("Review the entity filters and retry."),
        ExitCode::NetworkError => Some("Verify the endpoint and network connectivity, then retry."),
        ExitCode::AuthError => Some(
            "Verify credentials and the list-users, list-groups, and list-user-policies permissions.",
        ),
        ExitCode::UnsupportedFeature => {
            Some("Use a RustFS version that advertises IAM policy-entity inspection.")
        }
        ExitCode::NotFound
        | ExitCode::Conflict
        | ExitCode::Interrupted
        | ExitCode::Success
        | ExitCode::GeneralError => None,
    }
}

async fn execute_list(args: ListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.list_policies().await {
        Ok(policies) => {
            if formatter.is_json() {
                let output = PolicyListOutput {
                    policies: policies.into_iter().map(|p| p.name).collect(),
                };
                formatter.json(&output);
            } else if policies.is_empty() {
                formatter.println("No policies found.");
            } else {
                for policy in policies {
                    let styled_name = formatter.style_name(&policy.name);
                    formatter.println(&format!("  {styled_name}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to list policies: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_create(args: CreateArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    if args.name.is_empty() {
        formatter.error("Policy name cannot be empty");
        return ExitCode::UsageError;
    }

    // Read policy file
    let policy_content = match fs::read_to_string(&args.policy_file) {
        Ok(content) => content,
        Err(e) => {
            formatter.error(&format!(
                "Failed to read policy file '{}': {e}",
                args.policy_file
            ));
            return ExitCode::UsageError;
        }
    };

    // Validate JSON
    if serde_json::from_str::<serde_json::Value>(&policy_content).is_err() {
        formatter.error("Policy file is not valid JSON");
        return ExitCode::UsageError;
    }

    match client.create_policy(&args.name, &policy_content).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = PolicyOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Policy '{}' created successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Policy '{styled_name}' created successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to create policy: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_policy(&args.name).await {
        Ok(policy) => {
            if formatter.is_json() {
                let policy_value: serde_json::Value = policy
                    .parse_document()
                    .unwrap_or(serde_json::Value::String(policy.policy.clone()));
                let output = PolicyInfoOutput {
                    name: policy.name,
                    policy: policy_value,
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&policy.name);
                formatter.println(&format!("Policy: {styled_name}"));
                formatter.println("");
                formatter.println(&policy.policy);
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Policy '{}' not found", args.name));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to get policy info: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: RemoveArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.delete_policy(&args.name).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = PolicyOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Policy '{}' removed successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Policy '{styled_name}' removed successfully."));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Policy '{}' not found", args.name));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to remove policy: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_attach(args: AttachArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let (entity_type, entity_name) = match (&args.user, &args.group) {
        (Some(user), None) => (PolicyEntity::User, user.clone()),
        (None, Some(group)) => (PolicyEntity::Group, group.clone()),
        _ => {
            formatter.error("Must specify either --user or --group");
            return ExitCode::UsageError;
        }
    };

    let policy_names: Vec<String> = args
        .policies
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if policy_names.is_empty() {
        formatter.error("At least one policy name is required");
        return ExitCode::UsageError;
    }

    match client
        .attach_policy(&policy_names, entity_type, &entity_name)
        .await
    {
        Ok(()) => {
            let entity_desc = match entity_type {
                PolicyEntity::User => format!("user '{}'", entity_name),
                PolicyEntity::Group => format!("group '{}'", entity_name),
            };
            if formatter.is_json() {
                let output = PolicyOperationOutput {
                    success: true,
                    name: policy_names.join(","),
                    message: format!(
                        "Policy '{}' attached to {} successfully",
                        policy_names.join(","),
                        entity_desc
                    ),
                };
                formatter.json(&output);
            } else {
                let styled_policies = formatter.style_name(&policy_names.join(", "));
                formatter.success(&format!(
                    "Policy '{styled_policies}' attached to {entity_desc} successfully."
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to attach policy: {e}"));
            ExitCode::GeneralError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use jsonschema::Validator;
    use rc_core::Result;
    use rc_core::admin::{
        CapabilityEntry, CapabilityReport, ClusterSnapshotMetadata, GroupPolicyEntities,
        PolicyEntities, UserPolicyEntities,
    };
    use serde_json::Value;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum ReadResult {
        Success,
        Auth,
        Unsupported,
    }

    struct StubIamApi {
        availability: Option<CapabilityAvailability>,
        read_result: ReadResult,
        read_calls: AtomicUsize,
    }

    #[derive(Clone, Copy)]
    enum DetachResult {
        Changed,
        NoOp,
        Auth,
        Missing,
    }

    struct StubMutationApi {
        availability: Option<CapabilityAvailability>,
        result: DetachResult,
        discovery_calls: AtomicUsize,
        mutation_calls: AtomicUsize,
    }

    impl StubMutationApi {
        fn report(&self) -> CapabilityReport {
            CapabilityReport {
                server_version: Some("1.0.0-beta.10".to_string()),
                runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
                extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
                cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
                capabilities: self
                    .availability
                    .map(|availability| CapabilityEntry {
                        name: IAM_POLICY_DETACH_CAPABILITY.to_string(),
                        availability,
                        reason: None,
                    })
                    .into_iter()
                    .collect(),
                extensions: Vec::new(),
                cluster: ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                },
            }
        }
    }

    #[async_trait]
    impl CapabilityApi for StubMutationApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            self.discovery_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.report())
        }
    }

    #[async_trait]
    impl IamMutationApi for StubMutationApi {
        async fn detach_policies(
            &self,
            request: &PolicyDetachRequest,
        ) -> Result<PolicyDetachResult> {
            self.mutation_calls.fetch_add(1, Ordering::Relaxed);
            match self.result {
                DetachResult::Changed => Ok(PolicyDetachResult {
                    entity: request.entity,
                    entity_name: request.entity_name.clone(),
                    attached: Vec::new(),
                    detached: vec!["diagnostics".to_string(), "readonly".to_string()],
                    unchanged: vec!["writeonly".to_string()],
                    updated_at: "2026-07-24T08:00:00Z".parse().expect("valid timestamp"),
                }),
                DetachResult::NoOp => Ok(PolicyDetachResult {
                    entity: request.entity,
                    entity_name: request.entity_name.clone(),
                    attached: Vec::new(),
                    detached: Vec::new(),
                    unchanged: request.policies.clone(),
                    updated_at: "2026-07-24T08:00:00Z".parse().expect("valid timestamp"),
                }),
                DetachResult::Auth => Err(Error::Auth("permission denied".to_string())),
                DetachResult::Missing => {
                    Err(Error::NotFound("IAM entity does not exist".to_string()))
                }
            }
        }
    }

    impl StubIamApi {
        fn report(&self) -> CapabilityReport {
            CapabilityReport {
                server_version: Some("1.0.0-beta.10".to_string()),
                runtime_path: "/rustfs/admin/v4/runtime/capabilities".to_string(),
                extensions_path: "/rustfs/admin/v4/extensions/catalog".to_string(),
                cluster_snapshot_path: "/rustfs/admin/v4/cluster/snapshot".to_string(),
                capabilities: self
                    .availability
                    .map(|availability| CapabilityEntry {
                        name: IAM_POLICY_ENTITIES_CAPABILITY.to_string(),
                        availability,
                        reason: None,
                    })
                    .into_iter()
                    .collect(),
                extensions: Vec::new(),
                cluster: ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                },
            }
        }
    }

    #[async_trait]
    impl CapabilityApi for StubIamApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            Ok(self.report())
        }
    }

    #[async_trait]
    impl IamReadApi for StubIamApi {
        async fn policy_entities(
            &self,
            _query: &PolicyEntitiesQuery,
        ) -> Result<PolicyEntitiesResult> {
            self.read_calls.fetch_add(1, Ordering::Relaxed);
            match self.read_result {
                ReadResult::Success => Ok(policy_entities_result()),
                ReadResult::Auth => Err(Error::Auth("permission denied".to_string())),
                ReadResult::Unsupported => {
                    Err(Error::UnsupportedFeature("route unavailable".to_string()))
                }
            }
        }
    }

    fn policy_entities_result() -> PolicyEntitiesResult {
        PolicyEntitiesResult {
            timestamp: "2026-07-24T08:00:00Z".parse().expect("valid timestamp"),
            user_mappings: vec![UserPolicyEntities {
                user: "alice".to_string(),
                policies: vec!["readonly".to_string()],
                member_of_mappings: vec![GroupPolicyEntities {
                    group: "ops".to_string(),
                    policies: vec!["diagnostics".to_string()],
                }],
            }],
            group_mappings: vec![GroupPolicyEntities {
                group: "ops".to_string(),
                policies: vec!["diagnostics".to_string()],
            }],
            policy_mappings: vec![PolicyEntities {
                policy: "readonly".to_string(),
                users: vec!["alice".to_string()],
                groups: Vec::new(),
            }],
        }
    }

    fn args() -> EntitiesArgs {
        EntitiesArgs {
            alias: "local".to_string(),
            user: vec!["alice".to_string()],
            group: Vec::new(),
            policy: vec!["readonly".to_string()],
        }
    }

    fn detach_args() -> DetachArgs {
        DetachArgs {
            alias: "local".to_string(),
            policies: vec![
                "readonly".to_string(),
                "diagnostics".to_string(),
                "writeonly".to_string(),
            ],
            user: Some("alice".to_string()),
            group: None,
        }
    }

    fn quiet_formatter() -> Formatter {
        Formatter::new(crate::output::OutputConfig {
            quiet: true,
            ..Default::default()
        })
    }

    fn validator() -> Validator {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("schemas/output_v3.json");
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("output schema should be readable"),
        )
        .expect("output schema should parse");
        jsonschema::validator_for(&schema).expect("output schema should compile")
    }

    fn assert_valid(value: &Value) {
        let errors = validator()
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "invalid output: {}", errors.join("\n"));
    }

    #[test]
    fn test_policy_list_output_serialization() {
        let output = PolicyListOutput {
            policies: vec!["readonly".to_string(), "admin".to_string()],
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("readonly"));
        assert!(json.contains("admin"));
    }

    #[test]
    fn policy_entities_success_matches_v3_schema_and_golden_fixture() {
        let value = serde_json::to_value(policy_entities_success(&policy_entities_result()))
            .expect("serialize policy entities output");
        assert_valid(&value);

        let expected: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/output_v3/iam_policy_entities/success.json"
        ))
        .expect("golden fixture should parse");
        assert_eq!(value, expected);
    }

    #[test]
    fn policy_entities_empty_success_matches_v3_schema() {
        let result = PolicyEntitiesResult {
            timestamp: "2026-07-24T08:00:00Z".parse().expect("valid timestamp"),
            user_mappings: Vec::new(),
            group_mappings: Vec::new(),
            policy_mappings: Vec::new(),
        };
        let value =
            serde_json::to_value(policy_entities_success(&result)).expect("serialize empty output");
        assert_valid(&value);
    }

    #[test]
    fn policy_detach_multi_policy_and_noop_outputs_match_v3_schema() {
        let changed = PolicyDetachResult {
            entity: PolicyDetachEntity::User,
            entity_name: "alice".to_string(),
            attached: Vec::new(),
            detached: vec!["diagnostics".to_string(), "readonly".to_string()],
            unchanged: vec!["writeonly".to_string()],
            updated_at: "2026-07-24T08:00:00Z".parse().expect("valid timestamp"),
        };
        let value = serde_json::to_value(policy_detach_success(&changed))
            .expect("serialize changed output");
        assert_valid(&value);
        assert_eq!(value["data"]["changed"], true);
        assert_eq!(value["data"]["detached"].as_array().map(Vec::len), Some(2));

        let no_op = PolicyDetachResult {
            detached: Vec::new(),
            unchanged: vec!["readonly".to_string()],
            ..changed
        };
        let value =
            serde_json::to_value(policy_detach_success(&no_op)).expect("serialize no-op output");
        assert_valid(&value);
        assert_eq!(value["data"]["changed"], false);
        assert_eq!(value["data"]["unchanged"][0], "readonly");
    }

    #[tokio::test]
    async fn policy_detach_fails_closed_without_sending_mutation() {
        let api = StubMutationApi {
            availability: None,
            result: DetachResult::Changed,
            discovery_calls: AtomicUsize::new(0),
            mutation_calls: AtomicUsize::new(0),
        };
        assert_eq!(
            execute_detach_with_api(detach_args(), &api, &api, &quiet_formatter()).await,
            ExitCode::UnsupportedFeature
        );
        assert_eq!(api.discovery_calls.load(Ordering::Relaxed), 1);
        assert_eq!(api.mutation_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn policy_detach_rejects_selectors_before_capability_discovery() {
        let api = StubMutationApi {
            availability: Some(CapabilityAvailability::Available),
            result: DetachResult::Changed,
            discovery_calls: AtomicUsize::new(0),
            mutation_calls: AtomicUsize::new(0),
        };
        let mut invalid = detach_args();
        invalid.policies = vec![" readonly ".to_string()];
        assert_eq!(
            execute_detach_with_api(invalid, &api, &api, &quiet_formatter()).await,
            ExitCode::UsageError
        );
        assert_eq!(api.discovery_calls.load(Ordering::Relaxed), 0);
        assert_eq!(api.mutation_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn policy_detach_preserves_auth_and_missing_entity_exit_codes() {
        for (result, expected) in [
            (DetachResult::Auth, ExitCode::AuthError),
            (DetachResult::Missing, ExitCode::NotFound),
            (DetachResult::NoOp, ExitCode::Success),
        ] {
            let api = StubMutationApi {
                availability: Some(CapabilityAvailability::Available),
                result,
                discovery_calls: AtomicUsize::new(0),
                mutation_calls: AtomicUsize::new(0),
            };
            assert_eq!(
                execute_detach_with_api(detach_args(), &api, &api, &quiet_formatter()).await,
                expected
            );
            assert_eq!(api.mutation_calls.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn policy_entities_fails_closed_when_capability_is_missing() {
        let api = StubIamApi {
            availability: None,
            read_result: ReadResult::Success,
            read_calls: AtomicUsize::new(0),
        };
        assert_eq!(
            execute_entities_with_api(args(), &api, &api, &quiet_formatter()).await,
            ExitCode::UnsupportedFeature
        );
        assert_eq!(api.read_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn policy_entities_preserves_auth_and_route_unsupported_exit_codes() {
        for (read_result, expected) in [
            (ReadResult::Auth, ExitCode::AuthError),
            (ReadResult::Unsupported, ExitCode::UnsupportedFeature),
        ] {
            let api = StubIamApi {
                availability: Some(CapabilityAvailability::Available),
                read_result,
                read_calls: AtomicUsize::new(0),
            };
            assert_eq!(
                execute_entities_with_api(args(), &api, &api, &quiet_formatter()).await,
                expected
            );
            assert_eq!(api.read_calls.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn policy_entities_rejects_invalid_selector_before_discovery_or_read() {
        let api = StubIamApi {
            availability: Some(CapabilityAvailability::Available),
            read_result: ReadResult::Success,
            read_calls: AtomicUsize::new(0),
        };
        let mut invalid = args();
        invalid.user = vec![" ".to_string()];
        assert_eq!(
            execute_entities_with_api(invalid, &api, &api, &quiet_formatter()).await,
            ExitCode::UsageError
        );
        assert_eq!(api.read_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn capability_discovery_errors_do_not_echo_server_or_credential_text() {
        for error in [
            Error::Auth("access-key secret-key".to_string()),
            Error::Network("HTTP 500: session-token".to_string()),
            Error::General("credential material".to_string()),
        ] {
            let sanitized = sanitize_capability_discovery_error(&error);
            let message = sanitized.to_string();
            for secret in [
                "access-key",
                "secret-key",
                "session-token",
                "credential material",
            ] {
                assert!(!message.contains(secret));
            }
            assert_eq!(sanitized.exit_code(), error.exit_code());
        }
    }
}
