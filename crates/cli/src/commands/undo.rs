//! Safe, history-preserving undo for versioned object PUT and DELETE operations.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Args;
use futures::stream::{self, StreamExt as _};
use rc_core::{
    AliasManager, CopyObjectOptions, DeleteRequestOptions, Error, ListObjectVersionsOptions,
    ObjectInfo, ObjectStore, ObjectVersion, ObjectVersionListResult, ParsedPath, RemotePath,
    UndoAction, UndoObjectResult, UndoOutcome, UndoPlan, UndoPlanItem, parse_path,
    plan_object_undo,
};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{
    Formatter, OutputConfig, V3ErrorEnvelope, V3PartialErrorEnvelope, V3SuccessEnvelope,
};

const DEFAULT_UNDO_CONCURRENCY: usize = 4;
const MAX_UNDO_CONCURRENCY: usize = 64;
const UNDO_AFTER_HELP: &str = "\
Examples:
  rc undo local/my-bucket/report.txt --dry-run
  rc undo local/my-bucket/report.txt
  rc undo local/my-bucket/report.txt --version-id VERSION_ID
  rc undo local/my-bucket/reports/ --recursive --concurrency 4";

/// Restore a versioned object operation without deleting data versions.
#[derive(Args, Clone, Debug)]
#[command(after_help = UNDO_AFTER_HELP)]
pub struct UndoArgs {
    /// Object or prefix path (alias/bucket/key)
    pub path: String,

    /// Restore this exact data version or remove this exact current delete marker
    #[arg(long, value_name = "VERSION_ID")]
    pub version_id: Option<String>,

    /// Plan all reversible objects under the prefix
    #[arg(short, long)]
    pub recursive: bool,

    /// Show the complete plan without making changes
    #[arg(long)]
    pub dry_run: bool,

    /// Maximum number of object mutations in flight
    #[arg(long, default_value_t = DEFAULT_UNDO_CONCURRENCY)]
    pub concurrency: usize,
}

#[derive(Debug, Serialize)]
struct UndoCommandData {
    operation: &'static str,
    outcome: &'static str,
    dry_run: bool,
    results: Vec<UndoObjectResult>,
    summary: UndoSummary,
}

#[derive(Debug, Serialize)]
struct UndoSummary {
    planned: usize,
    succeeded: usize,
    failed: usize,
}

#[async_trait]
trait UndoStore: Send + Sync {
    async fn versioning(&self, bucket: &str) -> rc_core::Result<Option<bool>>;

    async fn list_versions_page(
        &self,
        path: &RemotePath,
        options: &ListObjectVersionsOptions,
    ) -> rc_core::Result<ObjectVersionListResult>;

    async fn remove_version(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> rc_core::Result<()>;

    async fn copy_version(
        &self,
        path: &RemotePath,
        options: &CopyObjectOptions,
    ) -> rc_core::Result<ObjectInfo>;
}

#[async_trait]
impl UndoStore for S3Client {
    async fn versioning(&self, bucket: &str) -> rc_core::Result<Option<bool>> {
        ObjectStore::get_versioning(self, bucket).await
    }

    async fn list_versions_page(
        &self,
        path: &RemotePath,
        options: &ListObjectVersionsOptions,
    ) -> rc_core::Result<ObjectVersionListResult> {
        ObjectStore::list_object_versions_page_with_options(self, path, options).await
    }

    async fn remove_version(
        &self,
        path: &RemotePath,
        options: DeleteRequestOptions,
    ) -> rc_core::Result<()> {
        ObjectStore::delete_object_with_options(self, path, options).await?;
        Ok(())
    }

