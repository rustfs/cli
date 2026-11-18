//! public-access-block command - Manage bucket-level public access controls.

use clap::{Args, Subcommand};
use rc_core::{AliasManager, ObjectStore as _, PublicAccessBlockConfiguration};
use rc_s3::S3Client;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const AFTER_HELP: &str = "\
Examples:
  rc bucket public-access-block get local/my-bucket
  rc bucket public-access-block set local/my-bucket public-access-block.json
  rc bucket public-access-block set local/my-bucket -
  rc bucket public-access-block remove local/my-bucket --yes";

/// Manage S3 Public Access Block settings for one bucket.
#[derive(Args, Debug)]
pub struct PublicAccessBlockArgs {
    #[command(subcommand)]
    pub command: PublicAccessBlockCommands,
}

#[derive(Subcommand, Debug)]
#[command(after_help = AFTER_HELP)]
pub enum PublicAccessBlockCommands {
    /// Show the current Public Access Block settings.
    Get(BucketArg),
    /// Replace the Public Access Block settings from a JSON file or stdin.
    Set(SetArgs),
    /// Remove Public Access Block settings; --yes is required because this can allow public access.
    Remove(RemoveArgs),
}

#[derive(Args, Debug)]
pub struct BucketArg {
    /// Bucket path (alias/bucket).
    pub path: String,
}

#[derive(Args, Debug)]
pub struct SetArgs {
    /// Bucket path (alias/bucket).
    pub path: String,
    /// JSON configuration file, or '-' to read from stdin.
    pub source: String,
}

#[derive(Args, Debug)]
pub struct RemoveArgs {
    /// Bucket path (alias/bucket).
    pub path: String,
    /// Confirm removal of the public access protection.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Serialize)]
struct ConfigurationOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: ConfigurationData,
}

#[derive(Debug, Serialize)]
struct ConfigurationData {
    bucket: String,
    operation: &'static str,
    configuration: Option<PublicAccessBlockConfiguration>,
}

/// Execute a Public Access Block command.
pub async fn execute(args: PublicAccessBlockArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        PublicAccessBlockCommands::Get(args) => execute_get(args, output_config).await,
        PublicAccessBlockCommands::Set(args) => execute_set(args, output_config).await,
        PublicAccessBlockCommands::Remove(args) => execute_remove(args, output_config).await,
    }
}

async fn execute_get(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(path) => path,
        Err(error) => return fail_message(&formatter, ExitCode::UsageError, error, None),
    };
    let client = match setup_client(&alias_name, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_public_access_block(&bucket).await {
        Ok(configuration) => {
            if formatter.is_json() {
                formatter.json(&success_output(bucket, "get", configuration));
            } else if let Some(configuration) = configuration {
                formatter.println(&format!("Public Access Block for '{bucket}':"));
                formatter.println(&format!(
                    "  Block public ACLs: {}",
                    configuration.block_public_acls
                ));
                formatter.println(&format!(
                    "  Ignore public ACLs: {}",
                    configuration.ignore_public_acls
                ));
                formatter.println(&format!(
                    "  Block public policies: {}",
                    configuration.block_public_policy
                ));
                formatter.println(&format!(
                    "  Restrict public buckets: {}",
                    configuration.restrict_public_buckets
                ));
            } else {
                formatter.println("No Public Access Block configuration is set.");
            }
            ExitCode::Success
        }
        Err(error) => super::ops_output::emit_error(
            &formatter,
            "public_access_block",
            "Failed to get Public Access Block",
            &error,
            None,
            None,
            None,
        ),
    }
}

async fn execute_set(args: SetArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(path) => path,
        Err(error) => return fail_message(&formatter, ExitCode::UsageError, error, None),
    };
    let contents = match read_source(&args.source).await {
        Ok(contents) => contents,
        Err(error) => {
            return super::ops_output::emit_message(
                &formatter,
                "public_access_block",
                ExitCode::GeneralError,
                format!("Failed to read Public Access Block configuration: {error}"),
                None,
            );
        }
    };
    let configuration = match serde_json::from_str::<PublicAccessBlockConfiguration>(&contents) {
        Ok(configuration) => configuration,
        Err(error) => {
            return super::ops_output::emit_message(
                &formatter,
                "public_access_block",
                ExitCode::UsageError,
                format!("Invalid Public Access Block configuration: {error}"),
                None,
            );
        }
    };
    let client = match setup_client(&alias_name, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client
        .set_bucket_public_access_block(&bucket, configuration)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&success_output(bucket, "set", Some(configuration)));
            } else {
                formatter.success("Public Access Block configuration applied.");
            }
            ExitCode::Success
        }
        Err(error) => super::ops_output::emit_error(
            &formatter,
            "public_access_block",
            "Failed to set Public Access Block",
            &error,
            None,
            None,
            None,
        ),
    }
}

