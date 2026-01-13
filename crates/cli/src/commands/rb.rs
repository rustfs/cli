//! rb command - Remove bucket
//!
//! Removes a bucket from the specified storage service.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Remove a bucket
#[derive(Args, Debug)]
pub struct RbArgs {
    /// Target path (alias/bucket)
    pub target: String,

    /// Force remove even if bucket is not empty (deletes all objects first)
    #[arg(long)]
    pub force: bool,

    /// Remove bucket even if it has incomplete multipart uploads
    #[arg(long)]
    pub dangerous: bool,
}

#[derive(Debug, Serialize)]
struct RbOutput {
    status: &'static str,
    bucket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Execute the rb command
pub async fn execute(args: RbArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse the target path
    let (alias_name, bucket) = match parse_rb_path(&args.target) {
        Ok(parsed) => parsed,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    // Load alias
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&alias_name) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            return ExitCode::NotFound;
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create S3 client: {e}"));
            return ExitCode::NetworkError;
        }
    };

    // Check if bucket exists
    match client.bucket_exists(&bucket).await {
        Ok(false) => {
            formatter.error(&format!("Bucket '{alias_name}/{bucket}' does not exist"));
            return ExitCode::NotFound;
        }
        Ok(true) => {}
        Err(e) => {
            formatter.error(&format!("Failed to check bucket existence: {e}"));
            return ExitCode::NetworkError;
        }
    }

    // TODO: If --force is specified, delete all objects first
    // This will be implemented in Phase 3 when we have delete_object

    // Delete the bucket
    match client.delete_bucket(&bucket).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = RbOutput {
                    status: "success",
                    bucket: bucket.clone(),
                    message: None,
                };
                formatter.json(&output);
            } else {
                formatter.success(&format!(
                    "Bucket '{alias_name}/{bucket}' removed successfully."
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("BucketNotEmpty") {
                if args.force {
                    formatter.error(&format!(
                        "Bucket '{alias_name}/{bucket}' is not empty. --force with object deletion not yet implemented."
                    ));
                } else {
                    formatter.error(&format!(
                        "Bucket '{alias_name}/{bucket}' is not empty. Use --force to delete all objects first."
                    ));
                }
                ExitCode::Conflict
            } else if err_str.contains("NoSuchBucket") || err_str.contains("NotFound") {
                formatter.error(&format!("Bucket '{alias_name}/{bucket}' does not exist"));
                ExitCode::NotFound
            } else if err_str.contains("AccessDenied") {
                formatter.error(&format!(
                    "Access denied: cannot remove bucket '{alias_name}/{bucket}'"
                ));
                ExitCode::AuthError
            } else {
                formatter.error(&format!("Failed to remove bucket: {e}"));
                ExitCode::NetworkError
            }
        }
    }
}

/// Parse rb target path into (alias, bucket)
fn parse_rb_path(path: &str) -> Result<(String, String), String> {
    let path = path.trim_end_matches('/');

    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(2, '/').collect();

    if parts.len() != 2 {
        return Err(format!(
            "Invalid path format: '{path}'. Expected: alias/bucket"
        ));
    }

    let alias = parts[0].to_string();
    let bucket = parts[1].to_string();

    if bucket.is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }

    Ok((alias, bucket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rb_path_valid() {
        let (alias, bucket) = parse_rb_path("minio/mybucket").unwrap();
        assert_eq!(alias, "minio");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_rb_path_trailing_slash() {
        let (alias, bucket) = parse_rb_path("minio/mybucket/").unwrap();
        assert_eq!(alias, "minio");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_rb_path_no_bucket() {
        assert!(parse_rb_path("minio").is_err());
    }

    #[test]
    fn test_parse_rb_path_empty() {
        assert!(parse_rb_path("").is_err());
    }
}
