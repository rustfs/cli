//! Alias management commands
//!
//! Aliases are named references to S3-compatible storage endpoints,
//! including connection details and credentials.

use clap::Subcommand;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};
use rc_core::{Alias, AliasManager, validate_alias_endpoint};

/// Alias subcommands for managing storage service connections
#[derive(Subcommand, Debug)]
pub enum AliasCommands {
    /// Add or update an alias
    Set(SetArgs),

    /// List all configured aliases
    List(ListArgs),

    /// Remove an alias
    Remove(RemoveArgs),
}

/// Arguments for the `alias set` command
#[derive(clap::Args, Debug)]
pub struct SetArgs {
    /// Alias name (e.g., "local", "s3", "rustfs")
    pub name: String,

    /// S3 endpoint URL (e.g., `http://localhost:9000`, `https://s3.amazonaws.com`)
    pub endpoint: String,

    /// Access key ID
    pub access_key: String,

    /// Secret access key
    pub secret_key: String,

    /// AWS region (default: us-east-1)
    #[arg(long, default_value = "us-east-1")]
    pub region: String,

    /// Signature version: v4 or v2 (default: v4)
    #[arg(long, default_value = "v4")]
    pub signature: String,

    /// Bucket lookup style: auto, path, or dns (default: auto)
    #[arg(long, default_value = "auto")]
    pub bucket_lookup: String,

    /// Allow insecure TLS connections
    #[arg(long, default_value = "false")]
    pub insecure: bool,
}

/// Arguments for the `alias list` command
#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Show full details including endpoints
    #[arg(short, long)]
    pub long: bool,
}

/// Arguments for the `alias remove` command
#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Name of the alias to remove
    pub name: String,
}

/// JSON output for alias list
#[derive(Serialize)]
struct AliasListOutput {
    aliases: Vec<AliasInfo>,
}

/// Alias information for JSON output (without sensitive data)
#[derive(Serialize)]
struct AliasInfo {
    name: String,
    endpoint: String,
    region: String,
    bucket_lookup: String,
}

impl From<&Alias> for AliasInfo {
    fn from(alias: &Alias) -> Self {
        Self {
            name: alias.name.clone(),
            endpoint: alias.endpoint.clone(),
            region: alias.region.clone(),
            bucket_lookup: alias.bucket_lookup.clone(),
        }
    }
}

/// JSON output for alias set/remove operations
#[derive(Serialize)]
struct AliasOperationOutput {
    success: bool,
    alias: String,
    message: String,
}

/// Execute an alias subcommand
pub async fn execute(cmd: AliasCommands, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    match cmd {
        AliasCommands::Set(args) => execute_set(args, &alias_manager, &formatter).await,
        AliasCommands::List(args) => execute_list(args, &alias_manager, &formatter).await,
        AliasCommands::Remove(args) => execute_remove(args, &alias_manager, &formatter).await,
    }
}

async fn execute_set(args: SetArgs, manager: &AliasManager, formatter: &Formatter) -> ExitCode {
    // Validate inputs
    if args.name.is_empty() {
        return formatter.fail(ExitCode::UsageError, "Alias name cannot be empty");
    }

    if args.endpoint.is_empty() {
        return formatter.fail(ExitCode::UsageError, "Endpoint URL cannot be empty");
    }

    if let Err(e) = validate_alias_endpoint(&args.endpoint) {
        return formatter.fail(ExitCode::UsageError, &alias_endpoint_error_message(e));
    }

    // Validate signature version
    if args.signature != "v4" && args.signature != "v2" {
        return formatter.fail(ExitCode::UsageError, "Signature must be 'v4' or 'v2'");
    }

    // Validate bucket lookup
    if args.bucket_lookup != "auto" && args.bucket_lookup != "path" && args.bucket_lookup != "dns" {
        return formatter.fail(
            ExitCode::UsageError,
            "Bucket lookup must be 'auto', 'path', or 'dns'",
        );
    }

    // Create alias
    let mut alias = Alias::new(
        &args.name,
        &args.endpoint,
        &args.access_key,
        &args.secret_key,
    );
    alias.region = args.region;
    alias.signature = args.signature;
    alias.bucket_lookup = args.bucket_lookup;
    alias.insecure = args.insecure;

    // Save alias
    match manager.set(alias) {
        Ok(()) => {
            if formatter.is_json() {
                let output = AliasOperationOutput {
                    success: true,
                    alias: args.name.clone(),
                    message: format!("Alias '{}' configured successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Alias '{styled_name}' configured successfully."));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&e.to_string());
            ExitCode::GeneralError
        }
    }
}