    async fn copy_version(
        &self,
        path: &RemotePath,
        options: &CopyObjectOptions,
    ) -> rc_core::Result<ObjectInfo> {
        ObjectStore::copy_object_with_options(self, path, path, options, None).await
    }
}

/// Execute the undo command.
pub async fn execute(args: UndoArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    if let Err(error) = validate_args(&args) {
        return fail(&formatter, ExitCode::UsageError, &error);
    }
    let path = match parse_remote_scope(&args) {
        Ok(path) => path,
        Err(error) => return fail(&formatter, ExitCode::UsageError, &error),
    };
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return fail(
                &formatter,
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
            );
        }
    };
    let alias = match alias_manager.get(&path.alias) {
        Ok(alias) => alias,
        Err(error) => return fail(&formatter, exit_code(&error), &error.to_string()),
    };
    let client = match S3Client::new(alias).await {
        Ok(client) => Arc::new(client),
        Err(error) => {
            return fail(
                &formatter,
                exit_code(&error),
                &format!("Failed to create S3 client: {error}"),
            );
        }
    };

    let report = match run_undo(client, &path, &args).await {
        Ok(report) => report,
        Err(error) => return fail(&formatter, exit_code(&error), &error.to_string()),
    };
    render_report(&formatter, report)
}

fn validate_args(args: &UndoArgs) -> Result<(), String> {
    if args.version_id.as_deref().is_some_and(str::is_empty) {
        return Err("--version-id cannot be empty".to_string());
    }
    if args.version_id.is_some() && args.recursive {
        return Err("--version-id cannot be combined with --recursive".to_string());
    }
    if args.concurrency == 0 || args.concurrency > MAX_UNDO_CONCURRENCY {
        return Err(format!(
            "--concurrency must be between 1 and {MAX_UNDO_CONCURRENCY}"
        ));
    }
    Ok(())
}

fn parse_remote_scope(args: &UndoArgs) -> Result<RemotePath, String> {
    let ParsedPath::Remote(path) = parse_path(&args.path).map_err(|error| error.to_string())?
    else {
        return Err("Undo requires a remote path in the form alias/bucket/key".to_string());
    };
    if path.key.is_empty() && !args.recursive {
        return Err("Bucket undo requires --recursive".to_string());
    }
    if !args.recursive && path.key.ends_with('/') {
        return Err("Prefix undo requires --recursive".to_string());
    }
    Ok(path)
}

async fn run_undo<S: UndoStore + 'static>(
    store: Arc<S>,
    path: &RemotePath,
    args: &UndoArgs,
) -> rc_core::Result<UndoCommandData> {
    match store.versioning(&path.bucket).await? {
        Some(true) => {}
        Some(false) => {
            return Err(Error::UnsupportedFeature(format!(
                "Undo requires enabled bucket versioning; versioning is suspended for {}",
                path.bucket
            )));
        }
        None => {
            return Err(Error::UnsupportedFeature(format!(
                "Undo requires enabled bucket versioning; {} is unversioned",
                path.bucket
            )));
        }
    }

    let history = list_all_versions(store.as_ref(), path).await?;
    let (plan, mut results) = build_plan(path, history, args);
    if plan.items.is_empty() && results.is_empty() {
        return Err(Error::NotFound(format!(
            "No object version history found for {}",
            path
        )));
    }
    let planned = plan.items.len();

    if args.dry_run {
        results.extend(plan.items.into_iter().map(|plan| UndoObjectResult {
            key: plan.key.clone(),
            plan: Some(plan),
            outcome: UndoOutcome::Planned,
        }));
    } else {
        let alias = path.alias.clone();
        let bucket = path.bucket.clone();
        let mut executed = stream::iter(plan.items.into_iter().map(|plan| {
            let store = Arc::clone(&store);
            let alias = alias.clone();
            let bucket = bucket.clone();
            async move { execute_plan_item(store.as_ref(), &alias, &bucket, plan).await }
        }))
        .buffer_unordered(args.concurrency)
        .collect::<Vec<_>>()
        .await;
        results.append(&mut executed);
    }
    results.sort_by(|left, right| left.key.cmp(&right.key));

    let failed = results
        .iter()
        .filter(|result| matches!(result.outcome, UndoOutcome::Failed { .. }))
        .count();
    let succeeded = results
        .iter()
        .filter(|result| {
            matches!(
                result.outcome,
                UndoOutcome::DeleteMarkerRemoved | UndoOutcome::VersionRestored { .. }
            )
        })
        .count();
    let outcome = if failed > 0 && (succeeded > 0 || (args.dry_run && planned > 0)) {
        "partial"
    } else if failed > 0 {
        "failed"
    } else if args.dry_run {
        "planned"
    } else {
        "success"
    };

    Ok(UndoCommandData {
        operation: "undo",
        outcome,
        dry_run: args.dry_run,
        results,
        summary: UndoSummary {
            planned,
            succeeded,
            failed,
        },
    })
}

