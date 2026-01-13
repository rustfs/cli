//! rm command - Remove objects
//!
//! Removes one or more objects from a bucket.

use clap::Args;
use rc_core::{AliasManager, ListOptions, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Remove objects
#[derive(Args, Debug)]
pub struct RmArgs {
    /// Object path(s) to remove (alias/bucket/key or alias/bucket/prefix/)
    #[arg(required = true)]
    pub paths: Vec<String>,

    /// Remove recursively (remove all objects with the given prefix)
    #[arg(short, long)]
    pub recursive: bool,

    /// Force removal without confirmation
    #[arg(short, long)]
    pub force: bool,

    /// Only show what would be deleted (dry run)
    #[arg(long)]
    pub dry_run: bool,

    /// Remove incomplete multipart uploads older than specified duration
    #[arg(long)]
    pub incomplete: bool,

    /// Include versions (requires versioning support)
    #[arg(long)]
    pub versions: bool,

    /// Bypass governance retention
    #[arg(long)]
    pub bypass: bool,
}

#[derive(Debug, Serialize)]
struct RmOutput {
    status: &'static str,
    deleted: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<Vec<String>>,
    total: usize,
}

/// Execute the rm command
pub async fn execute(args: RmArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Process each path
    let mut all_deleted = Vec::new();
    let mut all_failed = Vec::new();
    let mut has_error = false;

    for path_str in &args.paths {
        match process_rm_path(path_str, &args, &formatter).await {
            Ok(deleted) => all_deleted.extend(deleted),
            Err((code, failed)) => {
                has_error = true;
                all_failed.extend(failed);
                if code != ExitCode::Success {
                    // Continue processing other paths unless it's a critical error
                    if code == ExitCode::AuthError || code == ExitCode::UsageError {
                        return code;
                    }
                }
            }
        }
    }

    // Output summary
    if formatter.is_json() {
        let output = RmOutput {
            status: if has_error { "partial" } else { "success" },
            deleted: all_deleted.clone(),
            failed: if all_failed.is_empty() {
                None
            } else {
                Some(all_failed)
            },
            total: all_deleted.len(),
        };
        formatter.json(&output);
    } else if !args.dry_run && !all_deleted.is_empty() {
        formatter.success(&format!("Removed {} object(s).", all_deleted.len()));
    }

    if has_error {
        ExitCode::GeneralError
    } else {
        ExitCode::Success
    }
}

async fn process_rm_path(
    path_str: &str,
    args: &RmArgs,
    formatter: &Formatter,
) -> Result<Vec<String>, (ExitCode, Vec<String>)> {
    // Parse the path
    let (alias_name, bucket, key) = match parse_rm_path(path_str) {
        Ok(parsed) => parsed,
        Err(e) => {
            formatter.error(&e);
            return Err((ExitCode::UsageError, vec![path_str.to_string()]));
        }
    };

    // Load alias
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return Err((ExitCode::GeneralError, vec![]));
        }
    };

    let alias = match alias_manager.get(&alias_name) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            return Err((ExitCode::NotFound, vec![]));
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create S3 client: {e}"));
            return Err((ExitCode::NetworkError, vec![]));
        }
    };

    let is_prefix = key.ends_with('/') || key.is_empty();

    // If recursive or prefix, list and delete all matching objects
    if args.recursive || is_prefix {
        delete_recursive(&client, &alias_name, &bucket, &key, args, formatter).await
    } else {
        // Delete single object
        delete_single(&client, &alias_name, &bucket, &key, args, formatter).await
    }
}

async fn delete_single(
    client: &S3Client,
    alias_name: &str,
    bucket: &str,
    key: &str,
    args: &RmArgs,
    formatter: &Formatter,
) -> Result<Vec<String>, (ExitCode, Vec<String>)> {
    let path = RemotePath::new(alias_name, bucket, key);
    let full_path = format!("{alias_name}/{bucket}/{key}");

    if args.dry_run {
        formatter.println(&format!("Would remove: {full_path}"));
        return Ok(vec![full_path]);
    }

    match client.delete_object(&path).await {
        Ok(()) => {
            if !formatter.is_json() {
                formatter.println(&format!("Removed: {full_path}"));
            }
            Ok(vec![full_path])
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                if args.force {
                    // Force mode: ignore not found errors
                    Ok(vec![])
                } else {
                    formatter.error(&format!("Object not found: {full_path}"));
                    Err((ExitCode::NotFound, vec![full_path]))
                }
            } else if err_str.contains("AccessDenied") {
                formatter.error(&format!("Access denied: {full_path}"));
                Err((ExitCode::AuthError, vec![full_path]))
            } else {
                formatter.error(&format!("Failed to remove {full_path}: {e}"));
                Err((ExitCode::NetworkError, vec![full_path]))
            }
        }
    }
}

