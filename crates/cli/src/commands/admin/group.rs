//! Group management commands
//!
//! Commands for managing IAM groups: list, add, info, remove, enable, disable.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, Group, GroupStatus};

/// Group management subcommands
#[derive(Subcommand, Debug)]
pub enum GroupCommands {
    /// List all groups
    #[command(name = "ls", alias = "list")]
    List(ListArgs),

    /// Create a new group
    Add(AddArgs),

    /// Get group information
    Info(InfoArgs),

    /// Remove a group
    #[command(name = "rm", alias = "remove")]
    Remove(RemoveArgs),

    /// Enable a group
    Enable(EnableArgs),

    /// Disable a group
    Disable(DisableArgs),

    /// Add members to a group
    #[command(name = "add-members")]
    AddMembers(AddMembersArgs),

    /// Remove members from a group
    #[command(name = "rm-members")]
    RemoveMembers(RemoveMembersArgs),
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name
    pub name: String,

    /// Initial members (comma-separated access keys)
    #[arg(long)]
    pub members: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name to remove
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct EnableArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name to enable
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct DisableArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name to disable
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct AddMembersArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name
    pub name: String,

    /// Members to add (comma-separated access keys)
    pub members: String,
}

#[derive(clap::Args, Debug)]
pub struct RemoveMembersArgs {
    /// Alias name of the server
    pub alias: String,

    /// Group name
    pub name: String,

    /// Members to remove (comma-separated access keys)
    pub members: String,
}

/// JSON output for group list
#[derive(Serialize)]
struct GroupListOutput {
    groups: Vec<String>,
}

/// JSON representation of a group
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GroupInfo {
    name: String,
    status: String,
    policies: Vec<String>,
    members: Vec<String>,
}

impl From<Group> for GroupInfo {
    fn from(group: Group) -> Self {
        let policies = group.policies();
        Self {
            name: group.name,
            status: group.status.to_string(),
            policies,
            members: group.members,
        }
    }
}

/// JSON output for group operations
#[derive(Serialize)]
struct GroupOperationOutput {
    success: bool,
    name: String,
    message: String,
}

/// Execute a group subcommand
pub async fn execute(cmd: GroupCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        GroupCommands::List(args) => execute_list(args, formatter).await,
        GroupCommands::Add(args) => execute_add(args, formatter).await,
        GroupCommands::Info(args) => execute_info(args, formatter).await,
        GroupCommands::Remove(args) => execute_remove(args, formatter).await,
        GroupCommands::Enable(args) => execute_enable(args, formatter).await,
        GroupCommands::Disable(args) => execute_disable(args, formatter).await,
        GroupCommands::AddMembers(args) => execute_add_members(args, formatter).await,
        GroupCommands::RemoveMembers(args) => execute_remove_members(args, formatter).await,
    }
}

async fn execute_list(args: ListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.list_groups().await {
        Ok(groups) => {
            if formatter.is_json() {
                let output = GroupListOutput { groups };
                formatter.json(&output);
            } else if groups.is_empty() {
                formatter.println("No groups found.");
            } else {
                for group in groups {
                    let styled_name = formatter.style_name(&group);
                    formatter.println(&format!("  {styled_name}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to list groups: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_add(args: AddArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    if args.name.is_empty() {
        formatter.error("Group name cannot be empty");
        return ExitCode::UsageError;
    }

    let members: Vec<String> = args
        .members
        .as_ref()
        .map(|m| {
            m.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if members.is_empty() {
        if formatter.is_json() {
            let output = GroupOperationOutput {
                success: true,
                name: args.name.clone(),
                message: format!("Group '{}' created successfully", args.name),
            };
            formatter.json(&output);
        } else {
            let styled_name = formatter.style_name(&args.name);
            formatter.success(&format!("Group '{styled_name}' created successfully."));
        }
        ExitCode::Success
    } else {
        match client.add_group_members(&args.name, &members).await {
            Ok(()) => {
                if formatter.is_json() {
                    let output = GroupOperationOutput {
                        success: true,
                        name: args.name.clone(),
                        message: format!("Group '{}' created successfully with members", args.name),
                    };
                    formatter.json(&output);
                } else {
                    let styled_name = formatter.style_name(&args.name);
                    formatter.success(&format!("Group '{styled_name}' created successfully."));
                }
                ExitCode::Success
            }
            Err(e) => {
                formatter.error(&format!("Failed to create group: {e}"));
                ExitCode::GeneralError
            }
        }
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_group(&args.name).await {
        Ok(group) => {
            if formatter.is_json() {
                formatter.json(&GroupInfo::from(group));
            } else {
                let styled_name = formatter.style_name(&group.name);
                let status = match group.status {
                    GroupStatus::Enabled => formatter.style_size("enabled"),
                    GroupStatus::Disabled => formatter.style_date("disabled"),
                };
                formatter.println(&format!("Group:    {styled_name}"));
                formatter.println(&format!("Status:   {status}"));

                let policies = group.policies();
                if policies.is_empty() {
                    formatter.println("Policies: (none)");
                } else {
                    formatter.println(&format!("Policies: {}", policies.join(", ")));
                }

                if group.members.is_empty() {
                    formatter.println("Members:  (none)");
                } else {
                    formatter.println(&format!("Members:  {}", group.members.join(", ")));
                }
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Group '{}' not found", args.name));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to get group info: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: RemoveArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.delete_group(&args.name).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = GroupOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Group '{}' removed successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Group '{styled_name}' removed successfully."));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Group '{}' not found", args.name));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to remove group: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_enable(args: EnableArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client
        .set_group_status(&args.name, GroupStatus::Enabled)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                let output = GroupOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Group '{}' enabled successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Group '{styled_name}' enabled successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to enable group: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_disable(args: DisableArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client
        .set_group_status(&args.name, GroupStatus::Disabled)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                let output = GroupOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Group '{}' disabled successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Group '{styled_name}' disabled successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to disable group: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_add_members(args: AddMembersArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let members: Vec<String> = args
        .members
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if members.is_empty() {
        formatter.error("At least one member is required");
        return ExitCode::UsageError;
    }

    match client.add_group_members(&args.name, &members).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = GroupOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Members added to group '{}' successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!(
                    "Members added to group '{styled_name}' successfully."
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to add members: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove_members(args: RemoveMembersArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let members: Vec<String> = args
        .members
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if members.is_empty() {
        formatter.error("At least one member is required");
        return ExitCode::UsageError;
    }

    match client.remove_group_members(&args.name, &members).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = GroupOperationOutput {
                    success: true,
                    name: args.name.clone(),
                    message: format!("Members removed from group '{}' successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!(
                    "Members removed from group '{styled_name}' successfully."
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to remove members: {e}"));
            ExitCode::GeneralError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_info_from_group() {
        let group = Group {
            name: "developers".to_string(),
            policy: Some("readonly,writeonly".to_string()),
            members: vec!["user1".to_string(), "user2".to_string()],
            status: GroupStatus::Enabled,
        };

        let info = GroupInfo::from(group);
        assert_eq!(info.name, "developers");
        assert_eq!(info.status, "enabled");
        assert_eq!(info.policies, vec!["readonly", "writeonly"]);
        assert_eq!(info.members, vec!["user1", "user2"]);
    }
}
