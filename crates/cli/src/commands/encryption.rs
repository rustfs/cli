//! encryption command - Manage bucket default encryption.
//!
//! Set, inspect, or clear bucket default encryption for a bucket path.

use clap::{Args, Subcommand, ValueEnum};
use rc_core::{AliasManager, BucketEncryption, ObjectStore as _};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const ENCRYPTION_AFTER_HELP: &str = "\
Examples:
  rc bucket encryption set local/my-bucket --mode sse-s3
  rc bucket encryption set local/my-bucket --mode sse-kms
  rc bucket encryption set local/my-bucket --mode sse-kms --key-id alias/my-key
  rc bucket encryption info local/my-bucket
  rc bucket encryption clear local/my-bucket";

const SET_AFTER_HELP: &str = "\
Examples:
  rc bucket encryption set local/my-bucket --mode sse-s3
  rc bucket encryption set local/my-bucket --mode sse-kms
  rc bucket encryption set local/my-bucket --mode sse-kms --key-id alias/my-key";

const INFO_AFTER_HELP: &str = "\
Examples:
  rc bucket encryption info local/my-bucket";

const CLEAR_AFTER_HELP: &str = "\
Examples:
  rc bucket encryption clear local/my-bucket";

/// Manage bucket default encryption
#[derive(Args, Debug)]
#[command(after_help = ENCRYPTION_AFTER_HELP)]
pub struct EncryptionArgs {
    #[command(subcommand)]
    pub command: EncryptionCommands,
}

#[derive(Subcommand, Debug)]
pub enum EncryptionCommands {
    /// Set bucket default encryption
    Set(SetEncryptionArgs),

    /// Show bucket default encryption
    Info(InfoEncryptionArgs),

    /// Clear bucket default encryption
    Clear(ClearEncryptionArgs),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum EncryptionMode {
    SseS3,
    SseKms,
}

#[derive(Args, Debug)]
#[command(after_help = SET_AFTER_HELP)]
pub struct SetEncryptionArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Default encryption mode
    #[arg(long)]
    pub mode: EncryptionMode,

    /// Optional KMS key id for sse-kms mode
    #[arg(long)]
    pub key_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct BucketArg {
    /// Path to the bucket (alias/bucket)
    pub path: String,
}

#[derive(Args, Debug)]
#[command(after_help = INFO_AFTER_HELP)]
pub struct InfoEncryptionArgs {
    #[command(flatten)]
    pub bucket: BucketArg,
}

#[derive(Args, Debug)]
#[command(after_help = CLEAR_AFTER_HELP)]
pub struct ClearEncryptionArgs {
    #[command(flatten)]
    pub bucket: BucketArg,
}

#[derive(Debug, Serialize)]
struct BucketEncryptionOutput {
    bucket: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kms_key_id: Option<String>,
}

/// Execute the encryption command
pub async fn execute(args: EncryptionArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        EncryptionCommands::Set(args) => execute_set(args, output_config).await,
        EncryptionCommands::Info(args) => execute_info(args, output_config).await,
        EncryptionCommands::Clear(args) => execute_clear(args, output_config).await,
    }
}

async fn execute_set(args: SetEncryptionArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the encryption command.",
            );
        }
    };

    let encryption = match validate_set_encryption_args(&args, &formatter) {
        Ok(encryption) => encryption,
        Err(code) => return code,
    };

    let client = match setup_client(&alias_name, &bucket, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client
        .set_bucket_encryption(&bucket, encryption.clone())
        .await
    {
        Ok(()) => {
            let output = output_for_encryption(bucket, Some(encryption), "Not configured");
            if formatter.is_json() {
                formatter.json(&output);
            } else {
                formatter.println(&format!("Bucket: {}", output.bucket));
                formatter.println(&format!("Encryption: {}", output.status));
                if let Some(mode) = output.mode {
                    formatter.println(&format!("Mode: {mode}"));
                }
                if let Some(key_id) = output.kms_key_id {
                    formatter.println(&format!("KMS Key ID: {key_id}"));
                }
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            exit_code_from_error(&error),
            &format!("Failed to set bucket encryption: {error}"),
        ),
    }
}

fn validate_set_encryption_args(
    args: &SetEncryptionArgs,
    formatter: &Formatter,
) -> Result<BucketEncryption, ExitCode> {
    match (args.mode, args.key_id.as_deref()) {
        (EncryptionMode::SseS3, None) => Ok(BucketEncryption::SseS3),
        (EncryptionMode::SseS3, Some(_)) => Err(formatter.fail(
            ExitCode::UsageError,
            "--key-id is only valid with --mode sse-kms",
        )),
        (EncryptionMode::SseKms, key_id) => Ok(BucketEncryption::SseKms {
            key_id: key_id.map(ToString::to_string),
        }),
    }
}

async fn execute_info(args: InfoEncryptionArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.bucket.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the encryption command.",
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_encryption(&bucket).await {
        Ok(encryption) => {
            let output = output_for_encryption(bucket, encryption, "Not configured");
            if formatter.is_json() {
                formatter.json(&output);
            } else {
                formatter.println(&format!("Bucket: {}", output.bucket));
                formatter.println(&format!("Encryption: {}", output.status));
                if let Some(mode) = output.mode {
                    formatter.println(&format!("Mode: {mode}"));
                }
                if let Some(key_id) = output.kms_key_id {
                    formatter.println(&format!("KMS Key ID: {key_id}"));
                }
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            exit_code_from_error(&error),
            &format!("Failed to get bucket encryption: {error}"),
        ),
    }
}

async fn execute_clear(args: ClearEncryptionArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.bucket.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the encryption command.",
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.delete_bucket_encryption(&bucket).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&BucketEncryptionOutput {
                    bucket,
                    status: "Cleared".to_string(),
                    mode: None,
                    kms_key_id: None,
                });
            } else {
                formatter.success("Bucket encryption configuration cleared successfully.");
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            exit_code_from_error(&error),
            &format!("Failed to clear bucket encryption: {error}"),
        ),
    }
}