async fn execute_list(args: ListArgs, manager: &AliasManager, formatter: &Formatter) -> ExitCode {
    match manager.list() {
        Ok(aliases) => {
            if formatter.is_json() {
                let output = AliasListOutput {
                    aliases: aliases.iter().map(AliasInfo::from).collect(),
                };
                formatter.json(&output);
            } else if aliases.is_empty() {
                formatter.println("No aliases configured.");
            } else if args.long {
                // Long format with details
                for alias in &aliases {
                    let styled_name = formatter.style_name(&format!("{:<12}", alias.name));
                    let styled_url = formatter.style_url(&alias.endpoint);
                    let styled_region = formatter.style_date(&alias.region);
                    let styled_lookup = formatter.style_date(&alias.bucket_lookup);
                    formatter.println(&format!(
                        "{styled_name} {styled_url} (region: {styled_region}, lookup: {styled_lookup})"
                    ));
                }
            } else {
                // Short format
                for alias in &aliases {
                    let styled_name = formatter.style_name(&format!("{:<12}", alias.name));
                    let styled_url = formatter.style_url(&alias.endpoint);
                    formatter.println(&format!("{styled_name} {styled_url}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&e.to_string());
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(
    args: RemoveArgs,
    manager: &AliasManager,
    formatter: &Formatter,
) -> ExitCode {
    match manager.remove(&args.name) {
        Ok(()) => {
            if formatter.is_json() {
                let output = AliasOperationOutput {
                    success: true,
                    alias: args.name.clone(),
                    message: format!("Alias '{}' removed successfully", args.name),
                };
                formatter.json(&output);
            } else {
                let styled_name = formatter.style_name(&args.name);
                formatter.success(&format!("Alias '{styled_name}' removed successfully."));
            }
            ExitCode::Success
        }
        Err(rc_core::Error::AliasNotFound(_)) => {
            formatter.error(&format!("Alias '{}' not found", args.name));
            ExitCode::NotFound
        }
        Err(e) => {
            formatter.error(&e.to_string());
            ExitCode::GeneralError
        }
    }
}

fn alias_endpoint_error_message(error: rc_core::Error) -> String {
    match error {
        rc_core::Error::Config(message) => message,
        other => format!("Invalid endpoint: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_args_defaults() {
        // Verify default values are applied correctly
        let args = SetArgs {
            name: "test".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            access_key: "accesskey".to_string(),
            secret_key: "secretkey".to_string(),
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
        };

        assert_eq!(args.region, "us-east-1");
        assert_eq!(args.signature, "v4");
        assert_eq!(args.bucket_lookup, "auto");
        assert!(!args.insecure);
    }

    #[test]
    fn test_alias_info_from_alias() {
        let alias = Alias::new("test", "http://localhost:9000", "key", "secret");
        let info = AliasInfo::from(&alias);

        assert_eq!(info.name, "test");
        assert_eq!(info.endpoint, "http://localhost:9000");
        assert_eq!(info.region, "us-east-1");
    }

    #[tokio::test]
    async fn test_execute_set_rejects_invalid_endpoint_url() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let manager = AliasManager::with_config_manager(rc_core::ConfigManager::with_path(
            temp_dir.path().join("config.toml"),
        ));
        let formatter = Formatter::new(OutputConfig::default());
        let args = SetArgs {
            name: "rustfs".to_string(),
            endpoint: "http://rustfs-node{1...32}:9000".to_string(),
            access_key: "accesskey".to_string(),
            secret_key: "secretkey".to_string(),
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
        };

        let exit_code = execute_set(args, &manager, &formatter).await;

        assert_eq!(exit_code, ExitCode::UsageError);
        assert!(manager.get("rustfs").is_err());
    }

    #[test]
    fn test_alias_endpoint_error_message_omits_config_prefix() {
        let message = alias_endpoint_error_message(rc_core::Error::Config(
            "Endpoint must include a host".to_string(),
        ));

        assert_eq!(message, "Endpoint must include a host");
    }
}
