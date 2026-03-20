//! ilm restore command - Restore a transitioned (archived) object
//!
//! Initiates a restore request for an object that has been transitioned
//! to a remote storage tier via lifecycle rules.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Restore a transitioned object
#[derive(Args, Debug)]
pub struct RestoreArgs {
    /// Object path (alias/bucket/key)
    pub path: String,

    /// Number of days to keep the restored copy
    #[arg(long, default_value = "1")]
    pub days: i32,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct RestoreOutput {
    path: String,
    days: i32,
    status: String,
}

/// Execute the restore command
pub async fn execute(args: RestoreArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket, key) = match parse_object_path(&args.path) {
        Ok(parsed) => parsed,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    if args.days < 1 {
        formatter.error("--days must be at least 1");
        return ExitCode::UsageError;
    }

    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            formatter.error(&format!("Failed to load aliases: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            return ExitCode::NotFound;
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            formatter.error(&format!("Failed to create S3 client: {error}"));
            return ExitCode::NetworkError;
        }
    };

    if !args.force {
        match client.capabilities().await {
            Ok(caps) => {
                if !caps.lifecycle {
                    formatter.error(
                        "Backend does not support lifecycle. Use --force to attempt anyway.",
                    );
                    return ExitCode::UnsupportedFeature;
                }
            }
            Err(error) => {
                formatter.error(&format!("Failed to detect capabilities: {error}"));
                return ExitCode::NetworkError;
            }
        }
    }

    let remote_path = RemotePath::new(&alias_name, &bucket, &key);

    match client.restore_object(&remote_path, args.days).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&RestoreOutput {
                    path: args.path,
                    days: args.days,
                    status: "initiated".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Restore initiated for '{}' ({} day(s)).",
                    args.path, args.days
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to restore object: {error}"));
            ExitCode::GeneralError
        }
    }
}

fn parse_object_path(path: &str) -> Result<(String, String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();
    if parts.len() < 3 || parts[0].is_empty() || parts[1].is_empty() || parts[2].is_empty() {
        return Err("Object path must be in format alias/bucket/key".to_string());
    }

    Ok((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_object_path_success() {
        let (alias, bucket, key) =
            parse_object_path("local/my-bucket/my-key.txt").expect("should parse");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, "my-key.txt");
    }

    #[test]
    fn test_parse_object_path_nested_key() {
        let (alias, bucket, key) =
            parse_object_path("local/my-bucket/path/to/file.txt").expect("should parse");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_object_path_errors() {
        assert!(parse_object_path("").is_err());
        assert!(parse_object_path("local").is_err());
        assert!(parse_object_path("local/bucket").is_err());
        assert!(parse_object_path("/bucket/key").is_err());
        assert!(parse_object_path("local//key").is_err());
        assert!(parse_object_path("local/bucket/").is_err());
    }

    #[tokio::test]
    async fn test_execute_invalid_path_returns_usage_error() {
        let args = RestoreArgs {
            path: "invalid-path".to_string(),
            days: 1,
            force: false,
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn test_execute_invalid_days_returns_usage_error() {
        let args = RestoreArgs {
            path: "local/bucket/key.txt".to_string(),
            days: 0,
            force: false,
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
