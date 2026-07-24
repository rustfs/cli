//! Secret-safe, versioned IAM archive export and import.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use rc_core::admin::{
    IamArchiveApi, IamArchiveImportResult, IamArchiveInventory, MAX_IAM_ARCHIVE_BYTES,
};
use rc_core::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroizing;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;

const MANIFEST_PATH: &str = "iam-assets/manifest.json";
const ARCHIVE_FORMAT: &str = "rustfs-iam-archive";
const ARCHIVE_VERSION: u8 = 1;
const MAX_ARCHIVE_ENTRIES: usize = 8;
const IAM_FILES: &[&str] = &[
    "iam-assets/policies.json",
    "iam-assets/users.json",
    "iam-assets/groups.json",
    "iam-assets/svcaccts.json",
    "iam-assets/user_mappings.json",
    "iam-assets/group_mappings.json",
    "iam-assets/stsuser_mappings.json",
];

#[derive(Subcommand, Debug)]
pub enum IamCommands {
    /// Export IAM users, groups, policies, mappings, and service accounts
    Export(ExportArgs),
    /// Validate or import a previously exported IAM archive
    Import(ImportArgs),
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Alias name of the server
    pub alias: String,
    /// New private archive file; existing files are never overwritten
    #[arg(long)]
    pub file: PathBuf,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Alias name of the server
    pub alias: String,
    /// Private archive file, or '-' to read from stdin
    #[arg(long)]
    pub file: PathBuf,
    /// Validate and report the import plan without mutation
    #[arg(long)]
    pub dry_run: bool,
    /// Required before sending an import mutation
    #[arg(long)]
    pub yes: bool,
    /// How to handle destination entities with the same name
    #[arg(long, value_enum, default_value_t = ConflictPolicy::Fail)]
    pub conflict: ConflictPolicy,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictPolicy {
    #[default]
    Fail,
    Overwrite,
}

#[derive(Debug, Serialize)]
struct IamOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct IamErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: IamErrorBody,
}

