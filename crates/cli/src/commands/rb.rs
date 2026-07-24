//! rb command - Remove bucket
//!
//! Removes a bucket from the specified storage service.

use std::collections::HashSet;

use async_trait::async_trait;
use clap::Args;
use rc_core::{
    AbortMultipartUploadRequest, AliasManager, DeleteObjectsResult, DeleteRequestOptions, Error,
    ListObjectVersionsOptions, ListOptions, ListResult, MultipartUpload,
    MultipartUploadListOptions, MultipartUploadListResult, ObjectStore, ObjectVersionIdentifier,
    ObjectVersionListResult, RemotePath,
};
use rc_s3::S3Client;
use serde::Serialize;

use super::multipart::{
    MultipartCleanupOptions, cleanup_multipart_uploads, collect_multipart_uploads,
};
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const DELETE_BATCH_SIZE: usize = 1_000;
const BUCKET_REMOVE_SCHEMA_VERSION: u8 = 3;
const BUCKET_REMOVE_OUTPUT_TYPE: &str = "bucket_remove";

/// Remove a bucket.
#[derive(Args, Debug)]
pub struct RbArgs {
    /// Target path (alias/bucket)
    pub target: String,

    /// Delete bucket contents before removing the bucket
    #[arg(long)]
    pub force: bool,

    /// Permit aborting discovered incomplete multipart uploads
    #[arg(long, requires_all = ["force", "yes"])]
    pub dangerous: bool,

    /// Confirm the dangerous multipart cleanup
    #[arg(long, requires = "dangerous")]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CleanupOutcome {
    Success,
    Partial,
    Failed,
}

impl CleanupOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct DiscoverySummary {
    objects: usize,
    versions: usize,
    delete_markers: usize,
    multipart_uploads: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CleanupItemResult {
    target: String,
    kind: &'static str,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<CleanupError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CleanupError {
    #[serde(rename = "type")]
    error_type: &'static str,
    code: i32,
    message: String,
    retryable: bool,
}

impl CleanupError {
    fn new(code: ExitCode, message: impl Into<String>) -> Self {
        Self {
            error_type: exit_code_type(code),
            code: code.as_i32(),
            message: message.into(),
            retryable: matches!(code, ExitCode::NetworkError | ExitCode::Interrupted),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CleanupReport {
    bucket: String,
    force: bool,
    dangerous: bool,
    outcome: CleanupOutcome,
    completed_stages: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_stage: Option<&'static str>,
    discovery: DiscoverySummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining: Option<DiscoverySummary>,
    results: Vec<CleanupItemResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl CleanupReport {
    fn empty(bucket: String, force: bool, dangerous: bool) -> Self {
        Self {
            bucket,
            force,
            dangerous,
            outcome: CleanupOutcome::Failed,
            completed_stages: Vec::new(),
            failed_stage: None,
            discovery: DiscoverySummary::default(),
            remaining: None,
            results: Vec::new(),
            message: None,
        }
    }

    fn has_completed_mutation(&self) -> bool {
        self.results
            .iter()
            .any(|result| matches!(result.state, "deleted" | "aborted"))
    }

    fn fail(
        &mut self,
        stage: &'static str,
        code: ExitCode,
        message: impl Into<String>,
    ) -> ExitCode {
        self.failed_stage = Some(stage);
        self.outcome = if self.has_completed_mutation() {
            CleanupOutcome::Partial
        } else {
            CleanupOutcome::Failed
        };
        self.message = Some(message.into());
        code
    }
}

#[derive(Debug, Serialize)]
struct RbOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: &'a CleanupReport,
}

#[derive(Debug)]
struct DiscoveryPlan {
    deletion_targets: Vec<ObjectVersionIdentifier>,
    uploads: Vec<MultipartUpload>,
    summary: DiscoverySummary,
}

#[async_trait]
trait BucketCleanupStore: Sync {
    async fn list_objects_page(
        &self,
        path: &RemotePath,
        options: ListOptions,
    ) -> Result<ListResult, Error>;

    async fn list_versions_page(
        &self,
        path: &RemotePath,
        options: &ListObjectVersionsOptions,
    ) -> Result<ObjectVersionListResult, Error>;

    async fn list_uploads_page(
        &self,
        bucket: &str,
        options: MultipartUploadListOptions,
    ) -> Result<MultipartUploadListResult, Error>;

    async fn delete_versions(
        &self,
        bucket: &str,
        objects: Vec<ObjectVersionIdentifier>,
        options: DeleteRequestOptions,
    ) -> Result<DeleteObjectsResult, Error>;

    async fn abort_upload(&self, request: &AbortMultipartUploadRequest) -> Result<(), Error>;

    async fn delete_bucket(&self, bucket: &str) -> Result<(), Error>;
}

#[async_trait]
impl BucketCleanupStore for S3Client {
    async fn list_objects_page(
        &self,
        path: &RemotePath,
        options: ListOptions,
    ) -> Result<ListResult, Error> {
        ObjectStore::list_objects(self, path, options).await
    }

    async fn list_versions_page(
        &self,
        path: &RemotePath,
        options: &ListObjectVersionsOptions,
    ) -> Result<ObjectVersionListResult, Error> {
        ObjectStore::list_object_versions_page_with_options(self, path, options).await
    }

    async fn list_uploads_page(
        &self,
        bucket: &str,
        options: MultipartUploadListOptions,
    ) -> Result<MultipartUploadListResult, Error> {
        ObjectStore::list_multipart_uploads(self, bucket, options).await
    }

    async fn delete_versions(
        &self,
        bucket: &str,
        objects: Vec<ObjectVersionIdentifier>,
        options: DeleteRequestOptions,
    ) -> Result<DeleteObjectsResult, Error> {
        ObjectStore::delete_object_versions(self, bucket, objects, options).await
    }

    async fn abort_upload(&self, request: &AbortMultipartUploadRequest) -> Result<(), Error> {
        ObjectStore::abort_multipart_upload(self, request).await
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), Error> {
        ObjectStore::delete_bucket(self, bucket).await
    }
}

/// Execute the rb command.
pub async fn execute(args: RbArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Clap enforces the same relationships for normal parsing. This validation remains at the
    // execution boundary so direct callers cannot resolve an alias or create a client first.
    if let Err(message) = validate_destructive_guards(&args) {
        return formatter.fail(ExitCode::UsageError, message);
    }

    let (alias_name, bucket) = match parse_rb_path(&args.target) {
        Ok(parsed) => parsed,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };

    let alias_manager = match AliasManager::new() {
        Ok(alias_manager) => alias_manager,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
            );
        }
    };
    let alias = match alias_manager.get(&alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            return formatter.fail(
                ExitCode::NotFound,
                &format!("Alias '{alias_name}' not found"),
            );
        }
    };
    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {error}"),
            );
        }
    };

    if args.force {
        let (report, code) =
            run_forced_cleanup(&client, &alias_name, &bucket, args.dangerous).await;
        emit_report(&formatter, &report, code);
        return code;
    }

    let mut report = CleanupReport::empty(bucket.clone(), false, false);
    match ObjectStore::delete_bucket(&client, &bucket).await {
        Ok(()) => {
            report.completed_stages.push("bucket_delete");
            report.outcome = CleanupOutcome::Success;
            emit_report(&formatter, &report, ExitCode::Success);
            ExitCode::Success
        }
        Err(error) => {
            let code = exit_code_for_error(&error);
            report.fail(
                "bucket_delete",
                code,
                bucket_delete_message(&error, &bucket),
            );
            emit_report(&formatter, &report, code);
            code
        }
    }
}

