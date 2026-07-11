//! mirror command - Synchronize objects between two locations
//!
//! Mirrors objects from source to destination, optionally removing extra files.

use clap::Args;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rc_core::{AliasManager, ListOptions, ObjectStore as _, ParsedPath, RemotePath, parse_path};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::{JoinError, JoinSet};

use crate::commands::diff::{DiffEntry, DiffStatus};
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Synchronize objects between two locations
#[derive(Args, Debug)]
pub struct MirrorArgs {
    /// Source path (alias/bucket/prefix)
    pub source: String,

    /// Destination path (alias/bucket/prefix)
    pub target: String,

    /// Remove extra objects at destination
    #[arg(long)]
    pub remove: bool,

    /// Overwrite existing objects
    #[arg(long)]
    pub overwrite: bool,

    /// Dry run (show what would be done without doing it)
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Number of parallel operations
    #[arg(short = 'P', long, default_value = "4")]
    pub parallel: usize,

    /// Disable progress bar
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Debug, Serialize)]
struct MirrorOutput {
    source: String,
    target: String,
    copied: usize,
    removed: usize,
    skipped: usize,
    errors: usize,
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct FileInfo {
    size: Option<i64>,
    modified: Option<String>,
    etag: Option<String>,
}

struct MirrorTaskContext<'a> {
    success_marker: &'a str,
    operation: &'a str,
    quiet: bool,
    formatter: &'a Formatter,
    progress: Option<&'a ProgressBar>,
}

trait MirrorCopySource {
    async fn head_object_for_mirror(
        &self,
        path: &RemotePath,
    ) -> rc_core::Result<rc_core::ObjectInfo>;

    async fn get_object_for_mirror(&self, path: &RemotePath) -> rc_core::Result<Vec<u8>>;
}

trait MirrorCopyTarget {
    async fn put_object_for_mirror(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
        if_absent: bool,
    ) -> rc_core::Result<rc_core::ObjectInfo>;
}

impl MirrorCopySource for S3Client {
    async fn head_object_for_mirror(
        &self,
        path: &RemotePath,
    ) -> rc_core::Result<rc_core::ObjectInfo> {
        rc_core::ObjectStore::head_object(self, path).await
    }

    async fn get_object_for_mirror(&self, path: &RemotePath) -> rc_core::Result<Vec<u8>> {
        rc_core::ObjectStore::get_object(self, path).await
    }
}

impl MirrorCopyTarget for S3Client {
    async fn put_object_for_mirror(
        &self,
        path: &RemotePath,
        data: Vec<u8>,
        content_type: Option<&str>,
        if_absent: bool,
    ) -> rc_core::Result<rc_core::ObjectInfo> {
        if if_absent {
            self.put_object_if_absent(path, data, content_type).await
        } else {
            rc_core::ObjectStore::put_object(self, path, data, content_type, None).await
        }
    }
}