#[derive(Debug, Serialize)]
struct IamErrorBody {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: Value,
    server: Value,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ExportOutput {
    operation: &'static str,
    archive_version: u8,
    entries: usize,
    bytes: usize,
    private: bool,
}

#[derive(Debug, Serialize)]
struct ImportOutput {
    operation: &'static str,
    archive_version: u8,
    dry_run: bool,
    conflict_policy: ConflictPolicy,
    inventory: InventoryCounts,
    conflicts: InventoryCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ImportResultSummary>,
}

#[derive(Debug, Default, Serialize)]
struct InventoryCounts {
    users: usize,
    groups: usize,
    policies: usize,
    service_accounts: usize,
}

impl From<&IamArchiveInventory> for InventoryCounts {
    fn from(value: &IamArchiveInventory) -> Self {
        Self {
            users: value.users.len(),
            groups: value.groups.len(),
            policies: value.policies.len(),
            service_accounts: value.service_accounts.len(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ImportResultSummary {
    added: usize,
    skipped: usize,
    removed: usize,
    failed: usize,
    partial: bool,
}

#[derive(Debug, Serialize)]
struct ArchiveManifest<'a> {
    format: &'a str,
    version: u8,
    entries: Vec<&'a str>,
}

struct ValidatedArchive {
    bytes: Zeroizing<Vec<u8>>,
    inventory: IamArchiveInventory,
    version: u8,
}

pub async fn execute(command: IamCommands, formatter: &Formatter) -> ExitCode {
    match command {
        IamCommands::Export(args) => execute_export(args, formatter).await,
        IamCommands::Import(args) => execute_import(args, formatter).await,
    }
}

async fn execute_export(args: ExportArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match execute_export_with_api(args, &client, formatter).await {
        Ok(()) => ExitCode::Success,
        Err(error) => emit_error(&error, formatter),
    }
}

async fn execute_export_with_api(
    args: ExportArgs,
    api: &dyn IamArchiveApi,
    formatter: &Formatter,
) -> Result<()> {
    if args.file.as_os_str() == "-" {
        return Err(Error::InvalidPath(
            "IAM exports require --file with a filesystem path; stdout is not allowed".to_string(),
        ));
    }
    if args.file.exists() {
        return Err(Error::Conflict(
            "IAM archive output already exists".to_string(),
        ));
    }
    let raw = export_with_retry(api).await?;
    let archive = canonicalize_archive(&raw)?;
    let bytes = archive.bytes.len();
    write_private_atomic(&args.file, &archive.bytes)?;
    let output = IamOutput {
        schema_version: 3,
        output_type: "admin_iam_archive",
        status: "success",
        data: ExportOutput {
            operation: "iam.export",
            archive_version: archive.version,
            entries: inventory_entry_count(&archive.inventory),
            bytes,
            private: true,
        },
    };
    if formatter.is_json() {
        formatter.json(&output);
    } else {
        formatter.println(&format!(
            "IAM archive exported securely (version {}, {} entities, {} bytes)",
            archive.version,
            inventory_entry_count(&archive.inventory),
            bytes
        ));
    }
    Ok(())
}

async fn export_with_retry(api: &dyn IamArchiveApi) -> Result<Vec<u8>> {
    let mut attempt = 0_u8;
    loop {
        attempt += 1;
        let result = tokio::select! {
            result = api.export_iam_archive() => result,
            _ = tokio::signal::ctrl_c() => {
                return Err(Error::Interrupted("IAM export cancelled".to_string()));
            }
        };
        match result {
            Ok(archive) => return Ok(archive),
            Err(Error::Network(_)) if attempt < 3 => {
                tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn execute_import(args: ImportArgs, formatter: &Formatter) -> ExitCode {
    if !args.dry_run && !args.yes {
        return emit_error(
            &Error::InvalidPath("IAM import requires --yes unless --dry-run is used".to_string()),
            formatter,
        );
    }
    let archive =
        match read_archive_input(&args.file).and_then(|bytes| canonicalize_archive(&bytes)) {
            Ok(archive) => archive,
            Err(error) => return emit_error(&error, formatter),
        };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match execute_import_with_api(args, archive, &client, formatter).await {
        Ok(code) => code,
        Err(error) => emit_error(&error, formatter),
    }
}

async fn execute_import_with_api(
    args: ImportArgs,
    archive: ValidatedArchive,
    api: &dyn IamArchiveApi,
    formatter: &Formatter,
) -> Result<ExitCode> {
    let conflicts = tokio::select! {
        result = api.iam_archive_conflicts(&archive.inventory) => result?,
        _ = tokio::signal::ctrl_c() => {
            return Err(Error::Interrupted("IAM import preflight cancelled".to_string()));
        }
    };
    let conflict_count = inventory_entry_count(&conflicts);
    if conflict_count > 0 && args.conflict == ConflictPolicy::Fail {
        return Err(Error::Conflict(format!(
            "IAM import found {conflict_count} existing destination entities; use --conflict overwrite after review"
        )));
    }

    let result = if args.dry_run {
        None
    } else {
        Some(tokio::select! {
            result = api.import_iam_archive(archive.bytes.to_vec()) => result?,
            _ = tokio::signal::ctrl_c() => {
                return Err(Error::Interrupted(
                    "IAM import was interrupted; its outcome may be unknown, inspect destination IAM state before retrying".to_string()
                ));
            }
        })
    };
    let summary = result.as_ref().map(import_result_summary);
    let partial = summary.as_ref().is_some_and(|summary| summary.partial);
    let output = IamOutput {
        schema_version: 3,
        output_type: "admin_iam_archive",
        status: if partial { "partial" } else { "success" },
        data: ImportOutput {
            operation: "iam.import",
            archive_version: archive.version,
            dry_run: args.dry_run,
            conflict_policy: args.conflict,
            inventory: (&archive.inventory).into(),
            conflicts: (&conflicts).into(),
            result: summary,
        },
    };
    if formatter.is_json() {
        formatter.json(&output);
    } else if args.dry_run {
        formatter.println(&format!(
            "IAM archive validated; dry-run made no changes ({} entities, {} conflicts)",
            inventory_entry_count(&archive.inventory),
            conflict_count
        ));
    } else if let Some(summary) = output.data.result {
        formatter.println(&format!(
            "IAM import completed: {} added, {} skipped, {} removed, {} failed",
            summary.added, summary.skipped, summary.removed, summary.failed
        ));
    }
    Ok(if partial {
        ExitCode::GeneralError
    } else {
        ExitCode::Success
    })
}

fn import_result_summary(result: &IamArchiveImportResult) -> ImportResultSummary {
    let added = result_entity_count(&result.added);
    let skipped = result_entity_count(&result.skipped);
    let removed = result_entity_count(&result.removed);
    let failed = result.failed.len();
    ImportResultSummary {
        added,
        skipped,
        removed,
        failed,
        partial: failed > 0,
    }
}

fn result_entity_count(value: &rc_core::admin::IamArchiveResultEntities) -> usize {
    value.policies.len()
        + value.users.len()
        + value.groups.len()
        + value.service_accounts.len()
        + value.user_policies.len()
        + value.group_policies.len()
        + value.sts_policies.len()
}

fn inventory_entry_count(value: &IamArchiveInventory) -> usize {
    value.users.len() + value.groups.len() + value.policies.len() + value.service_accounts.len()
}

fn read_archive_input(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    if path.as_os_str() == "-" {
        return read_bounded(std::io::stdin().lock());
    }
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Error::InvalidPath("Failed to inspect IAM archive input".to_string()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(Error::InvalidPath(
            "IAM archive input must be a regular file, not a symlink".to_string(),
        ));
    }
    let file = File::open(path)
        .map_err(|_| Error::InvalidPath("Failed to open IAM archive input".to_string()))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| Error::InvalidPath("Failed to inspect opened IAM archive".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(Error::InvalidPath(
                "IAM archive input changed while being opened".to_string(),
            ));
        }
        if file_metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(
                "IAM archive input cannot grant group or other permissions".to_string(),
            ));
        }
    }
    read_bounded(file)
}

fn read_bounded(mut reader: impl Read) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .by_ref()
        .take((MAX_IAM_ARCHIVE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidPath("Failed to read IAM archive input".to_string()))?;
    if bytes.len() > MAX_IAM_ARCHIVE_BYTES {
        return Err(Error::InvalidPath(format!(
            "IAM archive exceeds the {MAX_IAM_ARCHIVE_BYTES} byte limit"
        )));
    }
    Ok(bytes)
}

fn canonicalize_archive(raw: &[u8]) -> Result<ValidatedArchive> {
    if raw.len() > MAX_IAM_ARCHIVE_BYTES {
        return Err(Error::InvalidPath(format!(
            "IAM archive exceeds the {MAX_IAM_ARCHIVE_BYTES} byte limit"
        )));
    }
    let cursor = Cursor::new(raw);
    let mut zip = ZipArchive::new(cursor)
        .map_err(|_| Error::InvalidPath("IAM archive is not a valid ZIP document".to_string()))?;
    if zip.is_empty() || zip.len() > MAX_ARCHIVE_ENTRIES {
        return Err(Error::InvalidPath(format!(
            "IAM archive must contain between 1 and {MAX_ARCHIVE_ENTRIES} entries"
        )));
    }
    let allowed = IAM_FILES
        .iter()
        .copied()
        .chain(std::iter::once(MANIFEST_PATH))
        .collect::<BTreeSet<_>>();
    let mut documents = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut manifest_version = None;
    let mut manifest_entries = None;
    let mut expanded = 0_usize;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|_| {
            Error::InvalidPath("IAM archive contains an unreadable entry".to_string())
        })?;
        let name = entry.name().to_string();
        if !allowed.contains(name.as_str()) || entry.is_dir() || !seen.insert(name.clone()) {
            return Err(Error::InvalidPath(
                "IAM archive contains an unexpected, duplicate, or directory entry".to_string(),
            ));
        }
        if entry.size() > MAX_IAM_ARCHIVE_BYTES as u64
            || expanded.saturating_add(entry.size() as usize) > MAX_IAM_ARCHIVE_BYTES
        {
            return Err(Error::InvalidPath(
                "IAM archive expanded content exceeds the safety limit".to_string(),
            ));
        }
        let remaining = MAX_IAM_ARCHIVE_BYTES - expanded;
        let mut bytes = Zeroizing::new(Vec::with_capacity(entry.size() as usize));
        entry
            .by_ref()
            .take((remaining + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                Error::InvalidPath("IAM archive contains an unreadable entry".to_string())
            })?;
        if bytes.len() > remaining {
            return Err(Error::InvalidPath(
                "IAM archive expanded content exceeds the safety limit".to_string(),
            ));
        }
        expanded += bytes.len();
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::InvalidPath("IAM archive contains malformed JSON".to_string()))?;
        if name == MANIFEST_PATH {
            let format = value.get("format").and_then(Value::as_str);
            let version = value.get("version").and_then(Value::as_u64);
            if format != Some(ARCHIVE_FORMAT) || version != Some(u64::from(ARCHIVE_VERSION)) {
                return Err(Error::UnsupportedFeature(
                    "IAM archive format or version is unsupported".to_string(),
                ));
            }
            manifest_version = Some(ARCHIVE_VERSION);
            let entries = value
                .get("entries")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Error::InvalidPath("IAM archive manifest has no entry list".to_string())
                })?
                .iter()
                .map(|entry| {
                    entry.as_str().map(str::to_string).ok_or_else(|| {
                        Error::InvalidPath(
                            "IAM archive manifest contains an invalid entry name".to_string(),
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            manifest_entries = Some(entries);
        } else {
            if !value.is_object() {
                return Err(Error::InvalidPath(
                    "IAM archive entity documents must be JSON objects".to_string(),
                ));
            }
            documents.insert(name, canonical_json(value));
        }
    }
    if documents.is_empty() {
        return Err(Error::InvalidPath(
            "IAM archive contains no supported entity documents".to_string(),
        ));
    }
    if let Some(mut entries) = manifest_entries {
        let entry_count = entries.len();
        entries.sort();
        entries.dedup();
        let actual = documents.keys().cloned().collect::<Vec<_>>();
        if entries.len() != entry_count || entries != actual {
            return Err(Error::InvalidPath(
                "IAM archive manifest does not match its entity documents".to_string(),
            ));
        }
    }
    let inventory = inventory_from_documents(&documents)?;
    let bytes = write_canonical_zip(&documents)?;
    Ok(ValidatedArchive {
        bytes: Zeroizing::new(bytes),
        inventory,
        version: manifest_version.unwrap_or(ARCHIVE_VERSION),
    })
}

fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        value => value,
    }
}

fn inventory_from_documents(documents: &BTreeMap<String, Value>) -> Result<IamArchiveInventory> {
    fn keys(documents: &BTreeMap<String, Value>, path: &str) -> Result<Vec<String>> {
        let Some(value) = documents.get(path) else {
            return Ok(Vec::new());
        };
        let object = value.as_object().ok_or_else(|| {
            Error::InvalidPath("IAM archive entity documents must be JSON objects".to_string())
        })?;
        let mut names = object.keys().cloned().collect::<Vec<_>>();
        if names.iter().any(|name| name.trim().is_empty()) {
            return Err(Error::InvalidPath(
                "IAM archive contains an empty entity name".to_string(),
            ));
        }
        names.sort();
        Ok(names)
    }
    Ok(IamArchiveInventory {
        users: keys(documents, "iam-assets/users.json")?,
        groups: keys(documents, "iam-assets/groups.json")?,
        policies: keys(documents, "iam-assets/policies.json")?,
        service_accounts: keys(documents, "iam-assets/svcaccts.json")?,
    })
}

fn write_canonical_zip(documents: &BTreeMap<String, Value>) -> Result<Vec<u8>> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let entries = documents.keys().map(String::as_str).collect::<Vec<_>>();
    let manifest = serde_json::to_vec(&ArchiveManifest {
        format: ARCHIVE_FORMAT,
        version: ARCHIVE_VERSION,
        entries,
    })
    .map_err(|_| Error::General("Failed to encode IAM archive manifest".to_string()))?;
    writer
        .start_file(MANIFEST_PATH, options)
        .map_err(|_| Error::General("Failed to create IAM archive".to_string()))?;
    writer
        .write_all(&manifest)
        .map_err(|_| Error::General("Failed to create IAM archive".to_string()))?;
    for (name, value) in documents {
        let bytes = Zeroizing::new(
            serde_json::to_vec(value)
                .map_err(|_| Error::General("Failed to encode IAM archive".to_string()))?,
        );
        writer
            .start_file(name, options)
            .map_err(|_| Error::General("Failed to create IAM archive".to_string()))?;
        writer
            .write_all(&bytes)
            .map_err(|_| Error::General("Failed to create IAM archive".to_string()))?;
    }
    writer
        .finish()
        .map(Cursor::into_inner)
        .map_err(|_| Error::General("Failed to finish IAM archive".to_string()))
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Err(Error::Conflict(
            "IAM archive output already exists".to_string(),
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".rc-iam-")
        .tempfile_in(parent)
        .map_err(|_| {
            Error::InvalidPath("Failed to create private IAM archive output".to_string())
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::InvalidPath("Failed to protect IAM archive output".to_string()))?;
    }
    temporary
        .write_all(bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| Error::InvalidPath("Failed to write IAM archive output".to_string()))?;
    temporary.persist_noclobber(path).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Conflict("IAM archive output already exists".to_string())
        } else {
            Error::InvalidPath("Failed to atomically publish IAM archive output".to_string())
        }
    })?;
    Ok(())
}