fn validate_destructive_guards(args: &RbArgs) -> Result<(), &'static str> {
    if args.dangerous && (!args.force || !args.yes) {
        return Err("--dangerous requires both --force and --yes");
    }
    if args.yes && !args.dangerous {
        return Err("--yes is valid only with --dangerous --force");
    }
    Ok(())
}

async fn run_forced_cleanup(
    store: &dyn BucketCleanupStore,
    alias: &str,
    bucket: &str,
    dangerous: bool,
) -> (CleanupReport, ExitCode) {
    let mut report = CleanupReport::empty(bucket.to_string(), true, dangerous);
    let plan = match discover_bucket(store, alias, bucket).await {
        Ok(plan) => plan,
        Err(error) => {
            let code = exit_code_for_error(&error);
            report.fail(
                "discovery",
                code,
                format!("Failed to discover bucket contents: {error}"),
            );
            return (report, code);
        }
    };
    report.discovery = plan.summary.clone();
    report.completed_stages.push("discovery");

    if !plan.uploads.is_empty() && !dangerous {
        let code = report.fail(
            "multipart_guard",
            ExitCode::Conflict,
            format!(
                "Bucket contains {} incomplete multipart upload(s); rerun with --force --dangerous --yes to abort the precomputed set",
                plan.uploads.len()
            ),
        );
        return (report, code);
    }
    report.completed_stages.push("multipart_guard");

    if let Some(code) =
        delete_precomputed_objects(store, alias, bucket, &plan.deletion_targets, &mut report).await
    {
        return (report, code);
    }
    report.completed_stages.push("object_cleanup");

    if !plan.uploads.is_empty() {
        let cleanup = cleanup_multipart_uploads(
            plan.uploads,
            MultipartCleanupOptions::command_default(false),
            |request| async move { store.abort_upload(&request).await },
        )
        .await;
        for upload in cleanup.completed {
            report
                .results
                .push(upload_result(alias, &upload, "aborted", None));
        }
        let mut failure_code = None;
        for failure in cleanup.failed {
            let code = exit_code_for_error(&failure.error);
            select_failure_code(&mut failure_code, code);
            report.results.push(upload_result(
                alias,
                &failure.upload,
                "failed",
                Some(CleanupError::new(code, failure.error.to_string())),
            ));
        }
        if let Some(code) = failure_code {
            report.fail(
                "multipart_cleanup",
                code,
                "One or more multipart uploads could not be aborted; bucket deletion was not attempted",
            );
            return (report, code);
        }
    }
    report.completed_stages.push("multipart_cleanup");

    let residue = match discover_bucket(store, alias, bucket).await {
        Ok(residue) => residue,
        Err(error) => {
            let code = exit_code_for_error(&error);
            report.fail(
                "recheck",
                code,
                format!("Failed to verify that the bucket is empty: {error}"),
            );
            return (report, code);
        }
    };
    if !residue.deletion_targets.is_empty() || !residue.uploads.is_empty() {
        report.remaining = Some(residue.summary);
        let code = report.fail(
            "recheck",
            ExitCode::Conflict,
            "Concurrent writes left objects, versions, delete markers, or multipart uploads in the bucket; ordinary bucket deletion was not attempted",
        );
        return (report, code);
    }
    report.completed_stages.push("recheck");

    match store.delete_bucket(bucket).await {
        Ok(()) => {
            report.completed_stages.push("bucket_delete");
            report.outcome = CleanupOutcome::Success;
            report.message = None;
            (report, ExitCode::Success)
        }
        Err(error) => {
            let code = exit_code_for_error(&error);
            report.fail("bucket_delete", code, bucket_delete_message(&error, bucket));
            (report, code)
        }
    }
}