/// Execute the mirror command
pub async fn execute(args: MirrorArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if !(1..=256).contains(&args.parallel) {
        formatter.error("--parallel must be between 1 and 256");
        return ExitCode::UsageError;
    }

    // Parse both paths
    let source_parsed = parse_path(&args.source);
    let target_parsed = parse_path(&args.target);

    // Both must be remote for now
    let (source_path, target_path) = match (&source_parsed, &target_parsed) {
        (Ok(ParsedPath::Remote(s)), Ok(ParsedPath::Remote(t))) => (s.clone(), t.clone()),
        (Ok(ParsedPath::Local(_)), _) | (_, Ok(ParsedPath::Local(_))) => {
            formatter.error("Local paths are not yet supported in mirror command");
            return ExitCode::UsageError;
        }
        (Err(e), _) => {
            formatter.error(&format!("Invalid source path: {e}"));
            return ExitCode::UsageError;
        }
        (_, Err(e)) => {
            formatter.error(&format!("Invalid target path: {e}"));
            return ExitCode::UsageError;
        }
    };

    // Load aliases
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    // Create clients
    let source_alias = match alias_manager.get(&source_path.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", source_path.alias));
            return ExitCode::NotFound;
        }
    };

    let target_alias = match alias_manager.get(&target_path.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", target_path.alias));
            return ExitCode::NotFound;
        }
    };

    if mirror_locations_overlap(
        &source_path,
        &target_path,
        &source_alias.endpoint,
        &target_alias.endpoint,
    ) {
        formatter.error("Mirror source and destination prefixes must not overlap");
        return ExitCode::UsageError;
    }

    let source_client = Arc::new(match S3Client::new(source_alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create source client: {e}"));
            return ExitCode::NetworkError;
        }
    });

    let target_client = Arc::new(match S3Client::new(target_alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create target client: {e}"));
            return ExitCode::NetworkError;
        }
    });

    // List objects from both paths
    let source_objects = match list_objects_map(&source_client, &source_path).await {
        Ok(o) => o,
        Err(e) => {
            formatter.error(&format!("Failed to list source: {e}"));
            return ExitCode::NetworkError;
        }
    };

    let target_objects = match list_objects_map(&target_client, &target_path).await {
        Ok(o) => o,
        Err(e) => {
            formatter.error(&format!("Failed to list target: {e}"));
            return ExitCode::NetworkError;
        }
    };

    // Compare and determine operations
    let diff_entries = compare_objects_internal(&source_objects, &target_objects);

    let mut to_copy: Vec<(&str, &FileInfo, bool)> = Vec::new();
    let mut to_remove: Vec<(&str, &FileInfo)> = Vec::new();
    let mut skipped = 0;

    for entry in &diff_entries {
        match entry.status {
            DiffStatus::OnlyFirst => {
                // New object, copy it
                if let Some(info) = source_objects.get(&entry.key) {
                    to_copy.push((&entry.key, info, true));
                }
            }
            DiffStatus::Different => {
                if args.overwrite {
                    // Different and overwrite enabled, copy it
                    if let Some(info) = source_objects.get(&entry.key) {
                        to_copy.push((&entry.key, info, false));
                    }
                } else {
                    skipped += 1;
                }
            }
            DiffStatus::OnlySecond => {
                if args.remove {
                    // Extra object at destination, remove it
                    if let Some(info) = target_objects.get(&entry.key) {
                        to_remove.push((&entry.key, info));
                    }
                }
            }
            DiffStatus::Same => {
                skipped += 1;
            }
        }
    }

    // Dry run output
    if args.dry_run {
        if !formatter.is_json() {
            formatter.println("Dry run mode - no changes will be made:");
            formatter.println("");

            if !to_copy.is_empty() {
                formatter.println(&format!("Would copy {} object(s):", to_copy.len()));
                for (key, info, _) in &to_copy {
                    let size = info
                        .size
                        .map(|s| humansize::format_size(s as u64, humansize::BINARY))
                        .unwrap_or_default();
                    formatter.println(&format!("  + {} ({size})", formatter.sanitize_text(key)));
                }
                formatter.println("");
            }

            if !to_remove.is_empty() {
                formatter.println(&format!("Would remove {} object(s):", to_remove.len()));
                for (key, _) in &to_remove {
                    formatter.println(&format!("  - {}", formatter.sanitize_text(key)));
                }
                formatter.println("");
            }

            formatter.println(&format!(
                "Summary: {} to copy, {} to remove, {} skipped",
                to_copy.len(),
                to_remove.len(),
                skipped
            ));
        } else {
            let output = MirrorOutput {
                source: args.source.clone(),
                target: args.target.clone(),
                copied: to_copy.len(),
                removed: to_remove.len(),
                skipped,
                errors: 0,
                dry_run: true,
            };
            formatter.json(&output);
        }
        return ExitCode::Success;
    }

    // Progress bar setup
    let multi_progress = if !args.quiet && !formatter.is_json() {
        Some(MultiProgress::new())
    } else {
        None
    };

    let overall_pb = multi_progress.as_ref().map(|mp| {
        let pb = mp.add(ProgressBar::new((to_copy.len() + to_remove.len()) as u64));
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                .expect("Valid template")
                .progress_chars("#>-"),
        );
        pb.set_message("Syncing...");
        pb
    });

    // Perform copy operations
    let mut copied = 0;
    let mut errors = 0;

    let parallel_limit = args.parallel.max(1);
    let mut copy_tasks: JoinSet<(String, Result<(), String>)> = JoinSet::new();
    let copy_context = MirrorTaskContext {
        success_marker: "+",
        operation: "copy",
        quiet: args.quiet,
        formatter: &formatter,
        progress: overall_pb.as_ref(),
    };

    for (key, _, if_absent) in to_copy {
        let source_sep = if source_path.key.is_empty() || source_path.key.ends_with('/') {
            ""
        } else {
            "/"
        };
        let source_full = RemotePath::new(
            &source_path.alias,
            &source_path.bucket,
            format!("{}{source_sep}{key}", source_path.key),
        );

        let target_sep = if target_path.key.is_empty() || target_path.key.ends_with('/') {
            ""
        } else {
            "/"
        };
        let target_full = RemotePath::new(
            &target_path.alias,
            &target_path.bucket,
            format!("{}{target_sep}{key}", target_path.key),
        );

        let key = key.to_string();
        let source_client = Arc::clone(&source_client);
        let target_client = Arc::clone(&target_client);
        copy_tasks.spawn(async move {
            let result = copy_object_with_metadata(
                source_client.as_ref(),
                target_client.as_ref(),
                &source_full,
                &target_full,
                if_absent,
            )
            .await
            .map_err(|e| format!("Failed to copy {key}: {e}"));
            (key, result)
        });

        if copy_tasks.len() >= parallel_limit
            && let Some(task_result) = copy_tasks.join_next().await
        {
            record_mirror_task(task_result, &copy_context, &mut copied, &mut errors);
        }
    }

    while let Some(task_result) = copy_tasks.join_next().await {
        record_mirror_task(task_result, &copy_context, &mut copied, &mut errors);
    }

    // Perform remove operations
    let mut removed = 0;

    if args.remove && errors == 0 {
        let mut remove_tasks: JoinSet<(String, Result<(), String>)> = JoinSet::new();
        let remove_context = MirrorTaskContext {
            success_marker: "-",
            operation: "remove",
            quiet: args.quiet,
            formatter: &formatter,
            progress: overall_pb.as_ref(),
        };

        for (key, info) in to_remove {
            let sep = if target_path.key.is_empty() || target_path.key.ends_with('/') {
                ""
            } else {
                "/"
            };
            let target_full = RemotePath::new(
                &target_path.alias,
                &target_path.bucket,
                format!("{}{sep}{key}", target_path.key),
            );

            let key = key.to_string();
            let etag = info.etag.clone();
            let target_client = Arc::clone(&target_client);
            remove_tasks.spawn(async move {
                let result = match etag {
                    Some(etag) => {
                        target_client
                            .delete_object_if_match(&target_full, &etag)
                            .await
                    }
                    None => Err(rc_core::Error::Conflict(format!(
                        "Refusing to remove {key} because its ETag is unavailable"
                    ))),
                }
                .map_err(|e| format!("Failed to remove {key}: {e}"));
                (key, result)
            });

            if remove_tasks.len() >= parallel_limit
                && let Some(task_result) = remove_tasks.join_next().await
            {
                record_mirror_task(task_result, &remove_context, &mut removed, &mut errors);
            }
        }

        while let Some(task_result) = remove_tasks.join_next().await {
            record_mirror_task(task_result, &remove_context, &mut removed, &mut errors);
        }
    } else if args.remove && errors > 0 && !formatter.is_json() {
        formatter.error("Skipping removals because one or more copy operations failed");
    }

    if let Some(pb) = overall_pb {
        pb.finish_with_message("Done");
    }

    // Output results
    if formatter.is_json() {
        let output = MirrorOutput {
            source: args.source.clone(),
            target: args.target.clone(),
            copied,
            removed,
            skipped,
            errors,
            dry_run: false,
        };
        formatter.json(&output);
    } else {
        formatter.println("");
        formatter.println(&format!(
            "Mirror complete: {copied} copied, {removed} removed, {skipped} skipped, {errors} errors"
        ));
    }

    if errors > 0 {
        ExitCode::GeneralError
    } else {
        ExitCode::Success
    }
}

