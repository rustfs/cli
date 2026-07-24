//! Validated, deterministic RustFS bucket-metadata archive commands.

use clap::{Args, Subcommand, ValueEnum};
use rc_core::Error;
use rc_core::admin::{
    BUCKET_METADATA_CAPABILITY, BucketMetadataApi, BucketMetadataArchive,
    MAX_BUCKET_METADATA_ARCHIVE_BYTES,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::future::Future;
use std::io::{Cursor, Read, Write, stdin};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EXPANDED_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const TARGETS_CONFIG: &str = "bucket-targets.json";
const CONFIG_FILES: [&str; 10] = [
    "policy.json",
    "notification.xml",
    "lifecycle.xml",
    "bucket-encryption.xml",
    "tagging.xml",
    "quota.json",
    "object-lock.xml",
    "versioning.xml",
    "replication.xml",
    TARGETS_CONFIG,
];

#[derive(Subcommand, Debug)]
pub enum BucketMetadataCommands {
    /// Export a deterministic, redacted metadata archive
    Export(ExportArgs),
    /// Validate and import metadata using an explicit conflict strategy
    Import(ImportArgs),
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    pub alias: String,
    /// Select one or more buckets; omit to export every bucket
    #[arg(long = "bucket")]
    pub buckets: Vec<String>,
    /// Destination ZIP path
    #[arg(long)]
    pub file: PathBuf,
    /// Atomically replace an existing destination
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ConflictStrategy {
    /// Stop before mutation when an imported config differs from current state
    Fail,
    /// Import differing configs
    Overwrite,
    /// Omit differing configs and import only non-conflicting entries
    Skip,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    pub alias: String,
    /// Protected ZIP path, or '-' to read from standard input
    #[arg(long)]
    pub file: String,
    /// Select one or more buckets from the archive
    #[arg(long = "bucket")]
    pub buckets: Vec<String>,
    /// Required behavior when an existing config differs
    #[arg(long, value_enum)]
    pub conflict: ConflictStrategy,
    /// Validate and report the import plan without mutation
    #[arg(long)]
    pub dry_run: bool,
    /// Confirm a mutating import
    #[arg(long)]
    pub yes: bool,
}

struct ArchiveEntry {
    bytes: Zeroizing<Vec<u8>>,
}

#[derive(Default)]
struct ValidatedArchive {
    entries: BTreeMap<(String, String), ArchiveEntry>,
}

#[derive(Debug, Serialize)]
struct OutputEnvelope {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: OutputData,
}

#[derive(Debug, Serialize)]
struct OutputData {
    operations: Vec<OutputOperation>,
}

#[derive(Debug, Serialize)]
struct OutputOperation {
    operation: &'static str,
    resource: String,
    state: &'static str,
    operation_id: Option<String>,
    changed: bool,
    result: OperationResult,
}

#[derive(Debug, Serialize)]
struct OperationResult {
    bucket: String,
    configs: usize,
    planned_changes: usize,
    conflicts: usize,
    skipped: usize,
    unchanged: usize,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    suggestion: &'static str,
}

pub async fn execute(command: BucketMetadataCommands, formatter: &Formatter) -> ExitCode {
    let alias = match &command {
        BucketMetadataCommands::Export(args) => &args.alias,
        BucketMetadataCommands::Import(args) => &args.alias,
    };
    let client = match get_admin_client(alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    let result = match command {
        BucketMetadataCommands::Export(args) => export(args, &client, formatter).await,
        BucketMetadataCommands::Import(args) => import(args, &client, formatter).await,
    };
    match result {
        Ok(()) => ExitCode::Success,
        Err(error) => emit_error(&error, formatter),
    }
}

async fn export(
    args: ExportArgs,
    api: &dyn BucketMetadataApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    let selected = validate_bucket_selection(&args.buckets)?;
    let archive = fetch_server_archive(api, &selected).await?;
    let buckets = if selected.is_empty() {
        archive.bucket_names()
    } else {
        selected.iter().cloned().collect()
    };
    let bucket_count = buckets.len();
    let bytes = archive.encode()?;
    write_atomic_private_file(&args.file, &bytes, args.force)?;

    let operations = buckets
        .into_iter()
        .map(|bucket| OutputOperation {
            operation: "bucket-metadata.export",
            resource: format!("bucket/{bucket}"),
            state: "succeeded",
            operation_id: None,
            changed: true,
            result: OperationResult {
                configs: archive.config_count(&bucket),
                planned_changes: archive.config_count(&bucket),
                bucket,
                conflicts: 0,
                skipped: 0,
                unchanged: 0,
                dry_run: false,
                path: Some(args.file.display().to_string()),
            },
        })
        .collect();
    emit_success(operations, formatter, || {
        formatter.success(&format!(
            "Exported validated metadata for {} bucket(s) to {}",
            bucket_count,
            args.file.display()
        ));
    });
    Ok(())
}

async fn import(
    args: ImportArgs,
    api: &dyn BucketMetadataApi,
    formatter: &Formatter,
) -> rc_core::Result<()> {
    if !args.dry_run && !args.yes {
        return Err(Error::Config(
            "Bucket metadata import requires --yes unless --dry-run is used".to_string(),
        ));
    }
    let selected = validate_bucket_selection(&args.buckets)?;
    let input = read_protected_archive(&args.file)?;
    let mut requested = ValidatedArchive::read(&input)?;
    requested.select(&selected)?;
    requested.reject_redacted_target_credentials()?;

    let current = fetch_server_archive(api, &selected).await?;
    let plan = ImportPlan::build(requested, &current, args.conflict)?;
    let operations = plan.operations(args.dry_run);
    if !args.dry_run && !plan.archive.entries.is_empty() {
        import_with_cancellation(
            api,
            BucketMetadataArchive::new(plan.archive.encode()?)?,
            tokio::signal::ctrl_c(),
        )
        .await?;
    }
    emit_success(operations, formatter, || {
        plan.print(args.dry_run, formatter)
    });
    Ok(())
}

async fn import_with_cancellation<F>(
    api: &dyn BucketMetadataApi,
    archive: BucketMetadataArchive,
    cancellation: F,
) -> rc_core::Result<()>
where
    F: Future<Output = std::io::Result<()>>,
{
    tokio::pin!(cancellation);
    tokio::select! {
        result = api.import_bucket_metadata(archive) => result,
        signal = &mut cancellation => match signal {
            Ok(()) => Err(Error::Interrupted(
                "Bucket metadata import was interrupted; the outcome may be partial, so inspect every selected bucket before retrying"
                    .to_string(),
            )),
            Err(_) => Err(Error::General(
                "Failed to register bucket metadata import interruption handling".to_string(),
            )),
        },
    }
}

async fn fetch_server_archive(
    api: &dyn BucketMetadataApi,
    selected: &BTreeSet<String>,
) -> rc_core::Result<ValidatedArchive> {
    if selected.is_empty() {
        let archive = api.export_bucket_metadata(None).await?;
        return ValidatedArchive::read_server(archive.as_bytes());
    }
    let mut combined = ValidatedArchive::default();
    for bucket in selected {
        let archive = api.export_bucket_metadata(Some(bucket)).await?;
        let archive = ValidatedArchive::read_server(archive.as_bytes())?;
        if archive
            .entries
            .keys()
            .any(|(candidate, _)| candidate != bucket)
        {
            return Err(Error::General(
                "RustFS returned metadata outside the selected bucket".to_string(),
            ));
        }
        for (key, entry) in archive.entries {
            if combined.entries.insert(key, entry).is_some() {
                return Err(Error::General(
                    "RustFS returned duplicate bucket metadata".to_string(),
                ));
            }
        }
    }
    Ok(combined)
}

fn validate_bucket_selection(values: &[String]) -> rc_core::Result<BTreeSet<String>> {
    let mut selected = BTreeSet::new();
    for value in values {
        if value.is_empty()
            || value.len() > 63
            || value.contains(['/', '\\', '\0'])
            || value == "."
            || value == ".."
        {
            return Err(Error::InvalidPath(
                "Bucket selectors must be non-empty bucket names without path separators"
                    .to_string(),
            ));
        }
        selected.insert(value.clone());
    }
    Ok(selected)
}

impl ValidatedArchive {
    fn read(bytes: &[u8]) -> rc_core::Result<Self> {
        Self::read_internal(bytes, false)
    }

    fn read_server(bytes: &[u8]) -> rc_core::Result<Self> {
        Self::read_internal(bytes, true)
    }

    fn read_internal(bytes: &[u8], allow_empty: bool) -> rc_core::Result<Self> {
        if bytes.len() > MAX_BUCKET_METADATA_ARCHIVE_BYTES {
            return Err(Error::RequestRejected(
                "Bucket metadata archive exceeds the 100 MiB limit".to_string(),
            ));
        }
        let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(|_| {
            Error::Config("Bucket metadata input is not a valid ZIP archive".to_string())
        })?;
        if zip.len() > MAX_ARCHIVE_ENTRIES {
            return Err(Error::Config(format!(
                "Bucket metadata archive exceeds the {MAX_ARCHIVE_ENTRIES} entry limit"
            )));
        }
        let mut entries = BTreeMap::new();
        let mut expanded = 0_u64;
        for index in 0..zip.len() {
            let mut file = zip.by_index(index).map_err(|_| {
                Error::Config("Bucket metadata archive contains an unreadable entry".to_string())
            })?;
            if !file.is_file() || file.enclosed_name().is_none() {
                return Err(Error::Config(
                    "Bucket metadata archive contains a non-file or unsafe entry".to_string(),
                ));
            }
            if file.size() > MAX_ENTRY_BYTES {
                return Err(Error::Config(format!(
                    "Bucket metadata archive entry exceeds the {MAX_ENTRY_BYTES} byte limit"
                )));
            }
            expanded = expanded.saturating_add(file.size());
            if expanded > MAX_EXPANDED_ARCHIVE_BYTES {
                return Err(Error::Config(
                    "Bucket metadata archive expands beyond the 128 MiB limit".to_string(),
                ));
            }
            let name = file.name().to_string();
            let mut parts = name.split('/');
            let bucket = parts.next().unwrap_or_default();
            let config = parts.next().unwrap_or_default();
            if bucket.is_empty()
                || config.is_empty()
                || parts.next().is_some()
                || !CONFIG_FILES.contains(&config)
            {
                return Err(Error::Config(
                    "Bucket metadata archive contains an unsupported entry path".to_string(),
                ));
            }
            validate_bucket_selection(&[bucket.to_string()])?;
            let mut content = Zeroizing::new(Vec::new());
            file.by_ref()
                .take(MAX_ENTRY_BYTES + 1)
                .read_to_end(&mut content)
                .map_err(|_| {
                    Error::Config(
                        "Bucket metadata archive contains an unreadable entry".to_string(),
                    )
                })?;
            if content.len() as u64 > MAX_ENTRY_BYTES {
                return Err(Error::Config(
                    "Bucket metadata archive entry exceeded its declared bound".to_string(),
                ));
            }
            if content.is_empty() {
                return Err(Error::Config(
                    "Bucket metadata archive contains an empty config entry".to_string(),
                ));
            }
            if entries
                .insert(
                    (bucket.to_string(), config.to_string()),
                    ArchiveEntry { bytes: content },
                )
                .is_some()
            {
                return Err(Error::Config(
                    "Bucket metadata archive contains a duplicate config entry".to_string(),
                ));
            }
        }
        if entries.is_empty() && !allow_empty {
            return Err(Error::Config(
                "Bucket metadata archive contains no supported configs".to_string(),
            ));
        }
        Ok(Self { entries })
    }

    fn encode(&self) -> rc_core::Result<Vec<u8>> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(0o600);
        for ((bucket, config), entry) in &self.entries {
            writer
                .start_file(format!("{bucket}/{config}"), options)
                .map_err(|_| {
                    Error::General("Failed to create bucket metadata archive".to_string())
                })?;
            writer.write_all(entry.bytes.as_slice())?;
        }
        let bytes = writer
            .finish()
            .map_err(|_| Error::General("Failed to finalize bucket metadata archive".to_string()))?
            .into_inner();
        if bytes.len() > MAX_BUCKET_METADATA_ARCHIVE_BYTES {
            return Err(Error::RequestRejected(
                "Bucket metadata archive exceeds the 100 MiB limit".to_string(),
            ));
        }
        Ok(bytes)
    }

    fn select(&mut self, selected: &BTreeSet<String>) -> rc_core::Result<()> {
        if selected.is_empty() {
            return Ok(());
        }
        let available = self.bucket_names().into_iter().collect::<BTreeSet<_>>();
        let missing = selected.difference(&available).cloned().collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(Error::NotFound(format!(
                "Selected bucket is absent from the metadata archive: {}",
                missing.join(", ")
            )));
        }
        self.entries
            .retain(|(bucket, _), _| selected.contains(bucket));
        Ok(())
    }

    fn bucket_names(&self) -> Vec<String> {
        self.entries
            .keys()
            .map(|(bucket, _)| bucket.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn config_count(&self, bucket: &str) -> usize {
        self.entries
            .keys()
            .filter(|(candidate, _)| candidate == bucket)
            .count()
    }

    fn reject_redacted_target_credentials(&self) -> rc_core::Result<()> {
        for ((_, config), entry) in &self.entries {
            if config != TARGETS_CONFIG {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(entry.bytes.as_slice()).map_err(|_| {
                    Error::Config("Bucket target metadata is malformed JSON".to_string())
                })?;
            if contains_unusable_target_secret(&value) {
                return Err(Error::Config(
                    "Bucket target metadata contains missing or redacted credentials and cannot be imported; provide a protected archive containing the real target credentials"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }
}

fn contains_unusable_target_secret(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter().any(contains_unusable_target_secret),
        serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
            let normalized = key
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>();
            if normalized == "secretkey" {
                return value.as_str().is_none_or(|secret| {
                    matches!(
                        secret.trim().to_ascii_lowercase().as_str(),
                        "" | "redacted" | "*redacted*" | "<redacted>"
                    )
                });
            }
            contains_unusable_target_secret(value)
        }),
        _ => false,
    }
}

struct ImportPlan {
    archive: ValidatedArchive,
    buckets: BTreeMap<String, BucketPlan>,
}

#[derive(Default)]
struct BucketPlan {
    configs: usize,
    conflicts: usize,
    skipped: usize,
    unchanged: usize,
}

impl ImportPlan {
    fn build(
        mut requested: ValidatedArchive,
        current: &ValidatedArchive,
        strategy: ConflictStrategy,
    ) -> rc_core::Result<Self> {
        let source_buckets = requested.bucket_names();
        let mut buckets = source_buckets
            .iter()
            .map(|bucket| (bucket.clone(), BucketPlan::default()))
            .collect::<BTreeMap<_, _>>();
        let mut remove = Vec::new();
        for (key, requested_entry) in &requested.entries {
            let bucket = buckets
                .get_mut(&key.0)
                .expect("bucket plan was created from requested entries");
            bucket.configs += 1;
            if let Some(current_entry) = current.entries.get(key) {
                if current_entry.bytes.as_slice() == requested_entry.bytes.as_slice() {
                    bucket.unchanged += 1;
                    remove.push(key.clone());
                } else {
                    bucket.conflicts += 1;
                    match strategy {
                        ConflictStrategy::Fail => {}
                        ConflictStrategy::Overwrite => {}
                        ConflictStrategy::Skip => {
                            bucket.skipped += 1;
                            remove.push(key.clone());
                        }
                    }
                }
            }
        }
        let conflicts = buckets.values().map(|plan| plan.conflicts).sum::<usize>();
        if conflicts > 0 && strategy == ConflictStrategy::Fail {
            return Err(Error::Conflict(format!(
                "Bucket metadata import found {conflicts} conflicting config(s); use --conflict overwrite or --conflict skip"
            )));
        }
        for key in remove {
            requested.entries.remove(&key);
        }
        Ok(Self {
            archive: requested,
            buckets,
        })
    }

    fn operations(&self, dry_run: bool) -> Vec<OutputOperation> {
        self.buckets
            .iter()
            .map(|(bucket, plan)| {
                let applied = self.archive.config_count(bucket);
                OutputOperation {
                    operation: "bucket-metadata.import",
                    resource: format!("bucket/{bucket}"),
                    state: "succeeded",
                    operation_id: None,
                    changed: !dry_run && applied > 0,
                    result: OperationResult {
                        bucket: bucket.clone(),
                        configs: plan.configs,
                        planned_changes: applied,
                        conflicts: plan.conflicts,
                        skipped: plan.skipped,
                        unchanged: plan.unchanged,
                        dry_run,
                        path: None,
                    },
                }
            })
            .collect()
    }

    fn print(&self, dry_run: bool, formatter: &Formatter) {
        for (bucket, plan) in &self.buckets {
            formatter.println(&format!(
                "{bucket}: {} config(s), {} planned change(s), {} conflict(s), {} skipped, {} unchanged{}",
                plan.configs,
                self.archive.config_count(bucket),
                plan.conflicts,
                plan.skipped,
                plan.unchanged,
                if dry_run { " (dry-run)" } else { "" }
            ));
        }
    }
}

fn read_protected_archive(path: &str) -> rc_core::Result<Zeroizing<Vec<u8>>> {
    if path == "-" {
        return read_bounded(stdin().lock());
    }
    let path = Path::new(path);
    let before = std::fs::symlink_metadata(path)
        .map_err(|_| Error::InvalidPath("Failed to inspect bucket metadata input".to_string()))?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(Error::InvalidPath(
            "Bucket metadata input must be a regular file, not a symlink".to_string(),
        ));
    }
    let file = File::open(path)
        .map_err(|_| Error::InvalidPath("Failed to open bucket metadata input".to_string()))?;
    let opened = file.metadata().map_err(|_| {
        Error::InvalidPath("Failed to inspect opened bucket metadata input".to_string())
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(Error::InvalidPath(
                "Bucket metadata input changed while being opened".to_string(),
            ));
        }
        if opened.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(
                "Bucket metadata input cannot grant group or other permissions".to_string(),
            ));
        }
    }
    read_bounded(file)
}

fn read_bounded(reader: impl Read) -> rc_core::Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(MAX_BUCKET_METADATA_ARCHIVE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidPath("Failed to read bucket metadata input".to_string()))?;
    if bytes.len() > MAX_BUCKET_METADATA_ARCHIVE_BYTES {
        return Err(Error::RequestRejected(
            "Bucket metadata archive exceeds the 100 MiB limit".to_string(),
        ));
    }
    Ok(bytes)
}

fn write_atomic_private_file(path: &Path, bytes: &[u8], force: bool) -> rc_core::Result<()> {
    let directory = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    if force {
        temporary.persist(path).map_err(|error| error.error)?;
    } else {
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
    }
    Ok(())
}

fn emit_success(operations: Vec<OutputOperation>, formatter: &Formatter, human: impl FnOnce()) {
    if formatter.is_json() {
        formatter.json(&OutputEnvelope {
            schema_version: 3,
            output_type: "admin_operations",
            status: "success",
            data: OutputData { operations },
        });
    } else {
        human();
    }
}

fn emit_error(error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let uncertain_import = matches!(
        error,
        Error::Network(message)
            if message.contains("outcome is unknown")
                || message.contains("partially applied")
                || message.contains("inspect every selected bucket")
    );
    if formatter.is_json() {
        formatter.json_error(&ErrorEnvelope {
            schema_version: 3,
            output_type: "admin_operations",
            status: "error",
            error: ErrorDetail {
                error_type: match code {
                    ExitCode::UsageError => "usage_error",
                    ExitCode::NetworkError => "network_error",
                    ExitCode::AuthError => "auth_error",
                    ExitCode::NotFound => "not_found",
                    ExitCode::Conflict => "conflict",
                    ExitCode::UnsupportedFeature => "unsupported_feature",
                    ExitCode::Interrupted => "interrupted",
                    _ => "general_error",
                },
                message: error.to_string(),
                retryable: matches!(code, ExitCode::NetworkError) && !uncertain_import,
                capability: matches!(code, ExitCode::UnsupportedFeature)
                    .then_some(BUCKET_METADATA_CAPABILITY),
                server: matches!(code, ExitCode::UnsupportedFeature).then_some("rustfs"),
                outcome: uncertain_import.then_some("unknown_partial"),
                suggestion: match code {
                    ExitCode::Conflict => {
                        "Choose an explicit overwrite or skip conflict strategy when appropriate."
                    }
                    ExitCode::NetworkError => {
                        "For import failures, inspect all selected buckets before retrying."
                    }
                    ExitCode::AuthError => {
                        "Verify ExportBucketMetadata or ImportBucketMetadata permission."
                    }
                    ExitCode::UnsupportedFeature => {
                        "Verify the RustFS server exposes the v3 bucket metadata routes."
                    }
                    _ => "Validate the protected archive, bucket selection, and command arguments.",
                },
            },
        });
    } else {
        formatter.error_with_code(code, &error.to_string());
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::future::pending;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .expect("start entry");
            writer.write_all(bytes).expect("write entry");
        }
        writer.finish().expect("finish").into_inner()
    }

    #[test]
    fn deterministic_archive_sorts_entries_and_round_trips_multiple_buckets() {
        let input = archive(&[
            ("zeta/quota.json", br#"{"quota":2}"#),
            ("alpha/policy.json", br#"{"Version":"2012-10-17"}"#),
        ]);
        let parsed = ValidatedArchive::read(&input).expect("parse");
        let first = parsed.encode().expect("encode");
        let second = parsed.encode().expect("encode");
        assert_eq!(first, second);
        let reparsed = ValidatedArchive::read(&first).expect("reparse");
        assert_eq!(reparsed.bucket_names(), ["alpha", "zeta"]);
    }

    #[test]
    fn selection_does_not_touch_unselected_buckets() {
        let input = archive(&[("alpha/policy.json", b"{}"), ("beta/policy.json", b"{}")]);
        let mut parsed = ValidatedArchive::read(&input).expect("parse");
        parsed
            .select(&BTreeSet::from(["beta".to_string()]))
            .expect("select");
        assert_eq!(parsed.bucket_names(), ["beta"]);
    }

    #[test]
    fn malformed_paths_and_unknown_configs_fail_closed() {
        assert!(ValidatedArchive::read(&archive(&[("../policy.json", b"{}")])).is_err());
        assert!(ValidatedArchive::read(&archive(&[("alpha/credentials.json", b"{}")])).is_err());
    }

    #[test]
    fn redacted_replication_target_credentials_are_never_imported() {
        let input = archive(&[(
            "alpha/bucket-targets.json",
            br#"{"targets":[{"accessKey":"visible","secretKey":"","description":"server-secret"}]}"#,
        )]);
        let parsed = ValidatedArchive::read(&input).expect("parse");
        let error = parsed
            .reject_redacted_target_credentials()
            .expect_err("redacted input must fail");
        let message = error.to_string();
        assert!(!message.contains("server-secret"));
    }

    #[test]
    fn conflict_strategies_are_explicit_and_skip_is_per_entry() {
        let requested = ValidatedArchive::read(&archive(&[
            ("alpha/policy.json", b"new"),
            ("alpha/quota.json", b"same"),
        ]))
        .expect("requested");
        let current = ValidatedArchive::read(&archive(&[
            ("alpha/policy.json", b"old"),
            ("alpha/quota.json", b"same"),
        ]))
        .expect("current");
        assert!(ImportPlan::build(requested, &current, ConflictStrategy::Fail).is_err());

        let requested = ValidatedArchive::read(&archive(&[
            ("alpha/policy.json", b"new"),
            ("alpha/quota.json", b"same"),
        ]))
        .expect("requested");
        let plan =
            ImportPlan::build(requested, &current, ConflictStrategy::Skip).expect("skip plan");
        assert!(plan.archive.entries.is_empty());
        assert_eq!(plan.buckets["alpha"].conflicts, 1);
        assert_eq!(plan.buckets["alpha"].skipped, 1);
    }

    #[test]
    fn export_is_private_atomic_and_refuses_implicit_overwrite() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("archive.zip");
        write_atomic_private_file(&path, b"first", false).expect("first");
        assert!(write_atomic_private_file(&path, b"second", false).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"first");
        write_atomic_private_file(&path, b"second", true).expect("replace");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }

    struct PendingImportApi;

    #[async_trait]
    impl BucketMetadataApi for PendingImportApi {
        async fn export_bucket_metadata(
            &self,
            _bucket: Option<&str>,
        ) -> rc_core::Result<BucketMetadataArchive> {
            panic!("export is not used")
        }

        async fn import_bucket_metadata(
            &self,
            _archive: BucketMetadataArchive,
        ) -> rc_core::Result<()> {
            pending().await
        }
    }

    #[tokio::test]
    async fn interruption_reports_unknown_partial_outcome() {
        let archive =
            BucketMetadataArchive::new(archive(&[("alpha/policy.json", b"new")])).expect("archive");
        let error =
            import_with_cancellation(&PendingImportApi, archive, std::future::ready(Ok(())))
                .await
                .expect_err("interrupted");
        assert!(matches!(error, Error::Interrupted(_)));
        assert!(error.to_string().contains("partial"));
    }
}