async fn discover_bucket(
    store: &dyn BucketCleanupStore,
    alias: &str,
    bucket: &str,
) -> Result<DiscoveryPlan, Error> {
    let path = RemotePath::new(alias, bucket, "");
    let object_keys = collect_object_keys(store, &path).await?;
    let mut versions = collect_versions(store, &path).await?;
    let uploads = collect_multipart_uploads(
        MultipartUploadListOptions {
            max_uploads: Some(1_000),
            ..MultipartUploadListOptions::default()
        },
        |options| store.list_uploads_page(bucket, options),
    )
    .await?;

    let version_keys = versions
        .iter()
        .map(|version| version.key.clone())
        .collect::<HashSet<_>>();
    versions.extend(
        object_keys
            .iter()
            .filter(|key| !version_keys.contains(key.as_str()))
            .map(|key| ObjectVersionIdentifier {
                key: key.clone(),
                version_id: None,
                is_delete_marker: false,
            }),
    );
    versions.sort_by(|left, right| deletion_identity(left).cmp(&deletion_identity(right)));
    versions.dedup_by(|left, right| deletion_identity(left) == deletion_identity(right));

    let summary = DiscoverySummary {
        objects: object_keys.len(),
        versions: versions
            .iter()
            .filter(|version| version.version_id.is_some() && !version.is_delete_marker)
            .count(),
        delete_markers: versions
            .iter()
            .filter(|version| version.is_delete_marker)
            .count(),
        multipart_uploads: uploads.len(),
    };
    Ok(DiscoveryPlan {
        deletion_targets: versions,
        uploads,
        summary,
    })
}

