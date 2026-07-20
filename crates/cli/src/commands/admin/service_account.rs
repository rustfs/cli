//! Service account management commands
//!
//! Commands for managing service accounts: list, create, update, info, remove.

use clap::Subcommand;
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{
    AdminApi, CreateServiceAccountRequest, ServiceAccount, UpdateServiceAccountRequest,
};

const DEFAULT_SERVICE_ACCOUNT_EXPIRY: &str = "9999-12-31T23:59:59Z";

/// Service account management subcommands
#[derive(Subcommand, Debug)]
pub enum ServiceAccountCommands {
    /// List service accounts
    #[command(name = "ls", alias = "list")]
    List(ListArgs),

    /// Create a new service account
    Create(CreateArgs),

    /// Update an existing service account
    #[command(aliases = ["edit", "set"])]
    Update(UpdateArgs),

    /// Get service account information
    Info(InfoArgs),

    /// Remove a service account
    #[command(name = "rm", alias = "remove")]
    Remove(RemoveArgs),
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Alias name of the server
    pub alias: String,

    /// Filter by parent user
    #[arg(long)]
    pub user: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct CreateArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key for the service account
    pub access_key: String,

    /// Secret key for the service account
    pub secret_key: String,

    /// Optional name for the service account
    #[arg(long)]
    pub name: Option<String>,

    /// Optional description
    #[arg(long)]
    pub description: Option<String>,

    /// Optional policy document (JSON file path)
    #[arg(long)]
    pub policy: Option<String>,

    /// Optional expiration time (ISO 8601 format)
    #[arg(long)]
    pub expiry: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct UpdateArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the service account
    pub access_key: String,

    /// Replacement secret key
    #[arg(long)]
    pub secret_key: Option<String>,

    /// Replacement name for the service account
    #[arg(long)]
    pub name: Option<String>,

    /// Replacement description
    #[arg(long)]
    pub description: Option<String>,

    /// Replacement policy document (JSON file path)
    #[arg(long)]
    pub policy: Option<String>,

    /// Replacement expiration time (ISO 8601 format)
    #[arg(long)]
    pub expiry: Option<String>,

    /// Replacement account status
    #[arg(long, value_parser = ["enabled", "disabled"])]
    pub status: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the service account
    pub access_key: String,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key of the service account to remove
    pub access_key: String,
}

/// JSON output for service account list
#[derive(Serialize)]
struct ServiceAccountListOutput {
    accounts: Vec<ServiceAccountInfo>,
}

/// JSON representation of a service account
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceAccountInfo {
    access_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiration: Option<String>,
}

impl From<ServiceAccount> for ServiceAccountInfo {
    fn from(sa: ServiceAccount) -> Self {
        Self {
            access_key: sa.access_key,
            secret_key: sa.secret_key,
            parent_user: sa.parent_user,
            account_status: sa.account_status,
            expiration: sa.expiration,
        }
    }
}

/// JSON output for service account create
#[derive(Serialize)]
struct ServiceAccountCreateOutput {
    success: bool,
    access_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_key: Option<String>,
    message: String,
}

/// JSON output for service account operations
#[derive(Serialize)]
struct ServiceAccountOperationOutput {
    success: bool,
    access_key: String,
    message: String,
}

/// Execute a service account subcommand
pub async fn execute(cmd: ServiceAccountCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        ServiceAccountCommands::List(args) => execute_list(args, formatter).await,
        ServiceAccountCommands::Create(args) => execute_create(args, formatter).await,
        ServiceAccountCommands::Update(args) => execute_update(args, formatter).await,
        ServiceAccountCommands::Info(args) => execute_info(args, formatter).await,
        ServiceAccountCommands::Remove(args) => execute_remove(args, formatter).await,
    }
}

