//! mb command - Make bucket
//!
//! Creates a new bucket on the specified storage service.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const MB_AFTER_HELP: &str = "\
Examples:
  rc bucket create local/my-bucket
  rc mb local/my-bucket --ignore-existing
  rc bucket create local/archive --with-versioning --with-lock";

/// Create a bucket
#[derive(Args, Debug)]
#[command(after_help = MB_AFTER_HELP)]
pub struct MbArgs {
    /// Target path (alias/bucket)
    pub target: String,

    /// Ignore error if bucket already exists
    #[arg(short = 'p', long)]
    pub ignore_existing: bool,

    /// Region for the bucket (overrides alias default)
    #[arg(long)]
    pub region: Option<String>,

    /// Enable object locking on the bucket
    #[arg(long)]
    pub with_lock: bool,

    /// Enable versioning on the bucket
    #[arg(long)]
    pub with_versioning: bool,
}

#[derive(Debug, Serialize)]
struct MbOutput {
    status: &'static str,
    bucket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Execute the mb command
pub async fn execute(args: MbArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if args.region.is_some() || args.with_lock || args.with_versioning {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--region, --with-lock, and --with-versioning are not implemented for bucket creation",
        );
    }

    // Parse the target path
    let (alias_name, bucket) = match parse_mb_path(&args.target) {
        Ok(parsed) => parsed,
        Err(e) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &e,
                "Use a bucket path in the form alias/bucket before retrying the create command.",
            );
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
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{alias_name}' not found"),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };

    // Check if bucket already exists
    if args.ignore_existing {
        match client.bucket_exists(&bucket).await {
            Ok(true) => {
                if formatter.is_json() {
                    let output = MbOutput {
                        status: "success",
                        bucket: bucket.clone(),
                        message: Some("Bucket already exists".to_string()),
                    };
                    formatter.json(&output);
                } else {
                    formatter.success(&format!("Bucket '{alias_name}/{bucket}' already exists."));
                }
                return ExitCode::Success;
            }
            Ok(false) => {}
            Err(_) => {
                // Failed to check existence, proceed to try creating the bucket.
                // If there is a real error, create_bucket will surface it.
            }
        }
    }

    // Create the bucket
    match client.create_bucket(&bucket).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = MbOutput {
                    status: "success",
                    bucket: bucket.clone(),
                    message: None,
                };
                formatter.json(&output);
            } else {
                formatter.success(&format!(
                    "Bucket '{alias_name}/{bucket}' created successfully."
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("BucketAlreadyExists")
                || err_str.contains("BucketAlreadyOwnedByYou")
            {
                if args.ignore_existing {
                    if formatter.is_json() {
                        let output = MbOutput {
                            status: "success",
                            bucket: bucket.clone(),
                            message: Some("Bucket already exists".to_string()),
                        };
                        formatter.json(&output);
                    } else {
                        formatter
                            .success(&format!("Bucket '{alias_name}/{bucket}' already exists."));
                    }
                    return ExitCode::Success;
                }
                formatter.fail_with_suggestion(
                    ExitCode::Conflict,
                    &format!("Bucket '{alias_name}/{bucket}' already exists"),
                    "Retry with --ignore-existing if an existing bucket is acceptable.",
                )
            } else if err_str.contains("AccessDenied") {
                formatter.fail(
                    ExitCode::AuthError,
                    &format!("Access denied: cannot create bucket '{alias_name}/{bucket}'"),
                )
            } else {
                formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to create bucket: {e}"),
                )
            }
        }
    }
}

/// Parse mb target path into (alias, bucket)
fn parse_mb_path(path: &str) -> Result<(String, String), String> {
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

    // Basic bucket name validation
    if bucket.len() < 3 || bucket.len() > 63 {
        return Err("Bucket name must be between 3 and 63 characters".to_string());
    }

    Ok((alias, bucket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mb_path_valid() {
        let (alias, bucket) = parse_mb_path("myalias/mybucket").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_mb_path_trailing_slash() {
        let (alias, bucket) = parse_mb_path("myalias/mybucket/").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_mb_path_no_bucket() {
        assert!(parse_mb_path("myalias").is_err());
    }

    #[test]
    fn test_parse_mb_path_empty_bucket() {
        assert!(parse_mb_path("myalias/").is_err());
    }

    #[test]
    fn test_parse_mb_path_short_bucket() {
        assert!(parse_mb_path("myalias/ab").is_err());
    }

    #[test]
    fn test_parse_mb_path_empty() {
        assert!(parse_mb_path("").is_err());
    }
}