async fn collect_object_keys(
    store: &dyn BucketCleanupStore,
    path: &RemotePath,
) -> Result<Vec<String>, Error> {
    let mut keys = Vec::new();
    let mut continuation_token = None;
    let mut seen = HashSet::from([None]);
    loop {
        let page = store
            .list_objects_page(
                path,
                ListOptions {
                    max_keys: Some(1_000),
                    continuation_token: continuation_token.clone(),
                    recursive: true,
                    ..ListOptions::default()
                },
            )
            .await?;
        keys.extend(
            page.items
                .into_iter()
                .filter(|item| !item.is_dir)
                .map(|item| item.key),
        );
        if !page.truncated {
            break;
        }
        let next = page.continuation_token.ok_or_else(|| {
            Error::Network(
                "S3 returned a truncated object listing without a continuation token".to_string(),
            )
        })?;
        if continuation_token.as_deref() == Some(next.as_str()) {
            return Err(Error::Network(
                "S3 returned a truncated object listing without advancing its continuation token"
                    .to_string(),
            ));
        }
        continuation_token = Some(next);
        if !seen.insert(continuation_token.clone()) {
            return Err(Error::Network(
                "S3 returned an object listing continuation-token cycle".to_string(),
            ));
        }
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

async fn collect_versions(
    store: &dyn BucketCleanupStore,
    path: &RemotePath,
) -> Result<Vec<ObjectVersionIdentifier>, Error> {
    let mut targets = Vec::new();
    let mut key_marker = None;
    let mut version_id_marker = None;
    let mut seen = HashSet::from([(None, None)]);
    loop {
        let page = store
            .list_versions_page(
                path,
                &ListObjectVersionsOptions {
                    max_keys: Some(1_000),
                    key_marker: key_marker.clone(),
                    version_id_marker: version_id_marker.clone(),
                },
            )
            .await?;
        targets.extend(
            page.items
                .into_iter()
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
        let next = (Some(next_key_marker), page.version_id_marker);
        if next == (key_marker.clone(), version_id_marker.clone()) {
            return Err(Error::Network(
                "S3 returned a truncated version listing without advancing its markers".to_string(),
            ));
        }
        if !seen.insert(next.clone()) {
            return Err(Error::Network(
                "S3 returned a version listing pagination-marker cycle".to_string(),
            ));
        }
        (key_marker, version_id_marker) = next;
    }
    Ok(targets)
}

async fn delete_precomputed_objects(
    store: &dyn BucketCleanupStore,
    alias: &str,
    bucket: &str,
    targets: &[ObjectVersionIdentifier],
    report: &mut CleanupReport,
) -> Option<ExitCode> {
    for (batch_index, batch) in targets.chunks(DELETE_BATCH_SIZE).enumerate() {
        let requested = batch.to_vec();
        let options = DeleteRequestOptions {
            version_id: None,
            bypass_governance: false,
            force_delete: false,
        };
        let result = match store
            .delete_versions(bucket, requested.clone(), options)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let code = exit_code_for_error(&error);
                for target in &requested {
                    report.results.push(object_result(
                        alias,
                        bucket,
                        target,
                        "failed",
                        Some(CleanupError::new(code, error.to_string())),
                    ));
                }
                let remaining_offset = ((batch_index + 1) * DELETE_BATCH_SIZE).min(targets.len());
                append_not_attempted(alias, bucket, &targets[remaining_offset..], report);
                report.fail(
                    "object_cleanup",
                    code,
                    "An object deletion batch failed; multipart cleanup and bucket deletion were not attempted",
                );
                return Some(code);
            }
        };

        let mut unmatched = requested;
        for deleted in result.deleted {
            if let Some(target) =
                take_target(&mut unmatched, &deleted.key, deleted.version_id.as_deref())
            {
                report
                    .results
                    .push(object_result(alias, bucket, &target, "deleted", None));
            }
        }

        let mut failure_code = None;
        for failure in result.failures {
            let target = take_target(&mut unmatched, &failure.key, failure.version_id.as_deref())
                .unwrap_or(ObjectVersionIdentifier {
                    key: failure.key,
                    version_id: failure.version_id,
                    is_delete_marker: false,
                });
            let code =
                exit_code_for_delete_failure(failure.code.as_deref(), failure.message.as_deref());
            select_failure_code(&mut failure_code, code);
            let message = failure
                .message
                .or(failure.code)
                .unwrap_or_else(|| "The backend rejected the object deletion".to_string());
            report.results.push(object_result(
                alias,
                bucket,
                &target,
                "failed",
                Some(CleanupError::new(code, message)),
            ));
        }
        for target in unmatched {
            select_failure_code(&mut failure_code, ExitCode::GeneralError);
            report.results.push(object_result(
                alias,
                bucket,
                &target,
                "failed",
                Some(CleanupError::new(
                    ExitCode::GeneralError,
                    "The backend omitted the target from its delete result",
                )),
            ));
        }

        if let Some(code) = failure_code {
            let remaining_offset = ((batch_index + 1) * DELETE_BATCH_SIZE).min(targets.len());
            append_not_attempted(alias, bucket, &targets[remaining_offset..], report);
            report.fail(
                "object_cleanup",
                code,
                "One or more object versions or delete markers could not be deleted; multipart cleanup and bucket deletion were not attempted",
            );
            return Some(code);
        }
    }
    None
}

fn append_not_attempted(
    alias: &str,
    bucket: &str,
    targets: &[ObjectVersionIdentifier],
    report: &mut CleanupReport,
) {
    for target in targets {
        report.results.push(object_result(
            alias,
            bucket,
            target,
            "not_attempted",
            Some(CleanupError::new(
                ExitCode::Conflict,
                "Not attempted after an earlier object deletion failure",
            )),
        ));
    }
}

fn take_target(
    targets: &mut Vec<ObjectVersionIdentifier>,
    key: &str,
    version_id: Option<&str>,
) -> Option<ObjectVersionIdentifier> {
    let position = targets.iter().position(|target| {
        target.key == key
            && match version_id {
                Some(version_id) => target.version_id.as_deref() == Some(version_id),
                None => true,
            }
    })?;
    Some(targets.remove(position))
}

fn object_result(
    alias: &str,
    bucket: &str,
    target: &ObjectVersionIdentifier,
    state: &'static str,
    error: Option<CleanupError>,
) -> CleanupItemResult {
    CleanupItemResult {
        target: format!("{alias}/{bucket}/{}", target.key),
        kind: if target.is_delete_marker {
            "delete_marker"
        } else if target.version_id.is_some() {
            "version"
        } else {
            "object"
        },
        state,
        version_id: target.version_id.clone(),
        upload_id: None,
        error,
    }
}

fn upload_result(
    alias: &str,
    upload: &MultipartUpload,
    state: &'static str,
    error: Option<CleanupError>,
) -> CleanupItemResult {
    CleanupItemResult {
        target: format!("{alias}/{}/{}", upload.bucket, upload.key),
        kind: "multipart_upload",
        state,
        version_id: None,
        upload_id: Some(upload.upload_id.clone()),
        error,
    }
}

fn deletion_identity(target: &ObjectVersionIdentifier) -> (&str, Option<&str>, bool) {
    (
        target.key.as_str(),
        target.version_id.as_deref(),
        target.is_delete_marker,
    )
}

fn emit_report(formatter: &Formatter, report: &CleanupReport, code: ExitCode) {
    if formatter.is_json() {
        formatter.json(&RbOutput {
            schema_version: BUCKET_REMOVE_SCHEMA_VERSION,
            output_type: BUCKET_REMOVE_OUTPUT_TYPE,
            status: report.outcome.as_str(),
            data: report,
        });
        return;
    }

    if !report.completed_stages.is_empty() {
        formatter.println(&format!(
            "Completed stages: {}",
            report.completed_stages.join(", ")
        ));
    }
    if let Some(failed_stage) = report.failed_stage {
        formatter.println(&format!("Failed stage: {failed_stage}"));
    }
    for line in human_report_lines(report) {
        formatter.println(&formatter.sanitize_text(&line));
    }
    if code == ExitCode::Success {
        formatter.success(&format!(
            "Bucket '{}' removed successfully.",
            formatter.sanitize_text(&report.bucket)
        ));
    } else {
        formatter.error_with_code(
            code,
            report.message.as_deref().unwrap_or("Bucket removal failed"),
        );
    }
}

fn human_report_lines(report: &CleanupReport) -> Vec<String> {
    let mut lines = Vec::new();
    for result in &report.results {
        let identity = result
            .version_id
            .as_deref()
            .map(|version| format!(" version {version}"))
            .or_else(|| {
                result
                    .upload_id
                    .as_deref()
                    .map(|upload| format!(" upload {upload}"))
            })
            .unwrap_or_default();
        let detail = result
            .error
            .as_ref()
            .map(|error| format!(": {}", error.message))
            .unwrap_or_default();
        lines.push(format!(
            "{} {}{} [{}]{}",
            result.state, result.target, identity, result.kind, detail
        ));
    }
    lines
}

fn bucket_delete_message(error: &Error, bucket: &str) -> String {
    if matches!(error, Error::Conflict(_)) {
        format!(
            "Bucket '{bucket}' is not empty after verification; a concurrent writer may have added new state"
        )
    } else {
        format!("Failed to remove bucket '{bucket}': {error}")
    }
}

fn exit_code_for_error(error: &Error) -> ExitCode {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => ExitCode::UsageError,
        Error::Network(_) => ExitCode::NetworkError,
        Error::Auth(_) => ExitCode::AuthError,
        Error::NotFound(_)
        | Error::VersionNotFound { .. }
        | Error::DeleteMarker { .. }
        | Error::AliasNotFound(_) => ExitCode::NotFound,
        Error::Conflict(_) | Error::GovernanceDenied { .. } | Error::AliasExists(_) => {
            ExitCode::Conflict
        }
        Error::UnsupportedFeature(_) => ExitCode::UnsupportedFeature,
        Error::Interrupted(_) => ExitCode::Interrupted,
        Error::Io(_)
        | Error::TomlParse(_)
        | Error::TomlSerialize(_)
        | Error::Json(_)
        | Error::InvalidUrl(_)
        | Error::General(_)
        | Error::RequestRejected(_) => ExitCode::GeneralError,
    }
}

