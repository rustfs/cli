//! User management commands
//!
//! Commands for managing IAM users: list, add, info, remove, enable, disable.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{AdminApi, User, UserStatus};

const ADD_USER_AFTER_HELP: &str = "\
Examples:
  rc admin user add local analyst analyst-secret
  rc admin user add local deployer deployer-secret
  rc admin user add production readonly-user long-secret-value";

/// User management subcommands
#[derive(Subcommand, Debug)]
pub enum UserCommands {
    /// List all users
    #[command(name = "ls", alias = "list")]
    List(ListArgs),

    /// Add a new user
    Add(AddArgs),

    /// Get user information
    Info(InfoArgs),

    /// Remove a user
    #[command(name = "rm", alias = "remove")]
    Remove(RemoveArgs),

    /// Enable a user
    Enable(EnableArgs),

    /// Disable a user
    Disable(DisableArgs),
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
#[command(after_help = ADD_USER_AFTER_HELP)]
pub struct AddArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key (username) for the new user
    pub access_key: String,

    /// Secret key (password) for the new user
    pub secret_key: String,
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user
    pub access_key: String,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user to remove
    pub access_key: String,
}

#[derive(clap::Args, Debug)]
pub struct EnableArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user to enable
    pub access_key: String,
}

#[derive(clap::Args, Debug)]
pub struct DisableArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user to disable
    pub access_key: String,
}

/// JSON output for user list
#[derive(Serialize)]
struct UserListOutput {
    users: Vec<UserInfo>,
}

/// JSON representation of a user
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserInfo {
    access_key: String,
    status: String,
    policies: Vec<String>,
    member_of: Vec<String>,
}

impl From<User> for UserInfo {
    fn from(user: User) -> Self {
        let policies = user.policies();
        Self {
            access_key: user.access_key,
            status: user.status.to_string(),
            policies,
            member_of: user.member_of,
        }
    }
}

/// JSON output for user operations
#[derive(Serialize)]
struct UserOperationOutput {
    success: bool,
    access_key: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_key: Option<String>,
}

/// Execute a user subcommand
pub async fn execute(cmd: UserCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        UserCommands::List(args) => execute_list(args, formatter).await,
        UserCommands::Add(args) => execute_add(args, formatter).await,
        UserCommands::Info(args) => execute_info(args, formatter).await,
        UserCommands::Remove(args) => execute_remove(args, formatter).await,
        UserCommands::Enable(args) => execute_enable(args, formatter).await,
        UserCommands::Disable(args) => execute_disable(args, formatter).await,
    }
}

async fn execute_list(args: ListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.list_users().await {
        Ok(users) => {
            if formatter.is_json() {
                let output = UserListOutput {
                    users: users.into_iter().map(UserInfo::from).collect(),
                };
                formatter.json(&output);
            } else if users.is_empty() {
                formatter.println("No users found.");
            } else {
                for user in users {
                    let status_icon = match user.status {
                        UserStatus::Enabled => formatter.style_size("●"),
                        UserStatus::Disabled => formatter.style_date("○"),
                    };
                    let styled_key = formatter.style_name(&user.access_key);
                    let policies = user.policies().join(", ");
                    if policies.is_empty() {
                        formatter.println(&format!("{status_icon} {styled_key}"));
                    } else {
                        let styled_policies = formatter.style_date(&policies);
                        formatter.println(&format!(
                            "{status_icon} {styled_key} (policies: {styled_policies})"
                        ));
                    }
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to list users: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_add(args: AddArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    if args.access_key.is_empty() {
        return formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Access key cannot be empty",
            "Provide a non-empty access key for the new user.",
        );
    }

    if args.secret_key.len() < 8 {
        return formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Secret key must be at least 8 characters long",
            "Provide a secret key that is at least 8 characters long.",
        );
    }

    match client.create_user(&args.access_key, &args.secret_key).await {
        Ok(user) => {
            if formatter.is_json() {
                let output = UserOperationOutput {
                    success: true,
                    access_key: user.access_key.clone(),
                    message: format!("User '{}' created successfully", user.access_key),
                    secret_key: user.secret_key,
                };
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&user.access_key);
                formatter.success(&format!("User '{styled_key}' created successfully."));
            }
            ExitCode::Success
        }
        Err(e) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to create user: {e}"),
        ),
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_user(&args.access_key).await {
        Ok(user) => {
            if formatter.is_json() {
                formatter.json(&UserInfo::from(user));
            } else {
                let styled_key = formatter.style_name(&user.access_key);
                let status = match user.status {
                    UserStatus::Enabled => formatter.style_size("enabled"),
                    UserStatus::Disabled => formatter.style_date("disabled"),
                };
                formatter.println(&format!("Access Key: {styled_key}"));
                formatter.println(&format!("Status:     {status}"));

                let policies = user.policies();
                if policies.is_empty() {
                    formatter.println("Policies:   (none)");
                } else {
                    formatter.println(&format!("Policies:   {}", policies.join(", ")));
                }

                if user.member_of.is_empty() {
                    formatter.println("Groups:     (none)");
                } else {
                    formatter.println(&format!("Groups:     {}", user.member_of.join(", ")));
                }
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("User '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to get user info: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: RemoveArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.delete_user(&args.access_key).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = UserOperationOutput {
                    success: true,
                    access_key: args.access_key.clone(),
                    message: format!("User '{}' removed successfully", args.access_key),
                    secret_key: None,
                };
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&args.access_key);
                formatter.success(&format!("User '{styled_key}' removed successfully."));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("User '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to remove user: {e}"));
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
        .set_user_status(&args.access_key, UserStatus::Enabled)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                let output = UserOperationOutput {
                    success: true,
                    access_key: args.access_key.clone(),
                    message: format!("User '{}' enabled successfully", args.access_key),
                    secret_key: None,
                };
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&args.access_key);
                formatter.success(&format!("User '{styled_key}' enabled successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to enable user: {e}"));
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
        .set_user_status(&args.access_key, UserStatus::Disabled)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                let output = UserOperationOutput {
                    success: true,
                    access_key: args.access_key.clone(),
                    message: format!("User '{}' disabled successfully", args.access_key),
                    secret_key: None,
                };
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&args.access_key);
                formatter.success(&format!("User '{styled_key}' disabled successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to disable user: {e}"));
            ExitCode::GeneralError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_info_from_user() {
        let user = User {
            access_key: "testuser".to_string(),
            secret_key: None,
            status: UserStatus::Enabled,
            policy_name: Some("policy1,policy2".to_string()),
            member_of: vec!["group1".to_string()],
        };

        let info = UserInfo::from(user);
        assert_eq!(info.access_key, "testuser");
        assert_eq!(info.status, "enabled");
        assert_eq!(info.policies, vec!["policy1", "policy2"]);
        assert_eq!(info.member_of, vec!["group1"]);
    }
}