fn record_mirror_task(
    task_result: Result<(String, Result<(), String>), JoinError>,
    context: &MirrorTaskContext<'_>,
    succeeded: &mut usize,
    errors: &mut usize,
) {
    match task_result {
        Ok((key, Ok(()))) => {
            *succeeded += 1;
            if !context.quiet && !context.formatter.is_json() {
                context.formatter.println(&format!(
                    "{} {}",
                    context.success_marker,
                    context.formatter.sanitize_text(&key)
                ));
            }
        }
        Ok((_, Err(message))) => {
            *errors += 1;
            if !context.formatter.is_json() {
                context.formatter.error(&message);
            }
        }
        Err(join_error) => {
            *errors += 1;
            if !context.formatter.is_json() {
                context.formatter.error(&format!(
                    "Mirror {} worker failed: {join_error}",
                    context.operation
                ));
            }
        }
    }

    if let Some(progress) = context.progress {
        progress.inc(1);
    }
}

// Mirror should preserve source content type when metadata is available, but
// still fall back to a plain byte copy if metadata lookup fails for any reason.
async fn copy_object_with_metadata<S, T>(
    source_client: &S,
    target_client: &T,
    source_path: &RemotePath,
    target_path: &RemotePath,
    if_absent: bool,
) -> rc_core::Result<()>
where
    S: MirrorCopySource,
    T: MirrorCopyTarget,
{
    let content_type = match source_client.head_object_for_mirror(source_path).await {
        Ok(source_info) => source_info.content_type,
        Err(error) => {
            tracing::warn!(
                source_alias = %source_path.alias,
                source_bucket = %source_path.bucket,
                source_key = %source_path.key,
                error = %error,
                "Falling back to mirror upload without source content type"
            );
            None
        }
    };

    let data = source_client.get_object_for_mirror(source_path).await?;
    target_client
        .put_object_for_mirror(target_path, data, content_type.as_deref(), if_absent)
        .await?;
    Ok(())
}

