//! rm command - Remove objects
//!
//! Removes one or more objects from a bucket.

use clap::Args;
use rc_core::{
    AliasManager, DeleteRequestOptions, Error, ListObjectVersionsOptions, ListOptions, ObjectStore,
    ObjectVersionIdentifier, RemotePath,
};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::HashSet;

use crate::commands::exit_code_for_core_error;
use crate::exit_code::ExitCode;
use crate::output::{
    Formatter, OutputConfig, V3ErrorEnvelope, V3PartialErrorEnvelope, V3SuccessEnvelope,
};

const RM_AFTER_HELP: &str = "\
Examples:
  rc object remove local/my-bucket/reports/2026-04.csv
  rc rm local/my-bucket/reports/2026-04.csv --version-id VERSION_ID
  rc rm local/my-bucket/reports/ --recursive --dry-run
  rc rm local/my-bucket/reports/ --recursive --versions --bypass
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
    #[arg(long, hide = true)]
    pub incomplete: bool,

    /// Remove all matching object versions and delete markers
    #[arg(long)]
    pub versions: bool,

    /// Remove one exact object version
    #[arg(long, value_name = "VERSION_ID")]
    pub version_id: Option<String>,

    /// Explicitly bypass Object Lock governance retention
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
    #[serde(skip_serializing_if = "Option::is_none")]
    deleted_versions: Option<Vec<RemovalRecord>>,
    total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RemovalRecord {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_id: Option<String>,
    is_delete_marker: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct VersionRemovalItem {
    path: String,
    version_id: Option<String>,
    delete_marker: bool,
}

impl From<RemovalRecord> for VersionRemovalItem {
    fn from(record: RemovalRecord) -> Self {
        Self {
            path: record.path,
            version_id: record.version_id,
            delete_marker: record.is_delete_marker,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RemovalFailureRecord {
    path: String,
    version_id: Option<String>,
    error_type: &'static str,
    message: String,
}

#[derive(Debug)]
pub(crate) struct RemovalError {
    pub(crate) code: ExitCode,
    removed: Vec<RemovalRecord>,
    failed: Vec<RemovalFailureRecord>,
}

impl RemovalError {
    fn failed(code: ExitCode, failure: RemovalFailureRecord) -> Self {
        Self {
            code,
            removed: Vec::new(),
            failed: vec![failure],
        }
    }

    fn partial(
        code: ExitCode,
        removed: Vec<RemovalRecord>,
        failed: Vec<RemovalFailureRecord>,
    ) -> Self {
        Self {
            code,
            removed,
            failed,
        }
    }
}

type RemovalResult = Result<Vec<RemovalRecord>, RemovalError>;

#[derive(Debug, Serialize)]
struct VersionRemoveData {
    operation: &'static str,
    outcome: &'static str,
    dry_run: bool,
    planned: Vec<VersionRemovalItem>,
    removed: Vec<VersionRemovalItem>,
    failed: Vec<RemovalFailureRecord>,
    summary: VersionRemoveSummary,
}

#[derive(Debug, Serialize)]
struct VersionRemoveSummary {
    planned: usize,
    removed: usize,
    failed: usize,
}

/// Execute the rm command
pub async fn execute(args: RmArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let version_output = uses_version_output(&args);

    if args.incomplete {
        return fail_rm(
            &formatter,
            version_output,
            ExitCode::UnsupportedFeature,
            "--incomplete is not implemented; refusing to continue with a silently ignored destructive option",
        );
    }
    if let Err(error) = validate_rm_selectors(&args) {
        return fail_rm(&formatter, version_output, ExitCode::UsageError, &error);
    }

    // Process each path
    let mut all_deleted = Vec::new();
    let mut all_failed = Vec::new();
    let mut first_error_code = None;

    for path_str in &args.paths {
        let critical = collect_removal_result(
            process_rm_path(path_str, &args, &formatter).await,
            &mut all_deleted,
            &mut all_failed,
            &mut first_error_code,
        );
        if let Some(code) = critical {
            if formatter.is_json() && version_output {
                break;
            }
            return code;
        }
    }

    // Output summary
    if formatter.is_json() && version_output {
        let failure_count = all_failed.len();
        let data = build_version_remove_data(args.dry_run, all_deleted, all_failed);
        if let Some(code) = first_error_code {
            let action = if args.dry_run {
                "plan removal of"
            } else {
                "remove"
            };
            formatter.json_error(&V3PartialErrorEnvelope::versioned_objects(
                code,
                format!("Failed to {action} {failure_count} object version(s)"),
                Some("delete_object_versions"),
                data,
            ));
        } else {
            formatter.json(&V3SuccessEnvelope::versioned_objects(data));
        }
    } else if formatter.is_json() {
        let output = RmOutput {
            status: if first_error_code.is_some() {
                "partial"
            } else {
                "success"
            },
            deleted: all_deleted
                .iter()
                .map(|record| record.path.clone())
                .collect(),
            failed: if all_failed.is_empty() {
                None
            } else {
                Some(all_failed.into_iter().map(|failure| failure.path).collect())
            },
            deleted_versions: all_deleted
                .iter()
                .any(|record| record.version_id.is_some())
                .then(|| all_deleted.clone()),
            total: all_deleted.len(),
        };
        formatter.json(&output);
    } else if !args.dry_run && !all_deleted.is_empty() {
        formatter.success(&format!("Removed {} object(s).", all_deleted.len()));
    }

    first_error_code.unwrap_or(ExitCode::Success)
}

fn uses_version_output(args: &RmArgs) -> bool {
    args.version_id.is_some() || args.versions
}

fn fail_rm(formatter: &Formatter, version_output: bool, code: ExitCode, message: &str) -> ExitCode {
    if formatter.is_json() && version_output {
        formatter.json_error(&V3ErrorEnvelope::versioned_objects(
            code,
            message,
            Some("versioned_objects"),
        ));
        code
    } else {
        formatter.fail(code, message)
    }
}

fn report_rm_error(
    formatter: &Formatter,
    args: &RmArgs,
    code: ExitCode,
    message: &str,
    suggestion: Option<&str>,
) -> ExitCode {
    if formatter.is_json() && uses_version_output(args) {
        return code;
    }
    if let Some(suggestion) = suggestion {
        formatter.fail_with_suggestion(code, message, suggestion)
    } else {
        formatter.fail(code, message)
    }
}

fn removal_failure(
    path: impl Into<String>,
    version_id: Option<String>,
    code: ExitCode,
    message: impl Into<String>,
) -> RemovalFailureRecord {
    RemovalFailureRecord {
        path: path.into(),
        version_id,
        error_type: error_type_for_exit_code(code),
        message: message.into(),
    }
}

fn error_type_for_exit_code(code: ExitCode) -> &'static str {
    match code {
        ExitCode::Success | ExitCode::GeneralError => "general_error",
        ExitCode::UsageError => "usage_error",
        ExitCode::NetworkError => "network_error",
        ExitCode::AuthError => "auth_error",
        ExitCode::NotFound => "not_found",
        ExitCode::Conflict => "conflict",
        ExitCode::UnsupportedFeature => "unsupported_feature",
        ExitCode::Interrupted => "interrupted",
    }
}

fn collect_removal_result(
    result: RemovalResult,
    all_removed: &mut Vec<RemovalRecord>,
    all_failed: &mut Vec<RemovalFailureRecord>,
    first_error_code: &mut Option<ExitCode>,
) -> Option<ExitCode> {
    match result {
        Ok(removed) => {
            all_removed.extend(removed);
            None
        }
        Err(error) => {
            record_removal_error(first_error_code, error.code);
            all_removed.extend(error.removed);
            all_failed.extend(error.failed);
            is_critical_removal_error(error.code).then_some(error.code)
        }
    }
}

fn record_removal_error(current: &mut Option<ExitCode>, candidate: ExitCode) {
    match current {
        None => *current = Some(candidate),
        Some(existing)
            if is_critical_removal_error(candidate) && !is_critical_removal_error(*existing) =>
        {
            *current = Some(candidate);
        }
        Some(_) => {}
    }
}

fn is_critical_removal_error(code: ExitCode) -> bool {
    matches!(
        code,
        ExitCode::AuthError | ExitCode::UsageError | ExitCode::Interrupted
    )
}

fn build_version_remove_data(
    dry_run: bool,
    records: Vec<RemovalRecord>,
    failed: Vec<RemovalFailureRecord>,
) -> VersionRemoveData {
    let records = records
        .into_iter()
        .map(VersionRemovalItem::from)
        .collect::<Vec<_>>();
    let (planned, removed) = if dry_run {
        (records, Vec::new())
    } else {
        (Vec::new(), records)
    };
    let outcome = if !failed.is_empty() {
        if !dry_run && !removed.is_empty() {
            "partial"
        } else {
            "failed"
        }
    } else if dry_run && !planned.is_empty() {
        "planned"
    } else if removed.is_empty() {
        "empty"
    } else {
        "success"
    };
    let summary = VersionRemoveSummary {
        planned: planned.len(),
        removed: removed.len(),
        failed: failed.len(),
    };

    VersionRemoveData {
        operation: "remove",
        outcome,
        dry_run,
        planned,
        removed,
        failed,
        summary,
    }
}

async fn process_rm_path(path_str: &str, args: &RmArgs, formatter: &Formatter) -> RemovalResult {
    // Parse the path
    let (alias_name, bucket, key) = match parse_rm_path(path_str) {
        Ok(parsed) => parsed,
        Err(e) => {
            let code = report_rm_error(
                formatter,
                args,
                ExitCode::UsageError,
                &e,
                Some(
                    "Use a remote path in the form alias/bucket[/key] before retrying the remove command.",
                ),
            );
            return Err(RemovalError::failed(
                code,
                removal_failure(path_str, args.version_id.clone(), code, e),
            ));
        }
    };

    if let Err(error) = validate_removal_scope(&key, args.recursive) {
        let code = report_rm_error(
            formatter,
            args,
            ExitCode::UsageError,
            &error,
            Some("Add --recursive only after verifying the bucket or prefix to remove."),
        );
        return Err(RemovalError::failed(
            code,
            removal_failure(path_str, args.version_id.clone(), code, error),
        ));
    }

    // Load alias
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            let message = format!("Failed to load aliases: {e}");
            let code = report_rm_error(formatter, args, ExitCode::GeneralError, &message, None);
            return Err(RemovalError::failed(
                code,
                removal_failure(path_str, args.version_id.clone(), code, message),
            ));
        }
    };

    let alias = match alias_manager.get(&alias_name) {
        Ok(a) => a,
        Err(_) => {
            let message = format!("Alias '{alias_name}' not found");
            let code = report_rm_error(
                formatter,
                args,
                ExitCode::NotFound,
                &message,
                Some(
                    "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
                ),
            );
            return Err(RemovalError::failed(
                code,
                removal_failure(path_str, args.version_id.clone(), code, message),
            ));
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            let message = format!("Failed to create S3 client: {e}");
            let code = report_rm_error(formatter, args, ExitCode::NetworkError, &message, None);
            return Err(RemovalError::failed(
                code,
                removal_failure(path_str, args.version_id.clone(), code, message),
            ));
        }
    };

    if args.versions {
        delete_versions(&client, &alias_name, &bucket, &key, args, formatter).await
    } else if args.recursive {
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
) -> RemovalResult {
    let path = RemotePath::new(alias_name, bucket, key);
    let full_path = format!("{alias_name}/{bucket}/{key}");

    if args.dry_run {
        if !formatter.is_json() {
            let styled_path = formatter.style_file(&full_path);
            let suffix = args
                .version_id
                .as_deref()
                .map(|version| format!(" (version {version})"))
                .unwrap_or_default();
            formatter.println(&format!("Would remove: {styled_path}{suffix}"));
        }
        return Ok(vec![RemovalRecord {
            path: full_path,
            version_id: args.version_id.clone(),
            is_delete_marker: false,
        }]);
    }

    match ObjectStore::delete_object_with_options(client, &path, delete_request_options(args)).await
    {
        Ok(deleted) => {
            if !formatter.is_json() {
                let styled_path = formatter.style_file(&full_path);
                let version = deleted
                    .version_id
                    .as_deref()
                    .map(|version_id| format!(" (version {version_id})"))
                    .unwrap_or_default();
                let marker = if deleted.is_delete_marker {
                    " [delete marker]"
                } else {
                    ""
                };
                formatter.println(&format!("Removed: {styled_path}{version}{marker}"));
            }
            Ok(vec![RemovalRecord {
                path: full_path,
                version_id: deleted.version_id.or_else(|| args.version_id.clone()),
                is_delete_marker: deleted.is_delete_marker,
            }])
        }
        Err(e) => {
            if matches!(
                &e,
                Error::NotFound(_) | Error::VersionNotFound { .. } | Error::DeleteMarker { .. }
            ) {
                if args.force {
                    // Force mode: ignore not found errors
                    Ok(vec![])
                } else {
                    let message = e.to_string();
                    let code = report_rm_error(
                        formatter,
                        args,
                        ExitCode::NotFound,
                        &message,
                        Some(
                            "Check the object key or retry with --force if missing objects are acceptable.",
                        ),
                    );
                    Err(RemovalError::failed(
                        code,
                        removal_failure(full_path, args.version_id.clone(), code, message),
                    ))
                }
            } else {
                let exit_code = exit_code_for_core_error(&e);
                let message = format!("Failed to remove {full_path}: {e}");
                let code = report_rm_error(formatter, args, exit_code, &message, None);
                Err(RemovalError::failed(
                    code,
                    removal_failure(full_path, args.version_id.clone(), code, message),
                ))
            }
        }
    }
}

pub(crate) async fn delete_recursive(
    client: &S3Client,
    alias_name: &str,
    bucket: &str,
    prefix: &str,
    args: &RmArgs,
    formatter: &Formatter,
) -> RemovalResult {
    let path = RemotePath::new(alias_name, bucket, prefix);

    // Collect all objects to delete
    let mut keys_to_delete = Vec::new();
    if args.purge {
        match list_versions_for_removal(client, &path, true).await {
            Ok(versions) => {
                let mut unique_keys = versions
                    .into_iter()
                    .map(|version| version.key)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                unique_keys.sort();
                keys_to_delete = unique_keys;
            }
            Err(error) => {
                let code = exit_code_for_core_error(&error);
                let message = format!("Failed to list object versions: {error}");
                report_rm_error(formatter, args, code, &message, None);
                return Err(RemovalError::failed(
                    code,
                    removal_failure(path.to_string(), None, code, message),
                ));
            }
        }
    } else {
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
                    keys_to_delete.extend(
                        result
                            .items
                            .into_iter()
                            .filter(|item| !item.is_dir)
                            .map(|item| item.key),
                    );
                    if result.truncated {
                        continuation_token = result.continuation_token;
                    } else {
                        break;
                    }
                }
                Err(error) => {
                    let code = exit_code_for_core_error(&error);
                    let message = format!("Failed to list objects: {error}");
                    report_rm_error(formatter, args, code, &message, None);
                    return Err(RemovalError::failed(
                        code,
                        removal_failure(path.to_string(), None, code, message),
                    ));
                }
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
            if !formatter.is_json() {
                let styled_path = formatter.style_file(&full_path);
                formatter.println(&format!("Would remove: {styled_path}"));
            }
        }
        return Ok(keys_to_delete
            .iter()
            .map(|key| RemovalRecord {
                path: format!("{alias_name}/{bucket}/{key}"),
                version_id: None,
                is_delete_marker: false,
            })
            .collect());
    }

    if args.purge {
        let mut deleted = Vec::new();
        let mut failed = Vec::new();
        let mut first_error_code = None;

        for key in keys_to_delete {
            let critical = collect_removal_result(
                delete_single(client, alias_name, bucket, &key, args, formatter).await,
                &mut deleted,
                &mut failed,
                &mut first_error_code,
            );
            if critical.is_some() {
                break;
            }
        }

        return if failed.is_empty() {
            Ok(deleted)
        } else {
            Err(RemovalError::partial(
                first_error_code.unwrap_or(ExitCode::GeneralError),
                deleted,
                failed,
            ))
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
                    deleted.push(RemovalRecord {
                        path: full_path,
                        version_id: None,
                        is_delete_marker: false,
                    });
                }
                failed.extend(failed_keys.into_iter().map(|key| {
                    let path = format!("{alias_name}/{bucket}/{key}");
                    removal_failure(
                        path,
                        None,
                        ExitCode::GeneralError,
                        "The backend omitted the object from its delete result",
                    )
                }));
            }
            Err(e) => {
                let message = format!("Failed to delete batch: {e}");
                report_rm_error(formatter, args, ExitCode::GeneralError, &message, None);
                for key in chunk_keys {
                    failed.push(removal_failure(
                        format!("{alias_name}/{bucket}/{key}"),
                        None,
                        ExitCode::GeneralError,
                        message.clone(),
                    ));
                }
            }
        }
    }

    if !failed.is_empty() {
        Err(RemovalError::partial(
            ExitCode::GeneralError,
            deleted,
            failed,
        ))
    } else {
        Ok(deleted)
    }
}