async fn setup_client(
    alias_name: &str,
    bucket: &str,
    formatter: &Formatter,
) -> Result<S3Client, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
            ));
        }
    };

    let alias = match alias_manager.get(alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            return Err(formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{alias_name}' not found"),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            ));
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {error}"),
            ));
        }
    };

    match client.bucket_exists(bucket).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Bucket '{bucket}' does not exist"),
                "Check the bucket path and retry the encryption command.",
            ));
        }
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to check bucket: {error}"),
            ));
        }
    }

    Ok(client)
}

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    if path.is_empty() {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }

    let parts: Vec<&str> = path.splitn(2, '/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }

    let bucket = parts[1].trim_end_matches('/');
    if bucket.is_empty() || bucket.contains('/') {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }

    Ok((parts[0].to_string(), bucket.to_string()))
}

fn output_for_encryption(
    bucket: String,
    encryption: Option<BucketEncryption>,
    empty_status: &str,
) -> BucketEncryptionOutput {
    match encryption {
        Some(BucketEncryption::SseS3) => BucketEncryptionOutput {
            bucket,
            status: "Configured".to_string(),
            mode: Some("SSE-S3".to_string()),
            kms_key_id: None,
        },
        Some(BucketEncryption::SseKms { key_id }) => BucketEncryptionOutput {
            bucket,
            status: "Configured".to_string(),
            mode: Some("SSE-KMS".to_string()),
            kms_key_id: key_id,
        },
        None => BucketEncryptionOutput {
            bucket,
            status: empty_status.to_string(),
            mode: None,
            kms_key_id: None,
        },
    }
}

fn exit_code_from_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bucket_path_rejects_object_paths() {
        assert!(parse_bucket_path("local/bucket/object.txt").is_err());
    }

    #[test]
    fn validate_set_kms_without_key_id_uses_default_kms_key() {
        let args = SetEncryptionArgs {
            path: "local/my-bucket".to_string(),
            mode: EncryptionMode::SseKms,
            key_id: None,
        };
        let formatter = Formatter::new(OutputConfig::default());

        let encryption = validate_set_encryption_args(&args, &formatter)
            .expect("sse-kms without key id should use the server default KMS key");

        assert_eq!(encryption, BucketEncryption::SseKms { key_id: None });
    }

    #[tokio::test]
    async fn execute_set_invalid_bucket_path_returns_usage_error() {
        let args = EncryptionArgs {
            command: EncryptionCommands::Set(SetEncryptionArgs {
                path: "local/my-bucket/object.txt".to_string(),
                mode: EncryptionMode::SseS3,
                key_id: None,
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn execute_set_sse_s3_with_key_id_returns_usage_error() {
        let args = EncryptionArgs {
            command: EncryptionCommands::Set(SetEncryptionArgs {
                path: "local/my-bucket".to_string(),
                mode: EncryptionMode::SseS3,
                key_id: Some("kms-key".to_string()),
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn execute_info_invalid_bucket_path_returns_usage_error() {
        let args = EncryptionArgs {
            command: EncryptionCommands::Info(InfoEncryptionArgs {
                bucket: BucketArg {
                    path: "local/my-bucket/object.txt".to_string(),
                },
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn execute_clear_invalid_bucket_path_returns_usage_error() {
        let args = EncryptionArgs {
            command: EncryptionCommands::Clear(ClearEncryptionArgs {
                bucket: BucketArg {
                    path: "local/my-bucket/object.txt".to_string(),
                },
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