fn exit_code_for_delete_failure(code: Option<&str>, message: Option<&str>) -> ExitCode {
    let normalized = format!(
        "{} {}",
        code.unwrap_or_default(),
        message.unwrap_or_default()
    )
    .to_ascii_lowercase();
    if [
        "legal hold",
        "governance",
        "compliance",
        "retention",
        "object lock",
        "worm",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        ExitCode::Conflict
    } else if matches!(
        code,
        Some("AccessDenied") | Some("Forbidden") | Some("Unauthorized")
    ) || normalized.contains("access denied")
        || normalized.contains("forbidden")
        || normalized.contains("unauthorized")
    {
        ExitCode::AuthError
    } else if matches!(
        code,
        Some("NoSuchVersion") | Some("NoSuchKey") | Some("NotFound")
    ) {
        ExitCode::NotFound
    } else {
        ExitCode::GeneralError
    }
}

fn select_failure_code(selected: &mut Option<ExitCode>, candidate: ExitCode) {
    let rank = |code| match code {
        ExitCode::Conflict => 7,
        ExitCode::AuthError => 6,
        ExitCode::Interrupted => 5,
        ExitCode::NetworkError => 4,
        ExitCode::UnsupportedFeature => 3,
        ExitCode::NotFound => 2,
        ExitCode::UsageError => 1,
        ExitCode::GeneralError | ExitCode::Success => 0,
    };
    if selected.is_none_or(|current| rank(candidate) > rank(current)) {
        *selected = Some(candidate);
    }
}

const fn exit_code_type(code: ExitCode) -> &'static str {
    match code {
        ExitCode::Success => "success",
        ExitCode::GeneralError => "general_error",
        ExitCode::UsageError => "usage_error",
        ExitCode::NetworkError => "network_error",
        ExitCode::AuthError => "auth_error",
        ExitCode::NotFound => "not_found",
        ExitCode::Conflict => "conflict",
        ExitCode::UnsupportedFeature => "unsupported_feature",
        ExitCode::Interrupted => "interrupted",
    }
}