async fn delete_versions(
    client: &S3Client,
    alias_name: &str,
    bucket: &str,
    key: &str,
    args: &RmArgs,
    formatter: &Formatter,
) -> RemovalResult {
    let path = RemotePath::new(alias_name, bucket, key);
    let versions = match list_versions_for_removal(client, &path, args.recursive).await {
        Ok(versions) => versions,
        Err(error) => {
            let exit_code = exit_code_for_core_error(&error);
            let message = format!("Failed to list object versions for removal: {error}");
            report_rm_error(formatter, args, exit_code, &message, None);
            return Err(RemovalError::failed(
                exit_code,
                removal_failure(path.to_string(), None, exit_code, message),
            ));
        }
    };

    if versions.is_empty() {
        if !args.force {
            formatter.warning(&format!("No object versions found: {path}"));
        }
        return Ok(Vec::new());
    }

    if args.dry_run {
        let records = versions
            .into_iter()
            .map(|version| {
                let full_path = format!("{alias_name}/{bucket}/{}", version.key);
                if !formatter.is_json() {
                    let marker = if version.is_delete_marker {
                        ", delete marker"
                    } else {
                        ""
                    };
                    formatter.println(&format!(
                        "Would remove: {} (version {}{marker})",
                        formatter.style_file(&full_path),
                        version.version_id.as_deref().unwrap_or("null")
                    ));
                }
                RemovalRecord {
                    path: full_path,
                    version_id: version.version_id,
                    is_delete_marker: version.is_delete_marker,
                }
            })
            .collect();
        return Ok(records);
    }

    let mut removed = Vec::new();
    let mut failed = Vec::new();
    let mut first_error_code = None;

    for (chunk_index, chunk) in versions.chunks(1000).enumerate() {
        let requested = chunk.to_vec();
        match ObjectStore::delete_object_versions(
            client,
            bucket,
            requested.clone(),
            delete_request_options(args),
        )
        .await
        {
            Ok(result) => {
                let mut unmatched = requested.clone();
                for deleted in result.deleted {
                    let requested_entry = take_requested_version(
                        &mut unmatched,
                        &deleted.key,
                        deleted.version_id.as_deref(),
                    );
                    let version_id = deleted.version_id.or_else(|| {
                        requested_entry
                            .as_ref()
                            .and_then(|entry| entry.version_id.clone())
                    });
                    let is_delete_marker = deleted.is_delete_marker
                        || requested_entry.is_some_and(|entry| entry.is_delete_marker);
                    let full_path = format!("{alias_name}/{bucket}/{}", deleted.key);
                    if !formatter.is_json() {
                        let marker = if is_delete_marker {
                            ", delete marker"
                        } else {
                            ""
                        };
                        formatter.println(&format!(
                            "Removed: {} (version {}{marker})",
                            formatter.style_file(&full_path),
                            version_id.as_deref().unwrap_or("null")
                        ));
                    }
                    removed.push(RemovalRecord {
                        path: full_path,
                        version_id,
                        is_delete_marker,
                    });
                }

                for failure in result.failures {
                    let requested_entry = take_requested_version(
                        &mut unmatched,
                        &failure.key,
                        failure.version_id.as_deref(),
                    );
                    let code = exit_code_for_delete_failure(
                        failure.code.as_deref(),
                        failure.message.as_deref(),
                    );
                    record_removal_error(&mut first_error_code, code);
                    let full_path = format!("{alias_name}/{bucket}/{}", failure.key);
                    let version_id = failure
                        .version_id
                        .or_else(|| requested_entry.and_then(|entry| entry.version_id));
                    let message = failure
                        .message
                        .as_deref()
                        .or(failure.code.as_deref())
                        .unwrap_or("unknown delete error")
                        .to_string();
                    report_rm_error(
                        formatter,
                        args,
                        code,
                        &format!(
                            "Failed to remove {} (version {}): {}",
                            full_path,
                            version_id.as_deref().unwrap_or("null"),
                            message
                        ),
                        None,
                    );
                    failed.push(removal_failure(full_path, version_id, code, message));
                }

                for omitted in unmatched {
                    let full_path = format!("{alias_name}/{bucket}/{}", omitted.key);
                    let message = "The backend omitted the object version from its delete result";
                    record_removal_error(&mut first_error_code, ExitCode::GeneralError);
                    report_rm_error(
                        formatter,
                        args,
                        ExitCode::GeneralError,
                        &format!(
                            "Failed to remove {} (version {}): {message}",
                            full_path,
                            omitted.version_id.as_deref().unwrap_or("null")
                        ),
                        None,
                    );
                    failed.push(removal_failure(
                        full_path,
                        omitted.version_id,
                        ExitCode::GeneralError,
                        message,
                    ));
                }
            }
            Err(error) => {
                let code = exit_code_for_core_error(&error);
                record_removal_error(&mut first_error_code, code);
                let message = format!("Failed to delete versions: {error}");
                report_rm_error(formatter, args, code, &message, None);
                failed.extend(requested.into_iter().map(|entry| {
                    removal_failure(
                        format!("{alias_name}/{bucket}/{}", entry.key),
                        entry.version_id,
                        code,
                        message.clone(),
                    )
                }));
                if is_critical_removal_error(code) {
                    failed.extend(versions.iter().skip((chunk_index + 1) * 1000).map(|entry| {
                        removal_failure(
                            format!("{alias_name}/{bucket}/{}", entry.key),
                            entry.version_id.clone(),
                            code,
                            format!(
                                "Not attempted after a critical version deletion failure: {error}"
                            ),
                        )
                    }));
                    break;
                }
            }
        }
    }

    if failed.is_empty() {
        Ok(removed)
    } else {
        Err(RemovalError::partial(
            first_error_code.unwrap_or(ExitCode::GeneralError),
            removed,
            failed,
        ))
    }
}