async fn execute_remove(args: RemoveArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    if !args.yes {
        return fail_message(
            &formatter,
            ExitCode::UsageError,
            "Removing Public Access Block can expose a bucket to public policies.".to_string(),
            Some("Review the bucket policy, then repeat with --yes to confirm removal."),
        );
    }
    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(path) => path,
        Err(error) => return fail_message(&formatter, ExitCode::UsageError, error, None),
    };
    let client = match setup_client(&alias_name, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.delete_bucket_public_access_block(&bucket).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&success_output(bucket, "remove", None));
            } else {
                formatter.success("Public Access Block configuration removed.");
            }
            ExitCode::Success
        }
        Err(error) => super::ops_output::emit_error(
            &formatter,
            "public_access_block",
            "Failed to remove Public Access Block",
            &error,
            None,
            None,
            None,
        ),
    }
}

fn success_output(
    bucket: String,
    operation: &'static str,
    configuration: Option<PublicAccessBlockConfiguration>,
) -> ConfigurationOutput {
    ConfigurationOutput {
        schema_version: 3,
        output_type: "public_access_block",
        status: "success",
        data: ConfigurationData {
            bucket,
            operation,
            configuration,
        },
    }
}

async fn setup_client(alias_name: &str, formatter: &Formatter) -> Result<S3Client, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return Err(fail_message(
                formatter,
                ExitCode::GeneralError,
                format!("Failed to load aliases: {error}"),
                None,
            ));
        }
    };
    let alias = match alias_manager.get(alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            return Err(fail_message(
                formatter,
                ExitCode::NotFound,
                format!("Alias '{alias_name}' not found"),
                Some("Run `rc alias list` or add one with `rc alias set ...`."),
            ));
        }
    };
    S3Client::new(alias).await.map_err(|error| {
        fail_message(
            formatter,
            ExitCode::NetworkError,
            format!("Failed to create S3 client: {error}"),
            None,
        )
    })
}

fn fail_message(
    formatter: &Formatter,
    code: ExitCode,
    message: String,
    suggestion: Option<&str>,
) -> ExitCode {
    super::ops_output::emit_message(formatter, "public_access_block", code, message, suggestion)
}

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    let (alias, bucket) = path
        .split_once('/')
        .ok_or_else(|| "Bucket path must be in format alias/bucket".to_string())?;
    let bucket = bucket.trim_end_matches('/');
    if alias.trim().is_empty() || bucket.is_empty() || bucket.contains('/') {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }
    Ok((alias.to_string(), bucket.to_string()))
}

async fn read_source(source: &str) -> Result<String, String> {
    if source == "-" {
        let mut bytes = Vec::new();
        tokio::io::stdin()
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        String::from_utf8(bytes).map_err(|error| error.to_string())
    } else {
        tokio::fs::read_to_string(source)
            .await
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bucket_path() {
        assert_eq!(
            parse_bucket_path("local/raw/"),
            Ok(("local".to_string(), "raw".to_string()))
        );
    }

    #[test]
    fn rejects_invalid_bucket_paths() {
        for path in ["", "local", "/bucket", "local/", "local/bucket/prefix"] {
            assert!(parse_bucket_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn parses_complete_public_access_block_json() {
        let configuration: PublicAccessBlockConfiguration = serde_json::from_value(json!({
            "block_public_acls": true,
            "ignore_public_acls": true,
            "block_public_policy": true,
            "restrict_public_buckets": true
        }))
        .expect("complete configuration");
        assert_eq!(
            configuration,
            PublicAccessBlockConfiguration::secure_default()
        );
    }

    #[tokio::test]
    async fn remove_requires_confirmation_before_network_setup() {
        let code = execute(
            PublicAccessBlockArgs {
                command: PublicAccessBlockCommands::Remove(RemoveArgs {
                    path: "local/raw".to_string(),
                    yes: false,
                }),
            },
            OutputConfig::default(),
        )
        .await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn set_rejects_configuration_missing_a_flag_before_network_setup() {
        let source = tempfile::NamedTempFile::new().expect("temporary config file");
        std::fs::write(
            source.path(),
            r#"{"block_public_acls":true,"ignore_public_acls":true,"block_public_policy":true}"#,
        )
        .expect("write incomplete configuration");
        let code = execute(
            PublicAccessBlockArgs {
                command: PublicAccessBlockCommands::Set(SetArgs {
                    path: "local/raw".to_string(),
                    source: source.path().display().to_string(),
                }),
            },
            OutputConfig::default(),
        )
        .await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
