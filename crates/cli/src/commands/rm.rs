//! rm command - Remove objects
//!
//! Removes one or more objects from a bucket.

use clap::Args;
use rc_core::{AliasManager, ListOptions, ObjectStore as _, RemotePath};
use rc_s3::{DeleteRequestOptions, S3Client};
use serde::Serialize;
use std::collections::HashSet;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const RM_AFTER_HELP: &str = "\
Examples:
  rc object remove local/my-bucket/reports/2026-04.csv
  rc rm local/my-bucket/reports/ --recursive --dry-run
  rc object remove local/my-bucket/archive/ --recursive --force";

/// Remove objects
#[derive(Args, Debug)]
#[command(after_help = RM_AFTER_HELP)]
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

    /// Permanently delete objects using the RustFS force-delete header
    #[arg(long)]
    pub purge: bool,
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

    if args.incomplete || args.versions || args.bypass {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--incomplete, --versions, and --bypass are not implemented; refusing to continue with a silently ignored destructive option",
        );
    }

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
            let code = formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &e,
                "Use a remote path in the form alias/bucket[/key] before retrying the remove command.",
            );
            return Err((code, vec![path_str.to_string()]));
        }
    };

    if let Err(error) = validate_removal_scope(&key, args.recursive) {
        let code = formatter.fail_with_suggestion(
            ExitCode::UsageError,
            &error,
            "Add --recursive only after verifying the bucket or prefix to remove.",
        );
        return Err((code, vec![path_str.to_string()]));
    }

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
            let code = formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{alias_name}' not found"),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
            return Err((code, vec![]));
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            let code = formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
            return Err((code, vec![]));
        }
    };

    if args.recursive {
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
        let styled_path = formatter.style_file(&full_path);
        formatter.println(&format!("Would remove: {styled_path}"));
        return Ok(vec![full_path]);
    }

    match client
        .delete_object_with_options(&path, delete_request_options(args))
        .await
    {
        Ok(()) => {
            if !formatter.is_json() {
                let styled_path = formatter.style_file(&full_path);
                formatter.println(&format!("Removed: {styled_path}"));
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
                    let code = formatter.fail_with_suggestion(
                        ExitCode::NotFound,
                        &format!("Object not found: {full_path}"),
                        "Check the object key or retry with --force if missing objects are acceptable.",
                    );
                    Err((code, vec![full_path]))
                }
            } else if err_str.contains("AccessDenied") {
                let code =
                    formatter.fail(ExitCode::AuthError, &format!("Access denied: {full_path}"));
                Err((code, vec![full_path]))
            } else {
                let code = formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to remove {full_path}: {e}"),
                );
                Err((code, vec![full_path]))
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
                    let code = formatter.fail_with_suggestion(
                        ExitCode::NotFound,
                        &format!("Bucket not found: {bucket}"),
                        "Check the bucket path and retry the remove command.",
                    );
                    return Err((code, vec![]));
                }
                let code = formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to list objects: {e}"),
                );
                return Err((code, vec![]));
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
            let full_path = format!("{alias_name}/{bucket}/{key}");
            let styled_path = formatter.style_file(&full_path);
            formatter.println(&format!("Would remove: {styled_path}"));
        }
        return Ok(keys_to_delete
            .iter()
            .map(|k| format!("{alias_name}/{bucket}/{k}"))
            .collect());
    }

    if args.purge {
        let mut deleted = Vec::new();
        let mut failed = Vec::new();
        let mut first_error_code = None;

        for key in keys_to_delete {
            match delete_single(client, alias_name, bucket, &key, args, formatter).await {
                Ok(paths) => deleted.extend(paths),
                Err((code, paths)) => {
                    first_error_code.get_or_insert(code);
                    failed.extend(paths);
                }
            }
        }

        return if failed.is_empty() {
            Ok(deleted)
        } else {
            Err((first_error_code.unwrap_or(ExitCode::GeneralError), failed))
        };
    }

    // Delete in batches (S3 allows up to 1000 per request)
    let mut deleted = Vec::new();
    let mut failed = Vec::new();

    for chunk in keys_to_delete.chunks(1000) {
        let chunk_keys: Vec<String> = chunk.to_vec();

        match client
            .delete_objects_with_options(bucket, chunk_keys.clone(), delete_request_options(args))
            .await
        {
            Ok(deleted_keys) => {
                let (confirmed_keys, failed_keys) =
                    partition_delete_results(&chunk_keys, deleted_keys);
                for key in &confirmed_keys {
                    let full_path = format!("{alias_name}/{bucket}/{key}");
                    if !formatter.is_json() {
                        let styled_path = formatter.style_file(&full_path);
                        formatter.println(&format!("Removed: {styled_path}"));
                    }
                    deleted.push(full_path);
                }
                failed.extend(
                    failed_keys
                        .into_iter()
                        .map(|key| format!("{alias_name}/{bucket}/{key}")),
                );
            }
            Err(e) => {
                formatter.error_with_code(
                    ExitCode::GeneralError,
                    &format!("Failed to delete batch: {e}"),
                );
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

    if alias.is_empty() {
        return Err("Alias name cannot be empty".to_string());
    }

    if bucket.is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }

    Ok((alias, bucket, key))
}

fn delete_request_options(args: &RmArgs) -> DeleteRequestOptions {
    DeleteRequestOptions {
        force_delete: args.purge,
    }
}

fn validate_removal_scope(key: &str, recursive: bool) -> Result<(), String> {
    if !recursive && (key.is_empty() || key.ends_with('/')) {
        return Err("Bucket and prefix removal requires the explicit --recursive flag".to_string());
    }
    Ok(())
}

fn partition_delete_results(
    requested: &[String],
    deleted: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let deleted: HashSet<String> = deleted.into_iter().collect();
    requested
        .iter()
        .cloned()
        .partition(|key| deleted.contains(key))
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

    #[test]
    fn test_parse_rm_path_empty_alias() {
        assert!(parse_rm_path("/mybucket/file.txt").is_err());
    }

    #[test]
    fn recursive_targets_require_explicit_recursive_flag() {
        assert!(validate_removal_scope("", false).is_err());
        assert!(validate_removal_scope("prefix/", false).is_err());
        assert!(validate_removal_scope("prefix/", true).is_ok());
        assert!(validate_removal_scope("object.txt", false).is_ok());
    }

    #[test]
    fn partial_batch_delete_marks_missing_results_as_failed() {
        let requested = vec!["deleted.txt".to_string(), "denied.txt".to_string()];
        let deleted = vec!["deleted.txt".to_string()];

        let (confirmed, failed) = partition_delete_results(&requested, deleted);

        assert_eq!(confirmed, vec!["deleted.txt"]);
        assert_eq!(failed, vec!["denied.txt"]);
    }

    #[test]
    fn test_delete_request_options_enable_force_delete_for_purge() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: false,
            dry_run: false,
            incomplete: false,
            versions: false,
            bypass: false,
            purge: true,
        };

        let options = delete_request_options(&args);
        assert!(options.force_delete);
    }

    #[test]
    fn test_delete_request_options_keep_force_delete_disabled_by_default() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: false,
            dry_run: false,
            incomplete: false,
            versions: false,
            bypass: false,
            purge: false,
        };

        let options = delete_request_options(&args);
        assert!(!options.force_delete);
    }

    #[test]
    fn test_delete_request_options_ignore_force_flag_without_purge() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: true,
            dry_run: false,
            incomplete: false,
            versions: false,
            bypass: false,
            purge: false,
        };

        let options = delete_request_options(&args);
        assert!(!options.force_delete);
    }
}