fn take_requested_version(
    requested: &mut Vec<ObjectVersionIdentifier>,
    key: &str,
    version_id: Option<&str>,
) -> Option<ObjectVersionIdentifier> {
    let position = requested.iter().position(|entry| {
        entry.key == key
            && match version_id {
                Some(version_id) => entry.version_id.as_deref() == Some(version_id),
                None => true,
            }
    })?;
    Some(requested.remove(position))
}

async fn list_versions_for_removal(
    client: &S3Client,
    path: &RemotePath,
    recursive: bool,
) -> Result<Vec<ObjectVersionIdentifier>, Error> {
    let mut versions = Vec::new();
    let mut key_marker = None;
    let mut version_id_marker = None;

    loop {
        let page = ObjectStore::list_object_versions_page_with_options(
            client,
            path,
            &ListObjectVersionsOptions {
                max_keys: Some(1000),
                key_marker: key_marker.clone(),
                version_id_marker: version_id_marker.clone(),
            },
        )
        .await?;
        versions.extend(
            page.items
                .into_iter()
                .filter(|version| recursive || version.key == path.key)
                .map(|version| ObjectVersionIdentifier {
                    key: version.key,
                    version_id: Some(version.version_id),
                    is_delete_marker: version.is_delete_marker,
                }),
        );

        if !page.truncated {
            break;
        }
        let next_key_marker = page.continuation_token.ok_or_else(|| {
            Error::Network(
                "S3 returned a truncated version listing without a key marker".to_string(),
            )
        })?;
        let next_version_id_marker = page.version_id_marker;
        if key_marker.as_deref() == Some(next_key_marker.as_str())
            && version_id_marker == next_version_id_marker
        {
            return Err(Error::Network(
                "S3 returned a truncated version listing without advancing its markers".to_string(),
            ));
        }
        key_marker = Some(next_key_marker);
        version_id_marker = next_version_id_marker;
    }

    Ok(versions)
}