async fn list_objects_map(
    client: &S3Client,
    path: &RemotePath,
) -> Result<HashMap<String, FileInfo>, rc_core::Error> {
    let mut objects = HashMap::new();
    let mut continuation_token: Option<String> = None;
    let base_prefix = &path.key;

    loop {
        let options = ListOptions {
            recursive: true,
            max_keys: Some(1000),
            continuation_token: continuation_token.clone(),
            ..Default::default()
        };

        let result = client.list_objects(path, options).await?;

        for item in result.items {
            if item.is_dir {
                continue;
            }

            // Get relative key (remove base prefix)
            let relative_key = item.key.strip_prefix(base_prefix).unwrap_or(&item.key);
            let relative_key = relative_key.trim_start_matches('/').to_string();

            if relative_key.is_empty() {
                continue;
            }

            objects.insert(
                relative_key,
                FileInfo {
                    size: item.size_bytes,
                    modified: item.last_modified.map(|t| t.to_string()),
                    etag: item.etag,
                },
            );
        }

        if result.truncated {
            continuation_token = result.continuation_token;
        } else {
            break;
        }
    }

    Ok(objects)
}

fn compare_objects_internal(
    source: &HashMap<String, FileInfo>,
    target: &HashMap<String, FileInfo>,
) -> Vec<DiffEntry> {
    let mut entries = Vec::new();

    // Check objects in source
    for (key, source_info) in source {
        if let Some(target_info) = target.get(key) {
            // Object exists in both
            let is_same = source_info.size == target_info.size
                && matches!(
                    (&source_info.etag, &target_info.etag),
                    (Some(source_etag), Some(target_etag)) if source_etag == target_etag
                );

            let status = if is_same {
                DiffStatus::Same
            } else {
                DiffStatus::Different
            };

            entries.push(DiffEntry {
                key: key.clone(),
                status,
                first_size: source_info.size,
                second_size: target_info.size,
                first_modified: source_info.modified.clone(),
                second_modified: target_info.modified.clone(),
            });
        } else {
            // Only in source
            entries.push(DiffEntry {
                key: key.clone(),
                status: DiffStatus::OnlyFirst,
                first_size: source_info.size,
                second_size: None,
                first_modified: source_info.modified.clone(),
                second_modified: None,
            });
        }
    }

    // Check objects only in target
    for (key, target_info) in target {
        if !source.contains_key(key) {
            entries.push(DiffEntry {
                key: key.clone(),
                status: DiffStatus::OnlySecond,
                first_size: None,
                second_size: target_info.size,
                first_modified: None,
                second_modified: target_info.modified.clone(),
            });
        }
    }

    entries.sort_by(|a, b| a.key.cmp(&b.key));
    entries
}