async fn execute_list(args: ListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.list_service_accounts(args.user.as_deref()).await {
        Ok(accounts) => {
            if formatter.is_json() {
                let output = ServiceAccountListOutput {
                    accounts: accounts.into_iter().map(ServiceAccountInfo::from).collect(),
                };
                formatter.json(&output);
            } else if accounts.is_empty() {
                formatter.println("No service accounts found.");
            } else {
                for sa in accounts {
                    let styled_key = formatter.style_name(&sa.access_key);
                    let parent = sa
                        .parent_user
                        .map(|p| format!(" (parent: {})", p))
                        .unwrap_or_default();
                    let status = sa
                        .account_status
                        .map(|s| format!(" [{}]", s))
                        .unwrap_or_default();
                    formatter.println(&format!("  {styled_key}{parent}{status}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to list service accounts: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_create(args: CreateArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    // Read policy if provided
    let policy = if let Some(policy_path) = &args.policy {
        match std::fs::read_to_string(policy_path) {
            Ok(content) => Some(content),
            Err(e) => {
                formatter.error(&format!(
                    "Failed to read policy file '{}': {e}",
                    policy_path
                ));
                return ExitCode::UsageError;
            }
        }
    } else {
        None
    };

    let request = build_create_service_account_request(&args, policy);
    let create_result = match client.create_service_account(request.clone()).await {
        Ok(sa) => Ok(sa),
        Err(e) if should_retry_with_default_expiry(&args, &e) => {
            let mut retry_request = request;
            retry_request.expiry = Some(DEFAULT_SERVICE_ACCOUNT_EXPIRY.to_string());
            client.create_service_account(retry_request).await
        }
        Err(e) => Err(e),
    };

    match create_result {
        Ok(sa) => {
            if formatter.is_json() {
                let output = ServiceAccountCreateOutput {
                    success: true,
                    access_key: sa.access_key.clone(),
                    secret_key: sa.secret_key.clone(),
                    message: "Service account created successfully".to_string(),
                };
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&sa.access_key);
                formatter.success("Service account created successfully.");
                formatter.println(&format!("Access Key: {styled_key}"));
                if let Some(secret) = &sa.secret_key {
                    formatter.println(&format!("Secret Key: {secret}"));
                    formatter.println("");
                    formatter
                        .warning("Make sure to save the secret key, it cannot be retrieved later!");
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to create service account: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn build_create_service_account_request(
    args: &CreateArgs,
    policy: Option<String>,
) -> CreateServiceAccountRequest {
    CreateServiceAccountRequest {
        policy,
        expiry: args.expiry.clone(),
        // Keep existing CLI UX where positional ACCESS_KEY is enough.
        name: args.name.clone().or_else(|| Some(args.access_key.clone())),
        description: args.description.clone(),
        access_key: args.access_key.clone(),
        secret_key: args.secret_key.clone(),
    }
}

fn should_retry_with_default_expiry(args: &CreateArgs, error: &rc_core::Error) -> bool {
    if args.expiry.is_some() {
        return false;
    }

    let text = error.to_string();
    text.contains("missing field `expiration`") || text.contains("InvalidExpiration")
}

async fn execute_update(args: UpdateArgs, formatter: &Formatter) -> ExitCode {
    let policy = if let Some(policy_path) = &args.policy {
        match std::fs::read_to_string(policy_path) {
            Ok(content) => Some(content),
            Err(e) => {
                formatter.error(&format!(
                    "Failed to read policy file '{}': {e}",
                    policy_path
                ));
                return ExitCode::UsageError;
            }
        }
    } else {
        None
    };

    let request = build_update_service_account_request(&args, policy);
    if request.is_empty() {
        formatter.error("Service account update requires at least one field");
        return ExitCode::UsageError;
    }

    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client
        .update_service_account(&args.access_key, request)
        .await
    {
        Ok(()) => {
            let output = ServiceAccountOperationOutput {
                success: true,
                access_key: args.access_key.clone(),
                message: format!("Service account '{}' updated successfully", args.access_key),
            };
            if formatter.is_json() {
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&args.access_key);
                formatter.success(&format!(
                    "Service account '{styled_key}' updated successfully."
                ));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Service account '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to update service account: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn build_update_service_account_request(
    args: &UpdateArgs,
    policy: Option<String>,
) -> UpdateServiceAccountRequest {
    UpdateServiceAccountRequest {
        new_policy: policy,
        new_secret_key: args.secret_key.clone(),
        new_status: args.status.clone(),
        new_name: args.name.clone(),
        new_description: args.description.clone(),
        new_expiration: args.expiry.clone(),
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_service_account(&args.access_key).await {
        Ok(sa) => {
            if formatter.is_json() {
                formatter.json(&ServiceAccountInfo::from(sa));
            } else {
                let styled_key = formatter.style_name(&sa.access_key);
                formatter.println(&format!("Access Key:  {styled_key}"));

                if let Some(parent) = &sa.parent_user {
                    formatter.println(&format!("Parent User: {parent}"));
                }

                if let Some(status) = &sa.account_status {
                    formatter.println(&format!("Status:      {status}"));
                }

                if let Some(expiry) = &sa.expiration {
                    formatter.println(&format!("Expiration:  {expiry}"));
                }

                if let Some(policy) = &sa.policy {
                    formatter.println("");
                    formatter.println("Policy:");
                    formatter.println(policy);
                }
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Service account '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to get service account info: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: RemoveArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.delete_service_account(&args.access_key).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = ServiceAccountOperationOutput {
                    success: true,
                    access_key: args.access_key.clone(),
                    message: format!("Service account '{}' removed successfully", args.access_key),
                };
                formatter.json(&output);
            } else {
                let styled_key = formatter.style_name(&args.access_key);
                formatter.success(&format!(
                    "Service account '{styled_key}' removed successfully."
                ));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Service account '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&format!("Failed to remove service account: {e}"));
            ExitCode::GeneralError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_account_info_from() {
        let sa = ServiceAccount {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string()),
            parent_user: Some("admin".to_string()),
            policy: None,
            account_status: Some("on".to_string()),
            expiration: None,
            name: None,
            description: None,
            implied_policy: None,
        };

        let info = ServiceAccountInfo::from(sa);
        assert_eq!(info.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert!(info.secret_key.is_some());
        assert_eq!(info.parent_user, Some("admin".to_string()));
    }

    #[test]
    fn test_build_create_request_uses_access_key_as_default_name() {
        let args = CreateArgs {
            alias: "local".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            name: None,
            description: None,
            policy: None,
            expiry: None,
        };

        let request = build_create_service_account_request(&args, None);
        assert_eq!(request.name.as_deref(), Some("AKIAIOSFODNN7EXAMPLE"));
        assert!(request.expiry.is_none());
    }

    #[test]
    fn test_build_create_request_keeps_explicit_name() {
        let args = CreateArgs {
            alias: "local".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            name: Some("manual-name".to_string()),
            description: Some("demo".to_string()),
            policy: None,
            expiry: Some("2030-01-01T00:00:00Z".to_string()),
        };

        let request = build_create_service_account_request(&args, Some("{\"x\":1}".to_string()));
        assert_eq!(request.name.as_deref(), Some("manual-name"));
        assert_eq!(request.description.as_deref(), Some("demo"));
        assert_eq!(request.expiry.as_deref(), Some("2030-01-01T00:00:00Z"));
        assert_eq!(request.policy.as_deref(), Some("{\"x\":1}"));
    }

    #[test]
    fn test_build_update_request_keeps_only_explicit_fields() {
        let args = UpdateArgs {
            alias: "local".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: None,
            name: Some("automation-key".to_string()),
            description: Some("Updated description".to_string()),
            policy: Some("policy.json".to_string()),
            expiry: None,
            status: Some("enabled".to_string()),
        };

        let request = build_update_service_account_request(
            &args,
            Some("{\"Version\":\"2012-10-17\"}".to_string()),
        );
        assert_eq!(
            request.new_policy.as_deref(),
            Some("{\"Version\":\"2012-10-17\"}")
        );
        assert!(request.new_secret_key.is_none());
        assert_eq!(request.new_name.as_deref(), Some("automation-key"));
        assert_eq!(
            request.new_description.as_deref(),
            Some("Updated description")
        );
        assert!(request.new_expiration.is_none());
        assert_eq!(request.new_status.as_deref(), Some("enabled"));
        assert!(!request.is_empty());
    }

    #[test]
    fn test_build_update_request_detects_empty_update() {
        let args = UpdateArgs {
            alias: "local".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: None,
            name: None,
            description: None,
            policy: None,
            expiry: None,
            status: None,
        };

        let request = build_update_service_account_request(&args, None);
        assert!(request.is_empty());
    }

    #[test]
    fn test_should_retry_with_default_expiry_for_missing_field_error() {
        let args = CreateArgs {
            alias: "local".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            name: None,
            description: None,
            policy: None,
            expiry: None,
        };
        let error = rc_core::Error::InvalidPath("missing field `expiration`".to_string());
        assert!(should_retry_with_default_expiry(&args, &error));
    }

    #[test]
    fn test_should_not_retry_with_default_expiry_when_expiry_is_explicit() {
        let args = CreateArgs {
            alias: "local".to_string(),
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            name: None,
            description: None,
            policy: None,
            expiry: Some("2030-01-01T00:00:00Z".to_string()),
        };
        let error = rc_core::Error::InvalidPath("missing field `expiration`".to_string());
        assert!(!should_retry_with_default_expiry(&args, &error));
    }
}