/// Parse rb target path into (alias, bucket).
fn parse_rb_path(path: &str) -> Result<(String, String), String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }
    let parts = path.splitn(2, '/').collect::<Vec<_>>();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid path format: '{path}'. Expected: alias/bucket"
        ));
    }
    if parts[0].is_empty() {
        return Err("Alias name cannot be empty".to_string());
    }
    if parts[1].is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }
    if parts[1].contains('/') {
        return Err("Bucket removal target cannot include an object key".to_string());
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use rc_core::{
        DeleteObjectFailure, DeletedObject, MultipartUploadListResult, ObjectInfo, ObjectVersion,
    };

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        ListObjects,
        ListVersions,
        ListUploads,
        Delete {
            count: usize,
            force_delete: bool,
            bypass_governance: bool,
        },
        Abort(String),
        DeleteBucket,
    }

    #[derive(Default)]
    struct FakeStore {
        calls: Mutex<Vec<Call>>,
        object_pages: Mutex<VecDeque<Result<ListResult, Error>>>,
        version_pages: Mutex<VecDeque<Result<ObjectVersionListResult, Error>>>,
        upload_pages: Mutex<VecDeque<Result<MultipartUploadListResult, Error>>>,
        delete_results: Mutex<VecDeque<Result<DeleteObjectsResult, Error>>>,
        abort_results: Mutex<VecDeque<Result<(), Error>>>,
        delete_bucket_results: Mutex<VecDeque<Result<(), Error>>>,
    }

    impl FakeStore {
        fn empty() -> Self {
            Self {
                object_pages: Mutex::new(VecDeque::from([
                    Ok(ListResult {
                        items: Vec::new(),
                        truncated: false,
                        continuation_token: None,
                    }),
                    Ok(ListResult {
                        items: Vec::new(),
                        truncated: false,
                        continuation_token: None,
                    }),
                ])),
                version_pages: Mutex::new(VecDeque::from([
                    Ok(ObjectVersionListResult {
                        items: Vec::new(),
                        truncated: false,
                        continuation_token: None,
                        version_id_marker: None,
                    }),
                    Ok(ObjectVersionListResult {
                        items: Vec::new(),
                        truncated: false,
                        continuation_token: None,
                        version_id_marker: None,
                    }),
                ])),
                upload_pages: Mutex::new(VecDeque::from([
                    Ok(empty_upload_page()),
                    Ok(empty_upload_page()),
                ])),
                delete_bucket_results: Mutex::new(VecDeque::from([Ok(())])),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl BucketCleanupStore for FakeStore {
        async fn list_objects_page(
            &self,
            _path: &RemotePath,
            _options: ListOptions,
        ) -> Result<ListResult, Error> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .push(Call::ListObjects);
            self.object_pages
                .lock()
                .expect("object page lock should not be poisoned")
                .pop_front()
                .expect("object page should exist")
        }

        async fn list_versions_page(
            &self,
            _path: &RemotePath,
            _options: &ListObjectVersionsOptions,
        ) -> Result<ObjectVersionListResult, Error> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .push(Call::ListVersions);
            self.version_pages
                .lock()
                .expect("version page lock should not be poisoned")
                .pop_front()
                .expect("version page should exist")
        }

        async fn list_uploads_page(
            &self,
            _bucket: &str,
            _options: MultipartUploadListOptions,
        ) -> Result<MultipartUploadListResult, Error> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .push(Call::ListUploads);
            self.upload_pages
                .lock()
                .expect("upload page lock should not be poisoned")
                .pop_front()
                .expect("upload page should exist")
        }

        async fn delete_versions(
            &self,
            _bucket: &str,
            objects: Vec<ObjectVersionIdentifier>,
            options: DeleteRequestOptions,
        ) -> Result<DeleteObjectsResult, Error> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .push(Call::Delete {
                    count: objects.len(),
                    force_delete: options.force_delete,
                    bypass_governance: options.bypass_governance,
                });
            self.delete_results
                .lock()
                .expect("delete result lock should not be poisoned")
                .pop_front()
                .expect("delete result should exist")
        }

        async fn abort_upload(&self, request: &AbortMultipartUploadRequest) -> Result<(), Error> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .push(Call::Abort(request.upload_id.clone()));
            self.abort_results
                .lock()
                .expect("abort result lock should not be poisoned")
                .pop_front()
                .unwrap_or(Ok(()))
        }

        async fn delete_bucket(&self, _bucket: &str) -> Result<(), Error> {
            self.calls
                .lock()
                .expect("call lock should not be poisoned")
                .push(Call::DeleteBucket);
            self.delete_bucket_results
                .lock()
                .expect("bucket delete lock should not be poisoned")
                .pop_front()
                .expect("bucket delete result should exist")
        }
    }

    fn empty_upload_page() -> MultipartUploadListResult {
        MultipartUploadListResult {
            uploads: Vec::new(),
            common_prefixes: Vec::new(),
            truncated: false,
            next_key_marker: None,
            next_upload_id_marker: None,
        }
    }

    fn upload(key: &str, upload_id: &str) -> MultipartUpload {
        MultipartUpload {
            bucket: "bucket".to_string(),
            key: key.to_string(),
            upload_id: upload_id.to_string(),
            initiated: None,
            size_bytes: None,
            storage_class: None,
            initiator: None,
            owner: None,
            checksum_algorithm: None,
            checksum_type: None,
        }
    }

    fn version(key: &str, version_id: &str, marker: bool) -> ObjectVersion {
        ObjectVersion {
            key: key.to_string(),
            version_id: version_id.to_string(),
            is_latest: false,
            is_delete_marker: marker,
            last_modified: None,
            size_bytes: None,
            etag: None,
        }
    }

    fn args(force: bool, dangerous: bool, yes: bool) -> RbArgs {
        RbArgs {
            target: "local/bucket".to_string(),
            force,
            dangerous,
            yes,
        }
    }

    #[tokio::test]
    async fn dangerous_guards_fail_before_alias_or_network_resolution() {
        let code = execute(
            args(false, true, true),
            OutputConfig {
                quiet: true,
                ..OutputConfig::default()
            },
        )
        .await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn direct_call_guard_validation_rejects_every_incomplete_combination() {
        assert_eq!(
            validate_destructive_guards(&args(false, true, true)),
            Err("--dangerous requires both --force and --yes")
        );
        assert_eq!(
            validate_destructive_guards(&args(true, true, false)),
            Err("--dangerous requires both --force and --yes")
        );
        assert_eq!(
            validate_destructive_guards(&args(true, false, true)),
            Err("--yes is valid only with --dangerous --force")
        );
    }

    #[tokio::test]
    async fn discovery_merges_unversioned_null_version_and_delete_marker_targets() {
        let store = FakeStore {
            object_pages: Mutex::new(VecDeque::from([Ok(ListResult {
                items: vec![
                    ObjectInfo::file("plain", 1),
                    ObjectInfo::file("suspended", 1),
                ],
                truncated: false,
                continuation_token: None,
            })])),
            version_pages: Mutex::new(VecDeque::from([Ok(ObjectVersionListResult {
                items: vec![
                    version("versioned", "v1", false),
                    version("suspended", "null", false),
                    version("gone", "m1", true),
                ],
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            })])),
            upload_pages: Mutex::new(VecDeque::from([Ok(empty_upload_page())])),
            ..FakeStore::default()
        };

        let plan = discover_bucket(&store, "local", "bucket")
            .await
            .expect("all version states should be discoverable");

        assert_eq!(
            plan.deletion_targets,
            vec![
                ObjectVersionIdentifier {
                    key: "gone".to_string(),
                    version_id: Some("m1".to_string()),
                    is_delete_marker: true,
                },
                ObjectVersionIdentifier {
                    key: "plain".to_string(),
                    version_id: None,
                    is_delete_marker: false,
                },
                ObjectVersionIdentifier {
                    key: "suspended".to_string(),
                    version_id: Some("null".to_string()),
                    is_delete_marker: false,
                },
                ObjectVersionIdentifier {
                    key: "versioned".to_string(),
                    version_id: Some("v1".to_string()),
                    is_delete_marker: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn discovers_every_category_before_refusing_multipart_without_mutation() {
        let store = FakeStore {
            object_pages: Mutex::new(VecDeque::from([Ok(ListResult {
                items: vec![ObjectInfo::file("plain", 1)],
                truncated: false,
                continuation_token: None,
            })])),
            version_pages: Mutex::new(VecDeque::from([Ok(ObjectVersionListResult {
                items: vec![
                    version("versioned", "v1", false),
                    version("gone", "m1", true),
                ],
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            })])),
            upload_pages: Mutex::new(VecDeque::from([Ok(MultipartUploadListResult {
                uploads: vec![upload("pending", "u1")],
                ..empty_upload_page()
            })])),
            ..FakeStore::default()
        };

        let (report, code) = run_forced_cleanup(&store, "local", "bucket", false).await;

        assert_eq!(code, ExitCode::Conflict);
        assert_eq!(
            report.discovery,
            DiscoverySummary {
                objects: 1,
                versions: 1,
                delete_markers: 1,
                multipart_uploads: 1,
            }
        );
        assert_eq!(
            store.calls(),
            vec![Call::ListObjects, Call::ListVersions, Call::ListUploads]
        );
    }

    #[tokio::test]
    async fn deletes_deterministic_batches_without_force_header_or_retention_bypass() {
        let targets = (0..1_001)
            .map(|index| version(&format!("key-{index:04}"), "null", false))
            .collect::<Vec<_>>();
        let first_deleted = targets[..1_000]
            .iter()
            .map(|target| DeletedObject {
                key: target.key.clone(),
                version_id: Some(target.version_id.clone()),
                is_delete_marker: false,
            })
            .collect();
        let second_deleted = targets[1_000..]
            .iter()
            .map(|target| DeletedObject {
                key: target.key.clone(),
                version_id: Some(target.version_id.clone()),
                is_delete_marker: false,
            })
            .collect();
        let mut store = FakeStore::empty();
        store.version_pages = Mutex::new(VecDeque::from([
            Ok(ObjectVersionListResult {
                items: targets,
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            }),
            Ok(ObjectVersionListResult {
                items: Vec::new(),
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            }),
        ]));
        store.delete_results = Mutex::new(VecDeque::from([
            Ok(DeleteObjectsResult {
                deleted: first_deleted,
                failures: Vec::new(),
            }),
            Ok(DeleteObjectsResult {
                deleted: second_deleted,
                failures: Vec::new(),
            }),
        ]));

        let (report, code) = run_forced_cleanup(&store, "local", "bucket", false).await;

        assert_eq!(code, ExitCode::Success);
        assert_eq!(report.outcome, CleanupOutcome::Success);
        let calls = store.calls();
        let delete_calls = calls
            .iter()
            .cloned()
            .filter_map(|call| match call {
                Call::Delete {
                    count,
                    force_delete,
                    bypass_governance,
                } => Some((count, force_delete, bypass_governance)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(delete_calls, vec![(1_000, false, false), (1, false, false)]);
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::ListObjects))
                .count(),
            2
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::ListVersions))
                .count(),
            2
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::ListUploads))
                .count(),
            2
        );
        assert!(matches!(calls.last(), Some(Call::DeleteBucket)));
    }

    #[tokio::test]
    async fn partial_object_failure_stops_before_later_batches_multipart_and_bucket_delete() {
        let targets = (0..1_001)
            .map(|index| version(&format!("key-{index:04}"), "v1", false))
            .collect::<Vec<_>>();
        let mut store = FakeStore {
            object_pages: Mutex::new(VecDeque::from([Ok(ListResult {
                items: Vec::new(),
                truncated: false,
                continuation_token: None,
            })])),
            version_pages: Mutex::new(VecDeque::from([Ok(ObjectVersionListResult {
                items: targets,
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            })])),
            upload_pages: Mutex::new(VecDeque::from([Ok(MultipartUploadListResult {
                uploads: vec![upload("pending", "u1")],
                ..empty_upload_page()
            })])),
            ..FakeStore::default()
        };
        let completed = (1..1_000)
            .map(|index| DeletedObject {
                key: format!("key-{index:04}"),
                version_id: Some("v1".to_string()),
                is_delete_marker: false,
            })
            .collect();
        store.delete_results = Mutex::new(VecDeque::from([Ok(DeleteObjectsResult {
            deleted: completed,
            failures: vec![DeleteObjectFailure {
                key: "key-0000".to_string(),
                version_id: Some("v1".to_string()),
                code: Some("AccessDenied".to_string()),
                message: Some("object lock compliance retention is active".to_string()),
            }],
        })]));

        let (report, code) = run_forced_cleanup(&store, "local", "bucket", true).await;

        assert_eq!(code, ExitCode::Conflict);
        assert_eq!(report.outcome, CleanupOutcome::Partial);
        assert_eq!(report.failed_stage, Some("object_cleanup"));
        assert!(
            report
                .results
                .iter()
                .any(|result| result.state == "not_attempted")
        );
        let calls = store.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| matches!(call, Call::Delete { .. }))
                .count(),
            1
        );
        assert!(!calls.iter().any(|call| matches!(call, Call::Abort(_))));
        assert!(!calls.iter().any(|call| matches!(call, Call::DeleteBucket)));
    }

    #[tokio::test]
    async fn multipart_abort_failure_preserves_partial_results_and_blocks_bucket_delete() {
        let mut store = FakeStore {
            object_pages: Mutex::new(VecDeque::from([Ok(ListResult {
                items: Vec::new(),
                truncated: false,
                continuation_token: None,
            })])),
            version_pages: Mutex::new(VecDeque::from([Ok(ObjectVersionListResult {
                items: Vec::new(),
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            })])),
            upload_pages: Mutex::new(VecDeque::from([Ok(MultipartUploadListResult {
                uploads: vec![upload("a", "u1"), upload("b", "u2")],
                ..empty_upload_page()
            })])),
            ..FakeStore::default()
        };
        store.abort_results = Mutex::new(VecDeque::from([
            Ok(()),
            Err(Error::Auth("AccessDenied".to_string())),
        ]));

        let (report, code) = run_forced_cleanup(&store, "local", "bucket", true).await;

        assert_eq!(code, ExitCode::AuthError);
        assert_eq!(report.outcome, CleanupOutcome::Partial);
        assert_eq!(report.failed_stage, Some("multipart_cleanup"));
        assert!(
            report
                .results
                .iter()
                .any(|result| result.state == "aborted")
        );
        assert!(report.results.iter().any(|result| result.state == "failed"));
        let calls = store.calls();
        let mut aborted = calls
            .iter()
            .filter_map(|call| match call {
                Call::Abort(upload_id) => Some(upload_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        aborted.sort_unstable();
        assert_eq!(aborted, vec!["u1", "u2"]);
        assert!(!calls.iter().any(|call| matches!(call, Call::DeleteBucket)));
    }

    #[tokio::test]
    async fn four_way_recheck_blocks_delete_when_a_concurrent_object_appears() {
        let mut store = FakeStore::empty();
        store.object_pages = Mutex::new(VecDeque::from([
            Ok(ListResult {
                items: Vec::new(),
                truncated: false,
                continuation_token: None,
            }),
            Ok(ListResult {
                items: vec![ObjectInfo::file("raced", 1)],
                truncated: false,
                continuation_token: None,
            }),
        ]));

        let (report, code) = run_forced_cleanup(&store, "local", "bucket", false).await;

        assert_eq!(code, ExitCode::Conflict);
        assert_eq!(report.failed_stage, Some("recheck"));
        assert_eq!(report.discovery.objects, 0);
        assert_eq!(
            report.remaining.as_ref().map(|remaining| remaining.objects),
            Some(1)
        );
        assert!(
            !store
                .calls()
                .iter()
                .any(|call| matches!(call, Call::DeleteBucket))
        );
    }

    #[tokio::test]
    async fn pagination_cycles_fail_deterministically_before_mutation() {
        let store = FakeStore {
            object_pages: Mutex::new(VecDeque::from([
                Ok(ListResult {
                    items: Vec::new(),
                    truncated: true,
                    continuation_token: Some("a".to_string()),
                }),
                Ok(ListResult {
                    items: Vec::new(),
                    truncated: true,
                    continuation_token: Some("b".to_string()),
                }),
                Ok(ListResult {
                    items: Vec::new(),
                    truncated: true,
                    continuation_token: Some("a".to_string()),
                }),
            ])),
            ..FakeStore::default()
        };

        let (report, code) = run_forced_cleanup(&store, "local", "bucket", false).await;

        assert_eq!(code, ExitCode::NetworkError);
        assert_eq!(report.failed_stage, Some("discovery"));
        assert!(
            store
                .calls()
                .iter()
                .all(|call| matches!(call, Call::ListObjects))
        );
    }

    #[test]
    fn output_v3_preserves_partial_status_stages_and_safe_targets() {
        let report = CleanupReport {
            bucket: "bucket".to_string(),
            force: true,
            dangerous: true,
            outcome: CleanupOutcome::Partial,
            completed_stages: vec!["discovery", "multipart_guard"],
            failed_stage: Some("object_cleanup"),
            discovery: DiscoverySummary {
                objects: 1,
                versions: 1,
                delete_markers: 0,
                multipart_uploads: 1,
            },
            remaining: None,
            results: vec![CleanupItemResult {
                target: "local/bucket/key".to_string(),
                kind: "version",
                state: "failed",
                version_id: Some("v1".to_string()),
                upload_id: None,
                error: Some(CleanupError::new(
                    ExitCode::Conflict,
                    "legal hold is active",
                )),
            }],
            message: Some("cleanup stopped".to_string()),
        };
        let json = serde_json::to_value(RbOutput {
            schema_version: BUCKET_REMOVE_SCHEMA_VERSION,
            output_type: BUCKET_REMOVE_OUTPUT_TYPE,
            status: report.outcome.as_str(),
            data: &report,
        })
        .expect("v3 output should serialize");

        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["type"], "bucket_remove");
        assert_eq!(json["status"], "partial");
        assert_eq!(json["data"]["failed_stage"], "object_cleanup");
        assert_eq!(json["data"]["results"][0]["error"]["code"], 6);
    }

    #[test]
    fn human_partial_output_includes_item_state_kind_and_error() {
        let mut report = CleanupReport::empty("bucket".to_string(), true, true);
        report.results.push(CleanupItemResult {
            target: "local/bucket/key".to_string(),
            kind: "delete_marker",
            state: "failed",
            version_id: Some("m1".to_string()),
            upload_id: None,
            error: Some(CleanupError::new(
                ExitCode::Conflict,
                "compliance retention is active",
            )),
        });

        let lines = human_report_lines(&report);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("failed local/bucket/key version m1 [delete_marker]"));
        assert!(lines[0].contains("compliance retention"));
    }

    #[test]
    fn human_output_sanitizes_server_control_characters_before_emission() {
        let mut report = CleanupReport::empty("bucket".to_string(), true, true);
        report.results.push(CleanupItemResult {
            target: "local/bucket/key\u{1b}[31m\nnext".to_string(),
            kind: "object",
            state: "failed",
            version_id: None,
            upload_id: None,
            error: Some(CleanupError::new(ExitCode::GeneralError, "bad\rbackend")),
        });
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..OutputConfig::default()
        });

        let rendered = formatter.sanitize_text(&human_report_lines(&report)[0]);

        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.contains("\\u{1b}"));
        assert!(rendered.contains("\\nnext"));
        assert!(rendered.contains("bad\\rbackend"));
    }

    #[test]
    fn parse_rb_path_rejects_object_keys_and_empty_aliases() {
        assert_eq!(
            parse_rb_path("myalias/mybucket/"),
            Ok(("myalias".to_string(), "mybucket".to_string()))
        );
        assert!(parse_rb_path("/bucket").is_err());
        assert!(parse_rb_path("local/bucket/key").is_err());
        assert!(parse_rb_path("myalias").is_err());
        assert!(parse_rb_path("").is_err());
    }

    #[test]
    fn retention_and_auth_failures_use_distinct_exit_codes() {
        assert_eq!(
            exit_code_for_delete_failure(
                Some("AccessDenied"),
                Some("Object Lock legal hold is active")
            ),
            ExitCode::Conflict
        );
        assert_eq!(
            exit_code_for_delete_failure(Some("AccessDenied"), Some("permission denied")),
            ExitCode::AuthError
        );
    }
}