fn emit_error(error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    if formatter.is_json() {
        let unsupported = matches!(error, Error::UnsupportedFeature(_));
        formatter.json_error(&IamErrorOutput {
            schema_version: 3,
            output_type: "admin_iam_archive",
            status: "error",
            error: IamErrorBody {
                error_type: iam_error_type(error),
                message: error.to_string(),
                retryable: matches!(error, Error::Network(_) | Error::Interrupted(_)),
                capability: if unsupported {
                    Value::String("admin.iam.archive".to_string())
                } else {
                    Value::Null
                },
                server: Value::Null,
                suggestion: iam_error_suggestion(error),
            },
        });
    } else {
        formatter.error_with_code(code, &error.to_string());
    }
    code
}

fn iam_error_type(error: &Error) -> &'static str {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => "usage_error",
        Error::Network(_) => "network_error",
        Error::Auth(_) => "auth_error",
        Error::NotFound(_) | Error::AliasNotFound(_) => "not_found",
        Error::Conflict(_) | Error::AliasExists(_) => "conflict",
        Error::UnsupportedFeature(_) => "unsupported_feature",
        Error::Interrupted(_) => "interrupted",
        _ => "general_error",
    }
}

fn iam_error_suggestion(error: &Error) -> Option<&'static str> {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => {
            Some("Review archive input, confirmation, and conflict options.")
        }
        Error::Network(_) => {
            Some("Verify connectivity; never retry an unknown import outcome blindly.")
        }
        Error::Auth(_) => Some("Verify credentials and IAM export/import permissions."),
        Error::Conflict(_) => Some("Review destination conflicts before selecting overwrite."),
        Error::UnsupportedFeature(_) => Some("Verify the RustFS IAM archive routes are available."),
        Error::Interrupted(_) => Some("Inspect destination IAM state before retrying an import."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn legacy_archive(secret: &str) -> Vec<u8> {
        let mut documents = BTreeMap::new();
        documents.insert(
            "iam-assets/policies.json".to_string(),
            serde_json::json!({"readonly": {"Version": "2012-10-17", "Statement": []}}),
        );
        documents.insert(
            "iam-assets/users.json".to_string(),
            serde_json::json!({"alice": {"secretKey": secret, "status": "enabled"}}),
        );
        documents.insert(
            "iam-assets/groups.json".to_string(),
            serde_json::json!({"ops": {"members": ["alice"], "status": "enabled"}}),
        );
        documents.insert(
            "iam-assets/svcaccts.json".to_string(),
            serde_json::json!({"svc-1": {
                "parent": "alice",
                "accessKey": "svc-1",
                "secretKey": secret,
                "groups": ["ops"],
                "claims": {},
                "sessionPolicy": {},
                "status": "on",
                "name": "backup",
                "description": "archive",
                "expiration": null
            }}),
        );
        documents.insert(
            "iam-assets/user_mappings.json".to_string(),
            serde_json::json!({"alice": {"policies": "readonly"}}),
        );
        documents.insert(
            "iam-assets/group_mappings.json".to_string(),
            serde_json::json!({"ops": {"policies": "readonly"}}),
        );
        documents.insert(
            "iam-assets/stsuser_mappings.json".to_string(),
            serde_json::json!({}),
        );
        write_canonical_zip(&documents).expect("archive")
    }

    struct MockApi {
        exports: AtomicUsize,
        import_calls: AtomicUsize,
        archive: Vec<u8>,
        conflicts: IamArchiveInventory,
        result: IamArchiveImportResult,
        fail_exports: usize,
    }

    #[async_trait]
    impl IamArchiveApi for MockApi {
        async fn export_iam_archive(&self) -> Result<Vec<u8>> {
            let call = self.exports.fetch_add(1, Ordering::SeqCst);
            if call < self.fail_exports {
                Err(Error::Network("temporary".to_string()))
            } else {
                Ok(self.archive.clone())
            }
        }

        async fn import_iam_archive(&self, _archive: Vec<u8>) -> Result<IamArchiveImportResult> {
            self.import_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.result.clone())
        }

        async fn iam_archive_conflicts(
            &self,
            _inventory: &IamArchiveInventory,
        ) -> Result<IamArchiveInventory> {
            Ok(self.conflicts.clone())
        }
    }

    #[test]
    fn canonical_archive_is_deterministic_and_versioned() {
        let first = canonicalize_archive(&legacy_archive("top-secret")).expect("first");
        let second = canonicalize_archive(&legacy_archive("top-secret")).expect("second");
        assert_eq!(first.bytes.as_slice(), second.bytes.as_slice());
        assert_eq!(first.version, 1);
        assert_eq!(first.inventory.users, ["alice"]);
        assert_eq!(first.inventory.groups, ["ops"]);
        assert_eq!(first.inventory.policies, ["readonly"]);
        assert_eq!(first.inventory.service_accounts, ["svc-1"]);
    }

    #[test]
    fn malformed_and_oversized_archives_are_rejected_without_echoing_contents() {
        let error = match canonicalize_archive(b"secret-key-not-a-zip") {
            Ok(_) => panic!("must reject"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("secret-key"));
        let oversized = vec![0_u8; MAX_IAM_ARCHIVE_BYTES + 1];
        assert!(canonicalize_archive(&oversized).is_err());
    }

    #[test]
    fn compressed_zip_bomb_is_rejected_by_expanded_limit() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        writer
            .start_file(
                "iam-assets/users.json",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .expect("entry");
        writer.write_all(b"{\"alice\":\"").expect("prefix");
        writer
            .write_all(&vec![b'x'; MAX_IAM_ARCHIVE_BYTES])
            .expect("compressed content");
        writer.write_all(b"\"}").expect("suffix");
        let archive = writer.finish().expect("zip").into_inner();
        assert!(archive.len() < MAX_IAM_ARCHIVE_BYTES);
        assert!(canonicalize_archive(&archive).is_err());
    }

    #[tokio::test]
    async fn export_retries_read_only_failures() {
        let api = MockApi {
            exports: AtomicUsize::new(0),
            import_calls: AtomicUsize::new(0),
            archive: legacy_archive("secret"),
            conflicts: IamArchiveInventory::default(),
            result: IamArchiveImportResult::default(),
            fail_exports: 2,
        };
        assert!(export_with_retry(&api).await.is_ok());
        assert_eq!(api.exports.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn dry_run_and_conflict_failure_do_not_mutate() {
        let api = MockApi {
            exports: AtomicUsize::new(0),
            import_calls: AtomicUsize::new(0),
            archive: Vec::new(),
            conflicts: IamArchiveInventory {
                users: vec!["alice".to_string()],
                ..Default::default()
            },
            result: IamArchiveImportResult::default(),
            fail_exports: 0,
        };
        let archive = canonicalize_archive(&legacy_archive("secret")).expect("archive");
        let formatter = Formatter::new(crate::output::OutputConfig::default());
        let result = execute_import_with_api(
            ImportArgs {
                alias: "local".to_string(),
                file: "-".into(),
                dry_run: true,
                yes: false,
                conflict: ConflictPolicy::Fail,
            },
            archive,
            &api,
            &formatter,
        )
        .await;
        assert!(matches!(result, Err(Error::Conflict(_))));
        assert_eq!(api.import_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn overwrite_dry_run_reports_plan_without_mutation() {
        let api = MockApi {
            exports: AtomicUsize::new(0),
            import_calls: AtomicUsize::new(0),
            archive: Vec::new(),
            conflicts: IamArchiveInventory {
                users: vec!["alice".to_string()],
                ..Default::default()
            },
            result: IamArchiveImportResult::default(),
            fail_exports: 0,
        };
        let archive = canonicalize_archive(&legacy_archive("secret")).expect("archive");
        let formatter = Formatter::new(crate::output::OutputConfig::default());
        let code = execute_import_with_api(
            ImportArgs {
                alias: "local".to_string(),
                file: "-".into(),
                dry_run: true,
                yes: false,
                conflict: ConflictPolicy::Overwrite,
            },
            archive,
            &api,
            &formatter,
        )
        .await
        .expect("dry run");
        assert_eq!(code, ExitCode::Success);
        assert_eq!(api.import_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn private_output_is_atomic_and_never_overwrites() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("iam.zip");
        write_private_atomic(&path, b"archive").expect("write");
        assert!(matches!(
            write_private_atomic(&path, b"replace"),
            Err(Error::Conflict(_))
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn import_rejects_group_readable_archive_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("iam.zip");
        std::fs::write(&path, legacy_archive("secret")).expect("archive");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("permissions");
        let error = match read_archive_input(&path) {
            Ok(_) => panic!("unsafe permissions must be rejected"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn partial_result_counts_failures_without_server_error_text() {
        let result = IamArchiveImportResult {
            failed: vec![rc_core::admin::IamArchiveImportSection {
                name: "alice".to_string(),
                policies: Vec::new(),
            }],
            ..Default::default()
        };
        let summary = import_result_summary(&result);
        assert!(summary.partial);
        assert_eq!(summary.failed, 1);
    }
}
