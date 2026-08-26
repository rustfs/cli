//! User management commands
//!
//! Commands for managing IAM users: list, add, info, remove, enable, disable.

use std::path::PathBuf;

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::confirm::{Confirmation, confirm};
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use crate::secret_input::{SecretSource, can_prompt};
use rc_core::admin::{AdminApi, SecretValue, User, UserCredentialApi, UserStatus};

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

    /// Reset a user's password (S3 secret key)
    #[command(after_help = PASSWD_AFTER_HELP)]
    Passwd(PasswdArgs),

    /// Inspect or clear a user's two-factor authentication
    #[command(subcommand)]
    Mfa(UserMfaCommands),
}

const PASSWD_AFTER_HELP: &str = "\
Examples:
  rc admin user passwd local analyst --password-from-env NEW_PW
  rc admin user passwd local analyst --password-file ./new.txt

Unlike re-creating the user, this changes only the secret key: the account's
status, policies and group memberships are left alone.";

const USER_MFA_AFTER_HELP: &str = "\
Examples:
  rc admin user mfa status local analyst
  rc admin user mfa reset local analyst --yes

`reset` is the break-glass path for a user who lost both their authenticator and
their recovery codes. It removes their second factor, so the account is left
protected by its password alone until they enrol again.";

#[derive(clap::Args, Debug)]
#[command(after_help = PASSWD_AFTER_HELP)]
pub struct PasswdArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user whose password is being reset
    pub access_key: String,

    /// Read the new password from this environment variable
    #[arg(long, value_name = "NAME")]
    pub password_from_env: Option<String>,

    /// Read the new password from the first line of this file
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
#[command(after_help = USER_MFA_AFTER_HELP)]
pub enum UserMfaCommands {
    /// Show whether a user has two-factor authentication enabled
    Status(UserMfaStatusArgs),

    /// Clear a user's second factor (break-glass)
    Reset(UserMfaResetArgs),
}

#[derive(clap::Args, Debug)]
pub struct UserMfaStatusArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user to inspect
    pub access_key: String,
}

#[derive(clap::Args, Debug)]
pub struct UserMfaResetArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the user whose second factor is being cleared
    pub access_key: String,

    /// Confirm without an interactive prompt
    #[arg(long)]
    pub yes: bool,
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
        UserCommands::Passwd(args) => execute_passwd(args, formatter).await,
        UserCommands::Mfa(mfa_cmd) => execute_user_mfa(mfa_cmd, formatter).await,
    }
}

/// JSON output for a password reset
#[derive(Serialize)]
struct UserPasswordOutput {
    success: bool,
    access_key: String,
    sessions_revoked: u32,
    message: String,
}

/// JSON output for a user's two-factor state
#[derive(Serialize)]
struct UserMfaStatusOutput {
    access_key: String,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    activated_at: Option<String>,
    recovery_codes_remaining: u32,
}

async fn execute_passwd(args: PasswdArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    if args.access_key.is_empty() {
        return formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Access key cannot be empty",
            "Provide the access key of the user whose password is being reset.",
        );
    }

    let source = match SecretSource::resolve(
        args.password_from_env,
        args.password_file,
        can_prompt(formatter.is_json()),
        "password",
    ) {
        Ok(source) => source,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error.to_string()),
    };
    let secret = match source.load(&format!("New password for {}: ", args.access_key)) {
        Ok(value) => SecretValue::new(value.to_string()),
        Err(error) => return formatter.fail(ExitCode::UsageError, &error.to_string()),
    };

    match client.set_user_secret_key(&args.access_key, &secret).await {
        Ok(result) => {
            let message = if result.sessions_revoked > 0 {
                format!(
                    "Password reset for '{}'. {} session(s) were signed out.",
                    args.access_key, result.sessions_revoked
                )
            } else {
                format!("Password reset for '{}'.", args.access_key)
            };

            if formatter.is_json() {
                formatter.json(&UserPasswordOutput {
                    success: true,
                    access_key: args.access_key,
                    sessions_revoked: result.sessions_revoked,
                    message,
                });
            } else {
                formatter.println(&message);
            }
            ExitCode::Success
        }
        Err(error) => {
            let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
            formatter.fail(code, &format!("Failed to reset the password: {error}"))
        }
    }
}

async fn execute_user_mfa(cmd: UserMfaCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        UserMfaCommands::Status(args) => execute_user_mfa_status(args, formatter).await,
        UserMfaCommands::Reset(args) => execute_user_mfa_reset(args, formatter).await,
    }
}

