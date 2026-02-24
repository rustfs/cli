//! quota command - Manage bucket quotas
//!
//! Set, inspect, or clear quota on a bucket.

use clap::{Args, Subcommand};
use rc_core::admin::{AdminApi, BucketQuota};
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

use super::admin::get_admin_client;

/// Manage bucket quota
#[derive(Args, Debug)]
pub struct QuotaArgs {
    #[command(subcommand)]
    pub command: QuotaCommands,
}

#[derive(Subcommand, Debug)]
pub enum QuotaCommands {
    /// Set bucket quota
    Set(SetQuotaArgs),

    /// Show bucket quota information
    Info(BucketArg),

    /// Clear bucket quota
    Clear(BucketArg),
}

#[derive(Args, Debug)]
pub struct BucketArg {
    /// Bucket path (alias/bucket)
    pub path: String,
}

#[derive(Args, Debug)]
pub struct SetQuotaArgs {
    /// Bucket path (alias/bucket)
    pub path: String,

    /// Quota value (bytes or units like 1G, 500M, 10KB)
    pub size: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaOutput {
    bucket: String,
    quota: Option<u64>,
    quota_human: Option<String>,
    usage: u64,
    usage_human: String,
    quota_type: String,
}

/// Execute the quota command
pub async fn execute(args: QuotaArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        QuotaCommands::Set(set_args) => execute_set(set_args, output_config).await,
        QuotaCommands::Info(bucket_arg) => execute_info(bucket_arg, output_config).await,
        QuotaCommands::Clear(bucket_arg) => execute_clear(bucket_arg, output_config).await,
    }
}

async fn execute_set(args: SetQuotaArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(err) => {
            formatter.error(&err);
            return ExitCode::UsageError;
        }
    };

    let quota_bytes = match parse_quota_size(&args.size) {
        Ok(size) => size,
        Err(err) => {
            formatter.error(&err);
            return ExitCode::UsageError;
        }
    };

    let client = match get_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.set_bucket_quota(&bucket, quota_bytes).await {
        Ok(quota) => {
            print_quota_result(&formatter, &quota);
            ExitCode::Success
        }
        Err(err) => {
            formatter.error(&format!("Failed to set bucket quota: {err}"));
            exit_code_from_error(&err)
        }
    }
}

async fn execute_info(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(err) => {
            formatter.error(&err);
            return ExitCode::UsageError;
        }
    };

    let client = match get_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_quota(&bucket).await {
        Ok(quota) => {
            print_quota_result(&formatter, &quota);
            ExitCode::Success
        }
        Err(err) => {
            formatter.error(&format!("Failed to get bucket quota: {err}"));
            exit_code_from_error(&err)
        }
    }
}

async fn execute_clear(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(err) => {
            formatter.error(&err);
            return ExitCode::UsageError;
        }
    };

    let client = match get_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.clear_bucket_quota(&bucket).await {
        Ok(quota) => {
            print_quota_result(&formatter, &quota);
            ExitCode::Success
        }
        Err(err) => {
            formatter.error(&format!("Failed to clear bucket quota: {err}"));
            exit_code_from_error(&err)
        }
    }
}

fn print_quota_result(formatter: &Formatter, quota: &BucketQuota) {
    if formatter.is_json() {
        formatter.json(&QuotaOutput {
            bucket: quota.bucket.clone(),
            quota: quota.quota,
            quota_human: quota.quota.map(format_human_size),
            usage: quota.size,
            usage_human: format_human_size(quota.size),
            quota_type: quota.quota_type.clone(),
        });
        return;
    }

    formatter.println(&format!("Bucket: {}", quota.bucket));
    let limit_text = quota
        .quota
        .map(format_human_size)
        .unwrap_or_else(|| "unlimited".to_string());
    formatter.println(&format!("Quota: {limit_text}"));
    formatter.println(&format!("Usage: {}", format_human_size(quota.size)));
    formatter.println(&format!("Type:  {}", quota.quota_type));
}

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    if path.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(2, '/').collect();

    if parts.len() < 2 || parts[0].is_empty() {
        return Err("Alias name is required (alias/bucket)".to_string());
    }

    let bucket = parts[1].trim_end_matches('/');
    if bucket.is_empty() {
        return Err("Bucket name is required (alias/bucket)".to_string());
    }

    Ok((parts[0].to_string(), bucket.to_string()))
}

fn parse_quota_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Quota size cannot be empty".to_string());
    }

    let split_index = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());

    let (number_part, unit_part) = value.split_at(split_index);
    if number_part.is_empty() {
        return Err(format!("Invalid quota size: '{value}'"));
    }

    let number = number_part
        .parse::<u64>()
        .map_err(|_| format!("Invalid quota size number: '{number_part}'"))?;

    let multiplier = match unit_part.trim().to_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TIB" => 1024_u64.pow(4),
        _ => return Err(format!("Invalid quota size unit: '{unit_part}'")),
    };

    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("Quota size is too large: '{value}'"))
}

fn format_human_size(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

fn exit_code_from_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bucket_path() {
        let (alias, bucket) = parse_bucket_path("local/my-bucket").unwrap();
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");

        let (alias, bucket) = parse_bucket_path("local/my-bucket/").unwrap();
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");
    }

    #[test]
    fn test_parse_bucket_path_errors() {
        assert!(parse_bucket_path("").is_err());
        assert!(parse_bucket_path("local").is_err());
        assert!(parse_bucket_path("/my-bucket").is_err());
        assert!(parse_bucket_path("local/").is_err());
    }

    #[test]
    fn test_parse_quota_size() {
        assert_eq!(parse_quota_size("1024").unwrap(), 1024);
        assert_eq!(parse_quota_size("1K").unwrap(), 1024);
        assert_eq!(parse_quota_size("1KB").unwrap(), 1024);
        assert_eq!(parse_quota_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_quota_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_quota_size_errors() {
        assert!(parse_quota_size("").is_err());
        assert!(parse_quota_size("abc").is_err());
        assert!(parse_quota_size("1X").is_err());
    }

    #[tokio::test]
    async fn test_execute_set_invalid_path_returns_usage_error() {
        let args = QuotaArgs {
            command: QuotaCommands::Set(SetQuotaArgs {
                path: "invalid-path".to_string(),
                size: "1G".to_string(),
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn test_execute_set_invalid_size_returns_usage_error() {
        let args = QuotaArgs {
            command: QuotaCommands::Set(SetQuotaArgs {
                path: "local/my-bucket".to_string(),
                size: "1X".to_string(),
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