fn build_plan(
    path: &RemotePath,
    history: Vec<ObjectVersion>,
    args: &UndoArgs,
) -> (UndoPlan, Vec<UndoObjectResult>) {
    let mut by_key = BTreeMap::<String, Vec<ObjectVersion>>::new();
    for version in history {
        if (args.recursive && version.key.starts_with(&path.key))
            || (!args.recursive && version.key == path.key)
        {
            by_key.entry(version.key.clone()).or_default().push(version);
        }
    }
    if !args.recursive && !by_key.contains_key(&path.key) {
        by_key.insert(path.key.clone(), Vec::new());
    }

    let mut plan = Vec::new();
    let mut failures = Vec::new();
    for (key, versions) in by_key {
        match plan_object_undo(&key, &versions, args.version_id.as_deref()) {
            Ok(item) => plan.push(item),
            Err(error) => failures.push(planning_failure(key, &error)),
        }
    }
    (UndoPlan::new(plan), failures)
}

async fn execute_plan_item<S: UndoStore>(
    store: &S,
    alias: &str,
    bucket: &str,
    plan: UndoPlanItem,
) -> UndoObjectResult {
    let path = RemotePath::new(alias, bucket, &plan.key);
    // S3 has no destination-version compare-and-swap for CopyObject or DeleteObject. Rechecking
    // immediately before mutation fails closed for observed changes, while the remaining
    // protocol-level race is tracked by rustfs/backlog#1438.
    let current = match list_all_versions(store, &path).await {
        Ok(history) => current_version_id(&plan.key, &history),
        Err(error) => return failed_result(plan, &error),
    };
    let current = match current {
        Ok(current) => current,
        Err(error) => return failed_result(plan, &error),
    };
    if current != plan.expected_latest_version_id {
        let error = Error::Conflict(format!(
            "Object changed after undo planning; expected current version '{}', found '{}'",
            plan.expected_latest_version_id, current
        ));
        return failed_result(plan, &error);
    }

    let result = match &plan.action {
        UndoAction::RemoveDeleteMarker {
            marker_version_id,
            revealed_version_id,
        } => store
            .remove_version(
                &path,
                DeleteRequestOptions {
                    version_id: Some(marker_version_id.clone()),
                    bypass_governance: false,
                    force_delete: false,
                },
            )
            .await
            .map(|()| {
                (
                    UndoOutcome::DeleteMarkerRemoved,
                    revealed_version_id.clone(),
                )
            }),
        UndoAction::RestoreVersion { source_version_id } => {
            let options = CopyObjectOptions {
                source_version_id: Some(source_version_id.clone()),
            };
            store.copy_version(&path, &options).await.and_then(|info| {
                let created_version_id = info.version_id.ok_or_else(|| {
                    Error::Conflict(format!(
                        "Versioned CopyObject did not return a destination version ID for {}",
                        plan.key
                    ))
                })?;
                Ok((
                    UndoOutcome::VersionRestored {
                        created_version_id: Some(created_version_id.clone()),
                    },
                    created_version_id,
                ))
            })
        }
    };

    match result {
        Ok((outcome, expected_current)) => {
            let observed_current = match list_all_versions(store, &path).await {
                Ok(history) => current_version_id(&plan.key, &history),
                Err(error) => return failed_result(plan, &error),
            };
            match observed_current {
                Ok(observed) if observed == expected_current => UndoObjectResult {
                    key: plan.key.clone(),
                    plan: Some(plan),
                    outcome,
                },
                Ok(observed) => {
                    let error = Error::Conflict(format!(
                        "Undo mutation completed but the current version changed; expected '{}', found '{}'",
                        expected_current, observed
                    ));
                    failed_result(plan, &error)
                }
                Err(error) => failed_result(plan, &error),
            }
        }
        Err(error) => failed_result(plan, &error),
    }
}