async fn delete_recursive(
    client: &S3Client,
    alias_name: &str,
    bucket: &str,
    prefix: &str,
    args: &RmArgs,
    formatter: &Formatter,
) -> Result<Vec<String>, (ExitCode, Vec<String>)> {
    let path = RemotePath::new(alias_name, bucket, prefix);

    // Collect all objects to delete
    let mut keys_to_delete = Vec::new();
    let mut continuation_token: Option<String> = None;

    loop {
        let options = ListOptions {
            recursive: true,
            max_keys: Some(1000),
            continuation_token: continuation_token.clone(),
            ..Default::default()
        };

        match client.list_objects(&path, options).await {
            Ok(result) => {
                for item in result.items {
                    if !item.is_dir {
                        keys_to_delete.push(item.key);
                    }
                }

                if result.truncated {
                    continuation_token = result.continuation_token;
                } else {
                    break;
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    formatter.error(&format!("Bucket not found: {bucket}"));
                    return Err((ExitCode::NotFound, vec![]));
                }
                formatter.error(&format!("Failed to list objects: {e}"));
                return Err((ExitCode::NetworkError, vec![]));
            }
        }
    }

    if keys_to_delete.is_empty() {
        if !args.force {
            formatter.warning(&format!(
                "No objects found matching prefix: {alias_name}/{bucket}/{prefix}"
            ));
        }
        return Ok(vec![]);
    }

    // Dry run mode
    if args.dry_run {
        for key in &keys_to_delete {
            formatter.println(&format!("Would remove: {alias_name}/{bucket}/{key}"));
        }
        return Ok(keys_to_delete
            .iter()
            .map(|k| format!("{alias_name}/{bucket}/{k}"))
            .collect());
    }

    // Delete in batches (S3 allows up to 1000 per request)
    let mut deleted = Vec::new();
    let mut failed = Vec::new();

    for chunk in keys_to_delete.chunks(1000) {
        let chunk_keys: Vec<String> = chunk.to_vec();

        match client.delete_objects(bucket, chunk_keys.clone()).await {
            Ok(deleted_keys) => {
                for key in &deleted_keys {
                    let full_path = format!("{alias_name}/{bucket}/{key}");
                    if !formatter.is_json() {
                        formatter.println(&format!("Removed: {full_path}"));
                    }
                    deleted.push(full_path);
                }
            }
            Err(e) => {
                formatter.error(&format!("Failed to delete batch: {e}"));
                for key in chunk_keys {
                    failed.push(format!("{alias_name}/{bucket}/{key}"));
                }
            }
        }
    }

    if !failed.is_empty() {
        Err((ExitCode::GeneralError, failed))
    } else {
        Ok(deleted)
    }
}

/// Parse rm path into (alias, bucket, key)
fn parse_rm_path(path: &str) -> Result<(String, String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    if parts.len() < 2 {
        return Err(format!(
            "Invalid path format: '{path}'. Expected: alias/bucket[/key]"
        ));
    }

    let alias = parts[0].to_string();
    let bucket = parts[1].to_string();
    let key = if parts.len() > 2 {
        parts[2].to_string()
    } else {
        String::new()
    };

    if bucket.is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }

    Ok((alias, bucket, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rm_path_with_key() {
        let (alias, bucket, key) = parse_rm_path("myalias/mybucket/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "file.txt");
    }

    #[test]
    fn test_parse_rm_path_with_prefix() {
        let (alias, bucket, key) = parse_rm_path("myalias/mybucket/path/to/").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/");
    }

    #[test]
    fn test_parse_rm_path_bucket_only() {
        let (alias, bucket, key) = parse_rm_path("myalias/mybucket").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "");
    }

    #[test]
    fn test_parse_rm_path_no_bucket() {
        assert!(parse_rm_path("myalias").is_err());
    }

    #[test]
    fn test_parse_rm_path_empty() {
        assert!(parse_rm_path("").is_err());
    }
}