fn mirror_locations_overlap(
    source: &RemotePath,
    target: &RemotePath,
    source_endpoint: &str,
    target_endpoint: &str,
) -> bool {
    if source.bucket != target.bucket || !same_endpoint(source_endpoint, target_endpoint) {
        return false;
    }

    let source = source.key.trim_matches('/');
    let target = target.key.trim_matches('/');
    if source.is_empty() || target.is_empty() || source == target {
        return true;
    }

    target
        .strip_prefix(source)
        .is_some_and(|rest| rest.starts_with('/'))
        || source
            .strip_prefix(target)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn same_endpoint(first: &str, second: &str) -> bool {
    match (url::Url::parse(first), url::Url::parse(second)) {
        (Ok(first), Ok(second)) => {
            first.scheme().eq_ignore_ascii_case(second.scheme())
                && first.host_str().zip(second.host_str()).is_some_and(
                    |(first_host, second_host)| first_host.eq_ignore_ascii_case(second_host),
                )
                && first.port_or_known_default() == second.port_or_known_default()
        }
        _ => first
            .trim_end_matches('/')
            .eq_ignore_ascii_case(second.trim_end_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::ObjectInfo;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct TestMirrorSource {
        content_type: Option<String>,
        data: Vec<u8>,
        head_error: Option<rc_core::Error>,
        head_calls: Mutex<Vec<String>>,
        get_calls: Mutex<Vec<String>>,
    }

    impl TestMirrorSource {
        fn new(content_type: Option<&str>, data: &[u8]) -> Self {
            Self {
                content_type: content_type.map(ToOwned::to_owned),
                data: data.to_vec(),
                head_error: None,
                head_calls: Mutex::new(Vec::new()),
                get_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_head_error(error: rc_core::Error, data: &[u8]) -> Self {
            Self {
                content_type: None,
                data: data.to_vec(),
                head_error: Some(error),
                head_calls: Mutex::new(Vec::new()),
                get_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl MirrorCopySource for TestMirrorSource {
        async fn head_object_for_mirror(&self, path: &RemotePath) -> rc_core::Result<ObjectInfo> {
            self.head_calls
                .lock()
                .expect("head call mutex should not be poisoned")
                .push(path.key.clone());
            if let Some(error) = &self.head_error {
                return Err(rc_core::Error::General(error.to_string()));
            }
            Ok(ObjectInfo {
                key: path.key.clone(),
                size_bytes: Some(self.data.len() as i64),
                size_human: None,
                last_modified: None,
                etag: None,
                storage_class: None,
                content_type: self.content_type.clone(),
                metadata: None,
                is_dir: false,
            })
        }

        async fn get_object_for_mirror(&self, path: &RemotePath) -> rc_core::Result<Vec<u8>> {
            self.get_calls
                .lock()
                .expect("get call mutex should not be poisoned")
                .push(path.key.clone());
            Ok(self.data.clone())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct PutCall {
        key: String,
        content_type: Option<String>,
        data: Vec<u8>,
        if_absent: bool,
    }

    #[derive(Debug, Default)]
    struct TestMirrorTarget {
        put_calls: Mutex<Vec<PutCall>>,
    }

    impl MirrorCopyTarget for TestMirrorTarget {
        async fn put_object_for_mirror(
            &self,
            path: &RemotePath,
            data: Vec<u8>,
            content_type: Option<&str>,
            if_absent: bool,
        ) -> rc_core::Result<ObjectInfo> {
            self.put_calls
                .lock()
                .expect("put call mutex should not be poisoned")
                .push(PutCall {
                    key: path.key.clone(),
                    content_type: content_type.map(ToOwned::to_owned),
                    data: data.clone(),
                    if_absent,
                });

            Ok(ObjectInfo::file(path.key.clone(), data.len() as i64))
        }
    }

    #[tokio::test]
    async fn test_copy_object_with_metadata_preserves_content_type() {
        let source = TestMirrorSource::new(Some("image/jpeg"), b"jpeg-bytes");
        let target = TestMirrorTarget::default();
        let source_path = RemotePath::new("stage", "images", "photo.jpg");
        let target_path = RemotePath::new("prod", "images", "photo.jpg");

        copy_object_with_metadata(&source, &target, &source_path, &target_path, true)
            .await
            .expect("copy should succeed");

        assert_eq!(
            source
                .head_calls
                .lock()
                .expect("head call mutex should not be poisoned")
                .as_slice(),
            ["photo.jpg"]
        );
        assert_eq!(
            source
                .get_calls
                .lock()
                .expect("get call mutex should not be poisoned")
                .as_slice(),
            ["photo.jpg"]
        );
        assert_eq!(
            target
                .put_calls
                .lock()
                .expect("put call mutex should not be poisoned")
                .as_slice(),
            [PutCall {
                key: "photo.jpg".to_string(),
                content_type: Some("image/jpeg".to_string()),
                data: b"jpeg-bytes".to_vec(),
                if_absent: true,
            }]
        );
    }

    #[tokio::test]
    async fn test_copy_object_with_metadata_passes_none_when_source_has_no_content_type() {
        let source = TestMirrorSource::new(None, b"plain-bytes");
        let target = TestMirrorTarget::default();
        let source_path = RemotePath::new("stage", "docs", "readme.txt");
        let target_path = RemotePath::new("prod", "docs", "readme.txt");

        copy_object_with_metadata(&source, &target, &source_path, &target_path, false)
            .await
            .expect("copy should succeed");

        assert_eq!(
            target
                .put_calls
                .lock()
                .expect("put call mutex should not be poisoned")
                .as_slice(),
            [PutCall {
                key: "readme.txt".to_string(),
                content_type: None,
                data: b"plain-bytes".to_vec(),
                if_absent: false,
            }]
        );
    }

    #[tokio::test]
    async fn test_copy_object_with_metadata_falls_back_when_head_lookup_fails() {
        let source = TestMirrorSource::with_head_error(
            rc_core::Error::Network("head failed".to_string()),
            b"plain-bytes",
        );
        let target = TestMirrorTarget::default();
        let source_path = RemotePath::new("stage", "docs", "readme.txt");
        let target_path = RemotePath::new("prod", "docs", "readme.txt");

        copy_object_with_metadata(&source, &target, &source_path, &target_path, false)
            .await
            .expect("copy should succeed");

        assert_eq!(
            source
                .head_calls
                .lock()
                .expect("head call mutex should not be poisoned")
                .as_slice(),
            ["readme.txt"]
        );
        assert_eq!(
            source
                .get_calls
                .lock()
                .expect("get call mutex should not be poisoned")
                .as_slice(),
            ["readme.txt"]
        );
        assert_eq!(
            target
                .put_calls
                .lock()
                .expect("put call mutex should not be poisoned")
                .as_slice(),
            [PutCall {
                key: "readme.txt".to_string(),
                content_type: None,
                data: b"plain-bytes".to_vec(),
                if_absent: false,
            }]
        );
    }

    #[test]
    fn test_compare_objects_internal() {
        let mut source = HashMap::new();
        source.insert(
            "file1.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc".to_string()),
            },
        );
        source.insert(
            "file2.txt".to_string(),
            FileInfo {
                size: Some(200),
                modified: None,
                etag: Some("def".to_string()),
            },
        );

        let mut target = HashMap::new();
        target.insert(
            "file1.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc".to_string()),
            },
        );
        target.insert(
            "file3.txt".to_string(),
            FileInfo {
                size: Some(300),
                modified: None,
                etag: Some("ghi".to_string()),
            },
        );

        let entries = compare_objects_internal(&source, &target);
        assert_eq!(entries.len(), 3);

        // file1.txt should be Same
        let f1 = entries.iter().find(|e| e.key == "file1.txt").unwrap();
        assert_eq!(f1.status, DiffStatus::Same);

        // file2.txt should be OnlyFirst
        let f2 = entries.iter().find(|e| e.key == "file2.txt").unwrap();
        assert_eq!(f2.status, DiffStatus::OnlyFirst);

        // file3.txt should be OnlySecond
        let f3 = entries.iter().find(|e| e.key == "file3.txt").unwrap();
        assert_eq!(f3.status, DiffStatus::OnlySecond);
    }

    #[test]
    fn test_compare_empty_source() {
        let source: HashMap<String, FileInfo> = HashMap::new();
        let mut target = HashMap::new();
        target.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc".to_string()),
            },
        );

        let entries = compare_objects_internal(&source, &target);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::OnlySecond);
    }

    #[test]
    fn test_compare_empty_target() {
        let mut source = HashMap::new();
        source.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc".to_string()),
            },
        );
        let target: HashMap<String, FileInfo> = HashMap::new();

        let entries = compare_objects_internal(&source, &target);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::OnlyFirst);
    }

    #[test]
    fn test_compare_both_empty() {
        let source: HashMap<String, FileInfo> = HashMap::new();
        let target: HashMap<String, FileInfo> = HashMap::new();

        let entries = compare_objects_internal(&source, &target);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_compare_different_sizes() {
        let mut source = HashMap::new();
        source.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc".to_string()),
            },
        );

        let mut target = HashMap::new();
        target.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(200), // Different size
                modified: None,
                etag: Some("def".to_string()),
            },
        );

        let entries = compare_objects_internal(&source, &target);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn test_compare_missing_etag_is_different() {
        let source = HashMap::from([(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: None,
            },
        )]);
        let target = HashMap::from([(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("target-etag".to_string()),
            },
        )]);

        let entries = compare_objects_internal(&source, &target);

        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn mirror_rejects_overlapping_prefixes_across_aliases_for_same_endpoint() {
        let source = RemotePath::new("source", "bucket", "data/source");
        let nested = RemotePath::new("target", "bucket", "data/source/archive");
        let adjacent = RemotePath::new("target", "bucket", "data/source-archive");

        assert!(mirror_locations_overlap(
            &source,
            &nested,
            "https://s3.example.com/",
            "https://S3.EXAMPLE.COM:443"
        ));
        assert!(!mirror_locations_overlap(
            &source,
            &adjacent,
            "https://s3.example.com",
            "https://s3.example.com"
        ));
    }

    #[test]
    fn test_mirror_args_defaults() {
        let args = MirrorArgs {
            source: "src".to_string(),
            target: "dst".to_string(),
            remove: false,
            overwrite: false,
            dry_run: false,
            parallel: 4,
            quiet: false,
        };
        assert_eq!(args.parallel, 4);
        assert!(!args.remove);
        assert!(!args.overwrite);
    }

    #[test]
    fn test_mirror_output_serialization() {
        let output = MirrorOutput {
            source: "src/".to_string(),
            target: "dst/".to_string(),
            copied: 10,
            removed: 2,
            skipped: 5,
            errors: 0,
            dry_run: false,
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"copied\":10"));
        assert!(json.contains("\"removed\":2"));
        assert!(json.contains("\"dry_run\":false"));
    }
}