fn exit_code_for_delete_failure(code: Option<&str>, message: Option<&str>) -> ExitCode {
    let normalized_message = message.unwrap_or_default().to_ascii_lowercase();
    if matches!(
        code,
        Some("NoSuchVersion") | Some("NoSuchKey") | Some("NotFound")
    ) {
        ExitCode::NotFound
    } else if normalized_message.contains("governance")
        || normalized_message.contains("retention")
        || normalized_message.contains("object lock")
        || normalized_message.contains("worm")
    {
        ExitCode::Conflict
    } else if matches!(
        code,
        Some("AccessDenied") | Some("Forbidden") | Some("Unauthorized")
    ) || normalized_message.contains("access denied")
        || normalized_message.contains("forbidden")
        || normalized_message.contains("unauthorized")
    {
        ExitCode::AuthError
    } else {
        ExitCode::GeneralError
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
        version_id: args.version_id.clone(),
        bypass_governance: args.bypass,
        force_delete: args.purge,
    }
}

fn validate_rm_selectors(args: &RmArgs) -> Result<(), String> {
    if args.version_id.as_deref().is_some_and(str::is_empty) {
        return Err("--version-id cannot be empty".to_string());
    }
    if args.version_id.is_some() && args.versions {
        return Err("--version-id cannot be combined with --versions".to_string());
    }
    if args.version_id.is_some() && args.recursive {
        return Err("--version-id cannot be combined with --recursive".to_string());
    }
    if args.version_id.is_some() && args.purge {
        return Err("--version-id cannot be combined with --purge".to_string());
    }
    if args.version_id.is_some() && args.paths.len() != 1 {
        return Err("--version-id requires exactly one object path".to_string());
    }
    Ok(())
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
            version_id: None,
            bypass: false,
            purge: true,
        };

        let options = delete_request_options(&args);
        assert!(options.force_delete);
    }

    #[test]
    fn test_delete_request_options_do_not_force_delete_for_versions() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: false,
            dry_run: false,
            incomplete: false,
            versions: true,
            version_id: None,
            bypass: false,
            purge: false,
        };

        let options = delete_request_options(&args);
        assert!(!options.force_delete);
    }

    #[test]
    fn test_delete_request_options_enable_governance_bypass() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: false,
            dry_run: false,
            incomplete: false,
            versions: false,
            version_id: None,
            bypass: true,
            purge: false,
        };

        let options = delete_request_options(&args);
        assert!(options.bypass_governance);
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
            version_id: None,
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
            version_id: None,
            bypass: false,
            purge: false,
        };

        let options = delete_request_options(&args);
        assert!(!options.force_delete);
    }

    #[test]
    fn delete_request_options_require_explicit_bypass_and_preserve_version() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: false,
            dry_run: false,
            incomplete: false,
            versions: false,
            version_id: Some("v1".to_string()),
            bypass: true,
            purge: false,
        };

        let options = delete_request_options(&args);
        assert_eq!(options.version_id.as_deref(), Some("v1"));
        assert!(options.bypass_governance);

        let default_args = RmArgs {
            bypass: false,
            ..args
        };
        assert!(!delete_request_options(&default_args).bypass_governance);
    }

    #[test]
    fn rm_rejects_conflicting_version_selectors() {
        let args = RmArgs {
            paths: vec!["test/bucket/object.txt".to_string()],
            recursive: false,
            force: false,
            dry_run: false,
            incomplete: false,
            versions: true,
            version_id: Some("v1".to_string()),
            bypass: false,
            purge: false,
        };

        assert!(validate_rm_selectors(&args).is_err());
    }

    #[test]
    fn batch_delete_failures_distinguish_access_and_governance_denials() {
        assert_eq!(
            exit_code_for_delete_failure(Some("NoSuchVersion"), Some("missing")),
            ExitCode::NotFound
        );
        assert_eq!(
            exit_code_for_delete_failure(Some("AccessDenied"), Some("policy denied")),
            ExitCode::AuthError
        );
        assert_eq!(
            exit_code_for_delete_failure(
                Some("AccessDenied"),
                Some("governance retention is active")
            ),
            ExitCode::Conflict
        );
    }

    #[test]
    fn rm_output_preserves_version_and_delete_marker_fields() {
        let output = RmOutput {
            status: "success",
            deleted: vec!["test/bucket/key.txt".to_string()],
            failed: None,
            deleted_versions: Some(vec![RemovalRecord {
                path: "test/bucket/key.txt".to_string(),
                version_id: Some("marker-v1".to_string()),
                is_delete_marker: true,
            }]),
            total: 1,
        };

        let json = serde_json::to_value(output).expect("serialize version-aware removal");
        assert_eq!(json["deleted"][0], "test/bucket/key.txt");
        assert_eq!(json["deleted_versions"][0]["version_id"], "marker-v1");
        assert_eq!(json["deleted_versions"][0]["is_delete_marker"], true);
    }

    #[test]
    fn partial_version_removal_keeps_successes_and_version_aware_failures() {
        let removed = RemovalRecord {
            path: "test/bucket/key.txt".to_string(),
            version_id: Some("v1".to_string()),
            is_delete_marker: false,
        };
        let failure = RemovalFailureRecord {
            path: "test/bucket/key.txt".to_string(),
            version_id: Some("v2".to_string()),
            error_type: "conflict",
            message: "Governance retention denied deletion".to_string(),
        };
        let mut all_removed = Vec::new();
        let mut all_failed = Vec::new();
        let mut first_error_code = None;

        let critical = collect_removal_result(
            Err(RemovalError {
                code: ExitCode::Conflict,
                removed: vec![removed.clone()],
                failed: vec![failure.clone()],
            }),
            &mut all_removed,
            &mut all_failed,
            &mut first_error_code,
        );

        assert_eq!(all_removed, vec![removed]);
        assert_eq!(all_failed, vec![failure]);
        assert_eq!(first_error_code, Some(ExitCode::Conflict));
        assert_eq!(critical, None);
    }

    #[test]
    fn critical_removal_error_overrides_an_earlier_noncritical_error() {
        let mut removed = Vec::new();
        let mut failed = Vec::new();
        let mut aggregate_code = None;

        let not_found = RemovalError::failed(
            ExitCode::NotFound,
            removal_failure(
                "test/bucket/missing.txt",
                Some("missing-v1".to_string()),
                ExitCode::NotFound,
                "version missing",
            ),
        );
        assert_eq!(
            collect_removal_result(
                Err(not_found),
                &mut removed,
                &mut failed,
                &mut aggregate_code,
            ),
            None
        );

        let access_denied = RemovalError::failed(
            ExitCode::AuthError,
            removal_failure(
                "test/bucket/private.txt",
                Some("private-v1".to_string()),
                ExitCode::AuthError,
                "access denied",
            ),
        );
        assert_eq!(
            collect_removal_result(
                Err(access_denied),
                &mut removed,
                &mut failed,
                &mut aggregate_code,
            ),
            Some(ExitCode::AuthError)
        );
        assert_eq!(aggregate_code, Some(ExitCode::AuthError));
        assert_eq!(failed.len(), 2);
    }

    #[test]
    fn version_delete_reconciliation_consumes_duplicate_keys_once() {
        let mut requested = vec![
            ObjectVersionIdentifier {
                key: "key.txt".to_string(),
                version_id: Some("v1".to_string()),
                is_delete_marker: false,
            },
            ObjectVersionIdentifier {
                key: "key.txt".to_string(),
                version_id: Some("v2".to_string()),
                is_delete_marker: true,
            },
        ];

        let first = take_requested_version(&mut requested, "key.txt", None)
            .expect("the first unqualified delete result must match one request");
        let second = take_requested_version(&mut requested, "key.txt", None)
            .expect("the second unqualified delete result must match the remaining request");

        assert_eq!(first.version_id.as_deref(), Some("v1"));
        assert_eq!(second.version_id.as_deref(), Some("v2"));
        assert!(requested.is_empty());
    }

    #[test]
    fn version_remove_data_marks_dry_run_items_as_planned() {
        let planned = RemovalRecord {
            path: "test/bucket/key.txt".to_string(),
            version_id: Some("v1".to_string()),
            is_delete_marker: false,
        };

        let data = build_version_remove_data(true, vec![planned.clone()], Vec::new());
        let json = serde_json::to_value(data).expect("serialize version removal dry-run");

        assert_eq!(json["outcome"], "planned");
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["planned"][0]["version_id"], "v1");
        assert_eq!(json["removed"].as_array().map(Vec::len), Some(0));
        assert_eq!(json["summary"]["planned"], 1);
    }

    #[test]
    fn version_remove_data_marks_dry_run_discovery_errors_as_failed() {
        let planned = RemovalRecord {
            path: "test/bucket/key.txt".to_string(),
            version_id: Some("v1".to_string()),
            is_delete_marker: false,
        };
        let failure = removal_failure(
            "test/bucket/private.txt",
            None,
            ExitCode::AuthError,
            "access denied",
        );

        let data = build_version_remove_data(true, vec![planned], vec![failure]);
        let json = serde_json::to_value(data).expect("serialize failed version removal dry-run");

        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["planned"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["failed"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["removed"].as_array().map(Vec::len), Some(0));
    }
}