async fn execute_user_mfa_status(args: UserMfaStatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.user_mfa_status(&args.access_key).await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&UserMfaStatusOutput {
                    access_key: status.access_key,
                    enabled: status.enabled,
                    activated_at: status.activated_at,
                    recovery_codes_remaining: status.recovery_codes_remaining,
                });
            } else {
                formatter.println(&format!(
                    "{}: two-factor authentication {}",
                    formatter.style_name(&status.access_key),
                    if status.enabled { "on" } else { "off" }
                ));
                if status.enabled {
                    if let Some(activated) = &status.activated_at {
                        formatter.println(&format!(
                            "  Enabled on: {}",
                            formatter.sanitize_text(activated)
                        ));
                    }
                    formatter.println(&format!(
                        "  Recovery:   {} code(s) remaining",
                        status.recovery_codes_remaining
                    ));
                }
            }
            ExitCode::Success
        }
        Err(error) => {
            let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
            formatter.fail(code, &format!("Failed to read two-factor state: {error}"))
        }
    }
}

async fn execute_user_mfa_reset(args: UserMfaResetArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    if let Err(error) = confirm_mfa_reset(&args.access_key, args.yes, formatter) {
        let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
        return formatter.fail(code, &error.to_string());
    }

    match client.user_mfa_reset(&args.access_key).await {
        Ok(()) => {
            let message = format!(
                "Two-factor authentication cleared for '{}'. The account is now protected by its password alone.",
                args.access_key
            );
            if formatter.is_json() {
                formatter.json(&UserOperationOutput {
                    success: true,
                    access_key: args.access_key,
                    message,
                    secret_key: None,
                });
            } else {
                formatter.println(&message);
            }
            ExitCode::Success
        }
        Err(error) => {
            let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
            formatter.fail(
                code,
                &format!("Failed to clear two-factor authentication: {error}"),
            )
        }
    }
}

/// Confirm a break-glass reset, naming the target.
///
/// Removing somebody's second factor is not reversible from the operator's side
/// — the user has to enrol again — so a non-interactive run must say `--yes`
/// rather than have the confirmation silently skipped.
fn confirm_mfa_reset(access_key: &str, yes: bool, formatter: &Formatter) -> rc_core::Result<()> {
    let prompt = format!(
        "Clear two-factor authentication for '{}'? The account will be protected by its password alone. [y/N]",
        formatter.sanitize_text(access_key)
    );
    confirm(
        &Confirmation {
            prompt: &prompt,
            requires_yes: "Clearing a user's second factor requires --yes in non-interactive or JSON mode",
            declined: "Clearing two-factor authentication was declined",
        },
        yes,
        formatter,
    )
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
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: UserCommands,
    }

    #[test]
    fn passwd_parses_a_target_and_an_environment_source() {
        let cli = TestCli::parse_from([
            "user",
            "passwd",
            "local",
            "analyst",
            "--password-from-env",
            "NEW_PW",
        ]);
        match cli.command {
            UserCommands::Passwd(args) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.access_key, "analyst");
                assert_eq!(args.password_from_env.as_deref(), Some("NEW_PW"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn passwd_requires_a_target_access_key() {
        // Without a target this would be ambiguous with the self-service
        // `account passwd`, so clap must reject it rather than guess.
        assert!(TestCli::try_parse_from(["user", "passwd", "local"]).is_err());
    }

    #[test]
    fn mfa_status_parses_a_target() {
        let cli = TestCli::parse_from(["user", "mfa", "status", "local", "analyst"]);
        match cli.command {
            UserCommands::Mfa(UserMfaCommands::Status(args)) => {
                assert_eq!(args.access_key, "analyst");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn mfa_reset_carries_an_explicit_confirmation_flag() {
        let cli = TestCli::parse_from(["user", "mfa", "reset", "local", "analyst", "--yes"]);
        match cli.command {
            UserCommands::Mfa(UserMfaCommands::Reset(args)) => {
                assert!(args.yes);
                assert_eq!(args.access_key, "analyst");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn a_json_mode_reset_without_confirmation_is_a_usage_error() {
        // Never silently proceed: a scripted break-glass must be deliberate.
        let formatter = Formatter::new(crate::output::OutputConfig {
            json: true,
            ..Default::default()
        });

        let error = confirm_mfa_reset("analyst", false, &formatter).expect_err("must refuse");
        assert!(matches!(error, rc_core::Error::InvalidPath(_)), "{error:?}");
        assert!(error.to_string().contains("--yes"), "{error}");
    }

    #[test]
    fn an_explicit_yes_skips_confirmation_even_in_json_mode() {
        let formatter = Formatter::new(crate::output::OutputConfig {
            json: true,
            ..Default::default()
        });

        confirm_mfa_reset("analyst", true, &formatter).expect("--yes must be honoured");
    }

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