fn current_version_id(key: &str, history: &[ObjectVersion]) -> rc_core::Result<String> {
    let mut latest = history
        .iter()
        .filter(|version| version.key == key && version.is_latest);
    let version = latest
        .next()
        .ok_or_else(|| Error::Conflict(format!("No current version found for {key}")))?;
    if latest.next().is_some() {
        return Err(Error::Conflict(format!(
            "Multiple current versions were reported for {key}"
        )));
    }
    Ok(version.version_id.clone())
}

async fn list_all_versions<S: UndoStore>(
    store: &S,
    path: &RemotePath,
) -> rc_core::Result<Vec<ObjectVersion>> {
    let mut items = Vec::new();
    let mut key_marker = None;
    let mut version_id_marker = None;
    loop {
        let page = store
            .list_versions_page(
                path,
                &ListObjectVersionsOptions {
                    max_keys: Some(1000),
                    key_marker: key_marker.clone(),
                    version_id_marker: version_id_marker.clone(),
                },
            )
            .await?;
        items.extend(page.items);
        if !page.truncated {
            return Ok(items);
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
}

fn failed_result(plan: UndoPlanItem, error: &Error) -> UndoObjectResult {
    let blocked_version_id = is_lock_conflict(error).then(|| match &plan.action {
        UndoAction::RemoveDeleteMarker {
            marker_version_id, ..
        } => marker_version_id.clone(),
        UndoAction::RestoreVersion { source_version_id } => source_version_id.clone(),
    });
    let message = blocked_version_id.as_ref().map_or_else(
        || error.to_string(),
        |version_id| format!("{error} (blocked version: {version_id})"),
    );
    UndoObjectResult {
        key: plan.key.clone(),
        plan: Some(plan),
        outcome: UndoOutcome::Failed {
            error_type: error_type(exit_code(error)).to_string(),
            message,
            blocked_version_id,
        },
    }
}

fn planning_failure(key: String, error: &Error) -> UndoObjectResult {
    UndoObjectResult {
        key,
        plan: None,
        outcome: UndoOutcome::Failed {
            error_type: error_type(exit_code(error)).to_string(),
            message: error.to_string(),
            blocked_version_id: None,
        },
    }
}

fn exit_code(error: &Error) -> ExitCode {
    if is_lock_conflict(error) {
        ExitCode::Conflict
    } else {
        ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
    }
}

fn is_lock_conflict(error: &Error) -> bool {
    if matches!(error, Error::GovernanceDenied { .. }) {
        return true;
    }
    let message = error.to_string().to_ascii_lowercase();
    [
        "legal hold",
        "legalhold",
        "retention",
        "object lock",
        "governance",
        "worm",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn error_type(code: ExitCode) -> &'static str {
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

fn report_exit_code(report: &UndoCommandData) -> ExitCode {
    report
        .results
        .iter()
        .filter_map(|result| match &result.outcome {
            UndoOutcome::Failed { error_type, .. } => Some(match error_type.as_str() {
                "usage_error" => ExitCode::UsageError,
                "network_error" => ExitCode::NetworkError,
                "auth_error" => ExitCode::AuthError,
                "not_found" => ExitCode::NotFound,
                "conflict" => ExitCode::Conflict,
                "unsupported_feature" => ExitCode::UnsupportedFeature,
                "interrupted" => ExitCode::Interrupted,
                _ => ExitCode::GeneralError,
            }),
            _ => None,
        })
        .next()
        .unwrap_or(ExitCode::Success)
}

fn format_dry_run(plan: &UndoPlanItem) -> String {
    match &plan.action {
        UndoAction::RemoveDeleteMarker {
            marker_version_id,
            revealed_version_id,
        } => format!(
            "Would remove delete marker '{}' for {} (expected current '{}'; reveals '{}')",
            marker_version_id, plan.key, plan.expected_latest_version_id, revealed_version_id
        ),
        UndoAction::RestoreVersion { source_version_id } => format!(
            "Would restore {} from version '{}' (expected current '{}')",
            plan.key, source_version_id, plan.expected_latest_version_id
        ),
    }
}

fn render_report(formatter: &Formatter, report: UndoCommandData) -> ExitCode {
    let code = report_exit_code(&report);
    if formatter.is_json() {
        if code == ExitCode::Success {
            formatter.json(&V3SuccessEnvelope::versioned_objects(report));
        } else {
            formatter.json_error(&V3PartialErrorEnvelope::versioned_objects(
                code,
                "One or more object undo operations failed",
                Some("versioned_object_undo"),
                report,
            ));
        }
    } else {
        for result in &report.results {
            match &result.outcome {
                UndoOutcome::Planned => {
                    if let Some(plan) = &result.plan {
                        formatter.println(&format_dry_run(plan));
                    }
                }
                UndoOutcome::DeleteMarkerRemoved => {
                    formatter.success(&format!("Restored deleted object: {}", result.key));
                }
                UndoOutcome::VersionRestored { .. } => {
                    formatter.success(&format!("Restored object version: {}", result.key));
                }
                UndoOutcome::Failed { message, .. } => {
                    formatter.error(&format!("Failed to undo {}: {message}", result.key));
                }
            }
        }
    }
    code
}

fn fail(formatter: &Formatter, code: ExitCode, message: &str) -> ExitCode {
    if formatter.is_json() {
        formatter.json_error(&V3ErrorEnvelope::versioned_objects(
            code,
            message,
            Some("versioned_object_undo"),
        ));
        code
    } else {
        formatter.fail(code, message)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use jiff::Timestamp;

    use super::*;

    #[derive(Default)]
    struct FakeStore {
        versioning: Mutex<Option<bool>>,
        listings: Mutex<Vec<Vec<ObjectVersion>>>,
        removed: Mutex<Vec<String>>,
        copied: Mutex<Vec<String>>,
        fail_copy: Mutex<bool>,
    }

    impl FakeStore {
        fn with_listings(listings: Vec<Vec<ObjectVersion>>) -> Self {
            Self {
                versioning: Mutex::new(Some(true)),
                listings: Mutex::new(listings),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl UndoStore for FakeStore {
        async fn versioning(&self, _bucket: &str) -> rc_core::Result<Option<bool>> {
            Ok(*self.versioning.lock().expect("versioning lock"))
        }

        async fn list_versions_page(
            &self,
            _path: &RemotePath,
            _options: &ListObjectVersionsOptions,
        ) -> rc_core::Result<ObjectVersionListResult> {
            let items = self.listings.lock().expect("listings lock").remove(0);
            Ok(ObjectVersionListResult {
                items,
                truncated: false,
                continuation_token: None,
                version_id_marker: None,
            })
        }

        async fn remove_version(
            &self,
            _path: &RemotePath,
            options: DeleteRequestOptions,
        ) -> rc_core::Result<()> {
            self.removed
                .lock()
                .expect("removed lock")
                .push(options.version_id.expect("exact version"));
            Ok(())
        }

        async fn copy_version(
            &self,
            _path: &RemotePath,
            options: &CopyObjectOptions,
        ) -> rc_core::Result<ObjectInfo> {
            if *self.fail_copy.lock().expect("failure lock") {
                return Err(Error::Auth("copy denied".to_string()));
            }
            self.copied
                .lock()
                .expect("copied lock")
                .push(options.source_version_id.clone().expect("source version"));
            let mut info = ObjectInfo::file("report.txt", 12);
            info.version_id = Some("restored-v4".to_string());
            Ok(info)
        }
    }

    fn version(id: &str, modified: &str, latest: bool, marker: bool) -> ObjectVersion {
        ObjectVersion {
            key: "report.txt".to_string(),
            version_id: id.to_string(),
            is_latest: latest,
            is_delete_marker: marker,
            last_modified: Some(modified.parse::<Timestamp>().expect("valid timestamp")),
            size_bytes: (!marker).then_some(12),
            etag: None,
        }
    }

    fn restore_plan() -> UndoPlanItem {
        UndoPlanItem {
            key: "report.txt".to_string(),
            expected_latest_version_id: "v2".to_string(),
            action: UndoAction::RestoreVersion {
                source_version_id: "v1".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn concurrent_new_version_is_stale_and_performs_no_mutation() {
        let store = FakeStore::with_listings(vec![vec![
            version("v3", "2026-07-23T03:00:00Z", true, false),
            version("v2", "2026-07-23T02:00:00Z", false, false),
        ]]);

        let result = execute_plan_item(&store, "local", "bucket", restore_plan()).await;

        assert!(matches!(result.outcome, UndoOutcome::Failed { .. }));
        assert!(store.copied.lock().expect("copied lock").is_empty());
        assert!(store.removed.lock().expect("removed lock").is_empty());
    }

    #[tokio::test]
    async fn overwrite_restore_reports_created_version() {
        let store = FakeStore::with_listings(vec![
            vec![
                version("v2", "2026-07-23T02:00:00Z", true, false),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
            vec![
                version("restored-v4", "2026-07-23T04:00:00Z", true, false),
                version("v2", "2026-07-23T02:00:00Z", false, false),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
        ]);

        let result = execute_plan_item(&store, "local", "bucket", restore_plan()).await;

        assert!(matches!(
            result.outcome,
            UndoOutcome::VersionRestored {
                created_version_id: Some(ref id)
            } if id == "restored-v4"
        ));
        assert_eq!(
            *store.copied.lock().expect("copied lock"),
            ["v1".to_string()]
        );
    }

    #[tokio::test]
    async fn access_denial_is_preserved_as_per_object_failure() {
        let store = FakeStore::with_listings(vec![vec![
            version("v2", "2026-07-23T02:00:00Z", true, false),
            version("v1", "2026-07-23T01:00:00Z", false, false),
        ]]);
        *store.fail_copy.lock().expect("failure lock") = true;

        let result = execute_plan_item(&store, "local", "bucket", restore_plan()).await;

        assert!(matches!(
            result.outcome,
            UndoOutcome::Failed { ref error_type, .. } if error_type == "auth_error"
        ));
    }

    #[tokio::test]
    async fn delete_marker_execution_removes_only_the_planned_marker() {
        let store = FakeStore::with_listings(vec![
            vec![
                version("marker-v2", "2026-07-23T02:00:00Z", true, true),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
            vec![version("v1", "2026-07-23T01:00:00Z", true, false)],
        ]);
        let plan = UndoPlanItem {
            key: "report.txt".to_string(),
            expected_latest_version_id: "marker-v2".to_string(),
            action: UndoAction::RemoveDeleteMarker {
                marker_version_id: "marker-v2".to_string(),
                revealed_version_id: "v1".to_string(),
            },
        };

        let result = execute_plan_item(&store, "local", "bucket", plan).await;

        assert!(matches!(result.outcome, UndoOutcome::DeleteMarkerRemoved));
        assert_eq!(
            *store.removed.lock().expect("removed lock"),
            ["marker-v2".to_string()]
        );
        assert!(store.copied.lock().expect("copied lock").is_empty());
    }

    #[tokio::test]
    async fn retrying_the_same_plan_after_success_does_not_copy_twice() {
        let store = FakeStore::with_listings(vec![
            vec![
                version("v2", "2026-07-23T02:00:00Z", true, false),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
            vec![
                version("restored-v4", "2026-07-23T04:00:00Z", true, false),
                version("v2", "2026-07-23T02:00:00Z", false, false),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
            vec![
                version("restored-v4", "2026-07-23T04:00:00Z", true, false),
                version("v2", "2026-07-23T02:00:00Z", false, false),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
        ]);
        let plan = restore_plan();

        let first = execute_plan_item(&store, "local", "bucket", plan.clone()).await;
        let retry = execute_plan_item(&store, "local", "bucket", plan).await;

        assert!(matches!(first.outcome, UndoOutcome::VersionRestored { .. }));
        assert!(matches!(
            retry.outcome,
            UndoOutcome::Failed { ref error_type, .. } if error_type == "conflict"
        ));
        assert_eq!(
            *store.copied.lock().expect("copied lock"),
            ["v1".to_string()]
        );
    }

    #[tokio::test]
    async fn concurrent_version_after_mutation_fails_the_postcondition() {
        let store = FakeStore::with_listings(vec![
            vec![
                version("v2", "2026-07-23T02:00:00Z", true, false),
                version("v1", "2026-07-23T01:00:00Z", false, false),
            ],
            vec![
                version("external-v5", "2026-07-23T05:00:00Z", true, false),
                version("restored-v4", "2026-07-23T04:00:00Z", false, false),
                version("v2", "2026-07-23T02:00:00Z", false, false),
            ],
        ]);

        let result = execute_plan_item(&store, "local", "bucket", restore_plan()).await;

        assert!(matches!(
            result.outcome,
            UndoOutcome::Failed { ref error_type, .. } if error_type == "conflict"
        ));
        assert_eq!(
            *store.copied.lock().expect("copied lock"),
            ["v1".to_string()]
        );
    }

    #[tokio::test]
    async fn suspended_and_unversioned_buckets_are_refused_before_listing() {
        let args = UndoArgs {
            path: "local/bucket/report.txt".to_string(),
            version_id: None,
            recursive: false,
            dry_run: true,
            concurrency: 1,
        };
        let path = RemotePath::new("local", "bucket", "report.txt");

        for status in [Some(false), None] {
            let store = Arc::new(FakeStore::default());
            *store.versioning.lock().expect("versioning lock") = status;
            let error = run_undo(store, &path, &args)
                .await
                .expect_err("unsafe bucket state must fail");
            assert!(matches!(error, Error::UnsupportedFeature(_)));
        }
    }

    #[tokio::test]
    async fn recursive_dry_run_returns_the_complete_plan_without_mutations() {
        let mut report_v2 = version("report-v2", "2026-07-23T02:00:00Z", true, false);
        report_v2.key = "reports/report.txt".to_string();
        let mut report_v1 = version("report-v1", "2026-07-23T01:00:00Z", false, false);
        report_v1.key = "reports/report.txt".to_string();
        let mut log_marker = version("log-marker", "2026-07-23T02:00:00Z", true, true);
        log_marker.key = "reports/log.txt".to_string();
        let mut log_v1 = version("log-v1", "2026-07-23T01:00:00Z", false, false);
        log_v1.key = "reports/log.txt".to_string();
        let store = Arc::new(FakeStore::with_listings(vec![vec![
            report_v2, report_v1, log_marker, log_v1,
        ]]));
        let args = UndoArgs {
            path: "local/bucket/reports/".to_string(),
            version_id: None,
            recursive: true,
            dry_run: true,
            concurrency: 2,
        };
        let path = RemotePath::new("local", "bucket", "reports/");

        let report = run_undo(Arc::clone(&store), &path, &args)
            .await
            .expect("recursive dry-run");

        assert_eq!(report.summary.planned, 2);
        assert_eq!(report.summary.succeeded, 0);
        assert_eq!(report.summary.failed, 0);
        assert!(
            report
                .results
                .iter()
                .all(|result| matches!(result.outcome, UndoOutcome::Planned))
        );
        assert!(store.copied.lock().expect("copied lock").is_empty());
        assert!(store.removed.lock().expect("removed lock").is_empty());
    }

    #[test]
    fn partial_report_returns_the_failed_objects_exit_code() {
        let success_plan = restore_plan();
        let failed_plan = UndoPlanItem {
            key: "private.txt".to_string(),
            expected_latest_version_id: "private-v2".to_string(),
            action: UndoAction::RestoreVersion {
                source_version_id: "private-v1".to_string(),
            },
        };
        let report = UndoCommandData {
            operation: "undo",
            outcome: "partial",
            dry_run: false,
            results: vec![
                UndoObjectResult {
                    key: success_plan.key.clone(),
                    plan: Some(success_plan),
                    outcome: UndoOutcome::VersionRestored {
                        created_version_id: Some("restored-v4".to_string()),
                    },
                },
                UndoObjectResult {
                    key: failed_plan.key.clone(),
                    plan: Some(failed_plan),
                    outcome: UndoOutcome::Failed {
                        error_type: "auth_error".to_string(),
                        message: "copy denied".to_string(),
                        blocked_version_id: None,
                    },
                },
            ],
            summary: UndoSummary {
                planned: 2,
                succeeded: 1,
                failed: 1,
            },
        };

        assert_eq!(report_exit_code(&report), ExitCode::AuthError);
    }

    #[test]
    fn legal_hold_failures_are_conflicts_with_the_blocked_version() {
        let plan = UndoPlanItem {
            key: "report.txt".to_string(),
            expected_latest_version_id: "marker-v2".to_string(),
            action: UndoAction::RemoveDeleteMarker {
                marker_version_id: "marker-v2".to_string(),
                revealed_version_id: "v1".to_string(),
            },
        };
        let result = failed_result(
            plan,
            &Error::Auth("Legal hold prevents version deletion".to_string()),
        );

        assert!(matches!(
            result.outcome,
            UndoOutcome::Failed {
                ref error_type,
                blocked_version_id: Some(ref version_id),
                ref message,
            } if error_type == "conflict"
                && version_id == "marker-v2"
                && message.contains("blocked version: marker-v2")
        ));
    }

    #[test]
    fn dry_run_text_identifies_action_and_both_version_roles() {
        let restore = format_dry_run(&restore_plan());
        assert!(restore.contains("Would restore report.txt from version 'v1'"));
        assert!(restore.contains("expected current 'v2'"));

        let marker = format_dry_run(&UndoPlanItem {
            key: "report.txt".to_string(),
            expected_latest_version_id: "marker-v2".to_string(),
            action: UndoAction::RemoveDeleteMarker {
                marker_version_id: "marker-v2".to_string(),
                revealed_version_id: "v1".to_string(),
            },
        });
        assert!(marker.contains("remove delete marker 'marker-v2'"));
        assert!(marker.contains("reveals 'v1'"));
    }

    #[test]
    fn command_errors_have_stable_usage_and_conflict_exit_codes() {
        assert_eq!(
            exit_code(&Error::InvalidPath("bad selector".to_string())),
            ExitCode::UsageError
        );
        assert_eq!(
            exit_code(&Error::Conflict("stale plan".to_string())),
            ExitCode::Conflict
        );
    }

    #[test]
    fn selectors_and_concurrency_fail_with_usage_errors() {
        let args = UndoArgs {
            path: "local/bucket/report.txt".to_string(),
            version_id: Some("v1".to_string()),
            recursive: true,
            dry_run: false,
            concurrency: 4,
        };
        assert!(validate_args(&args).is_err());
        assert!(
            validate_args(&UndoArgs {
                version_id: None,
                recursive: false,
                concurrency: 0,
                ..args
            })
            .is_err()
        );
    }
}
