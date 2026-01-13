//! Alias management commands
//!
//! Aliases are named references to S3-compatible storage endpoints,
//! including connection details and credentials.

use clap::Subcommand;
use serde::Serialize;

use crate::exit_code::ExitCode;
use rc_core::{Alias, AliasManager};

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

    /// S3 endpoint URL (e.g., "http://localhost:9000", "https://s3.amazonaws.com")
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
pub async fn execute(cmd: AliasCommands, json_output: bool) -> ExitCode {
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            if json_output {
                eprintln!("{}", serde_json::json!({"error": e.to_string()}));
            } else {
                eprintln!("Error: {e}");
            }
            return ExitCode::GeneralError;
        }
    };

    match cmd {
        AliasCommands::Set(args) => execute_set(args, &alias_manager, json_output).await,
        AliasCommands::List(args) => execute_list(args, &alias_manager, json_output).await,
        AliasCommands::Remove(args) => execute_remove(args, &alias_manager, json_output).await,
    }
}

async fn execute_set(args: SetArgs, manager: &AliasManager, json_output: bool) -> ExitCode {
    // Validate inputs
    if args.name.is_empty() {
        let msg = "Alias name cannot be empty";
        if json_output {
            eprintln!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("Error: {msg}");
        }
        return ExitCode::UsageError;
    }

    if args.endpoint.is_empty() {
        let msg = "Endpoint URL cannot be empty";
        if json_output {
            eprintln!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("Error: {msg}");
        }
        return ExitCode::UsageError;
    }

    // Validate signature version
    if args.signature != "v4" && args.signature != "v2" {
        let msg = "Signature must be 'v4' or 'v2'";
        if json_output {
            eprintln!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("Error: {msg}");
        }
        return ExitCode::UsageError;
    }

    // Validate bucket lookup
    if args.bucket_lookup != "auto" && args.bucket_lookup != "path" && args.bucket_lookup != "dns" {
        let msg = "Bucket lookup must be 'auto', 'path', or 'dns'";
        if json_output {
            eprintln!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("Error: {msg}");
        }
        return ExitCode::UsageError;
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
            if json_output {
                let output = AliasOperationOutput {
                    success: true,
                    alias: args.name.clone(),
                    message: format!("Alias '{}' configured successfully", args.name),
                };
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Alias '{}' configured successfully.", args.name);
            }
            ExitCode::Success
        }
        Err(e) => {
            if json_output {
                eprintln!("{}", serde_json::json!({"error": e.to_string()}));
            } else {
                eprintln!("Error: {e}");
            }
            ExitCode::GeneralError
        }
    }
}

async fn execute_list(args: ListArgs, manager: &AliasManager, json_output: bool) -> ExitCode {
    match manager.list() {
        Ok(aliases) => {
            if json_output {
                let output = AliasListOutput {
                    aliases: aliases.iter().map(AliasInfo::from).collect(),
                };
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else if aliases.is_empty() {
                println!("No aliases configured.");
            } else if args.long {
                // Long format with details
                for alias in &aliases {
                    println!(
                        "{:<12} {} (region: {}, lookup: {})",
                        alias.name, alias.endpoint, alias.region, alias.bucket_lookup
                    );
                }
            } else {
                // Short format
                for alias in &aliases {
                    println!("{:<12} {}", alias.name, alias.endpoint);
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            if json_output {
                eprintln!("{}", serde_json::json!({"error": e.to_string()}));
            } else {
                eprintln!("Error: {e}");
            }
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: RemoveArgs, manager: &AliasManager, json_output: bool) -> ExitCode {
    match manager.remove(&args.name) {
        Ok(()) => {
            if json_output {
                let output = AliasOperationOutput {
                    success: true,
                    alias: args.name.clone(),
                    message: format!("Alias '{}' removed successfully", args.name),
                };
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Alias '{}' removed successfully.", args.name);
            }
            ExitCode::Success
        }
        Err(rc_core::Error::AliasNotFound(_)) => {
            if json_output {
                eprintln!(
                    "{}",
                    serde_json::json!({"error": format!("Alias '{}' not found", args.name)})
                );
            } else {
                eprintln!("Error: Alias '{}' not found.", args.name);
            }
            ExitCode::NotFound
        }
        Err(e) => {
            if json_output {
                eprintln!("{}", serde_json::json!({"error": e.to_string()}));
            } else {
                eprintln!("Error: {e}");
            }
            ExitCode::GeneralError
        }
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
}
