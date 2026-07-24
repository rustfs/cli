//! replicate command - Manage bucket replication configuration
//!
//! Add, update, list, status, remove, export, or import bucket replication rules.
//! This command orchestrates both the S3 replication API and the Admin remote-target API.

use clap::{Args, Subcommand};
use comfy_table::{Cell, Table};
use rc_core::admin::{
    AdminApi, ReplicationDiff, ReplicationDiffApi, ReplicationDiffEntry, ReplicationInspectionApi,
    ReplicationMetricScope, ReplicationMetrics, ReplicationMrf,
};
use rc_core::replication::{
    BucketTarget, BucketTargetCredentials, ReplicationConfiguration, ReplicationDestination,
    ReplicationResyncStartOptions, ReplicationResyncStartResult, ReplicationResyncStatus,
    ReplicationResyncTargetStatus, ReplicationRule, ReplicationRuleStatus,
};
use rc_core::{AliasManager, Error, ObjectStore as _};
use rc_s3::{AdminClient, S3Client};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const DEFAULT_REMOTE_TARGET_PATH: &str = "auto";
const DEFAULT_REMOTE_TARGET_API: &str = "s3v4";
const DEFAULT_REPLICATION_STORAGE_CLASS: &str = "STANDARD";
const REPLICATE_AFTER_HELP: &str = "\
Examples:
  rc bucket replication list local/my-bucket
  rc bucket replication add local/my-bucket --remote-bucket backup/archive
  rc replicate status local/my-bucket";
const REPLICATE_ADD_AFTER_HELP: &str = "\
Examples:
  rc bucket replication add local/my-bucket --remote-bucket backup/archive
  rc replicate add local/my-bucket --remote-bucket backup/archive --prefix reports/
  rc bucket replication add local/my-bucket --remote-bucket backup/archive --replicate delete,existing-objects --sync
  rc bucket replication add local/my-bucket --remote-bucket backup/archive --insecure
  rc bucket replication add local/my-bucket --remote-bucket backup/archive --ca-cert ./private-ca.pem";

const CA_CERT_LOCAL_PATH_SUGGESTION: &str =
    "--ca-cert is a local CLI path; the certificate content will be uploaded to RustFS";

/// Manage bucket replication
#[derive(Args, Debug)]
#[command(after_help = REPLICATE_AFTER_HELP)]
pub struct ReplicateArgs {
    #[command(subcommand)]
    pub command: ReplicateCommands,
}

#[derive(Subcommand, Debug)]
pub enum ReplicateCommands {
    /// Add a new replication rule
    Add(AddArgs),

    /// Update an existing replication rule
    Update(UpdateArgs),

    /// List replication rules for a bucket
    List(BucketArg),

    /// Show replication status/metrics for a bucket
    Status(BucketArg),

    /// Show aggregate replication MRF backlog for a bucket
    Mrf(BucketArg),

    /// Scan for object versions that have not replicated
    Diff(DiffArgs),

    /// Remove replication rules from a bucket
    Remove(RemoveArgs),

    /// Export replication configuration as JSON
    Export(BucketArg),

    /// Import replication configuration from a JSON file
    Import(ImportArgs),

    /// Actively validate configured replication targets
    Check(CheckArgs),

    /// Manage bucket replication resync operations
    #[command(subcommand)]
    Resync(ResyncCommands),
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Confirm the active remote write/delete validation probe
    #[arg(long)]
    pub yes: bool,

    /// Force operation even if generic replication capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Subcommand, Debug)]
pub enum ResyncCommands {
    /// Start resyncing previously replicated objects
    Start(ResyncStartArgs),

    /// Show persisted server-side resync status
    Status(ResyncStatusArgs),
}

#[derive(Args, Debug)]
pub struct ResyncStartArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Select a configured replication target ARN
    #[arg(long)]
    pub target_arn: Option<String>,

    /// Only resync objects older than this duration (for example, 7d10h31s)
    #[arg(long)]
    pub older_than: Option<String>,

    /// Supply a stable resync identifier instead of using a server-generated ID
    #[arg(long)]
    pub reset_id: Option<String>,

    /// Confirm the active server-side resync
    #[arg(long)]
    pub yes: bool,

    /// Force operation even if generic replication capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ResyncStatusArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Filter status to one configured replication target ARN
    #[arg(long)]
    pub target_arn: Option<String>,

    /// Force operation even if generic replication capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct BucketArg {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Limit the scan to object keys below this prefix
    #[arg(long)]
    pub prefix: Option<String>,
}

#[derive(Args, Debug)]
#[command(after_help = REPLICATE_ADD_AFTER_HELP)]
pub struct AddArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Remote target bucket (TARGET_ALIAS/BUCKET)
    #[arg(long, value_name = "TARGET_ALIAS/BUCKET")]
    pub remote_bucket: String,

    /// Replication flags (comma-separated: delete,delete-marker,existing-objects)
    #[arg(long, value_name = "FLAGS")]
    pub replicate: Option<String>,

    /// Rule priority (higher = more important)
    #[arg(long, default_value = "1")]
    pub priority: i32,

    /// Storage class override at destination
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Bandwidth limit in bytes/sec (0 = unlimited)
    #[arg(long, default_value = "0")]
    pub bandwidth: i64,

    /// Enable synchronous replication
    #[arg(long)]
    pub sync: bool,

    /// Key prefix filter
    #[arg(long)]
    pub prefix: Option<String>,

    /// Rule identifier (auto-generated if not specified)
    #[arg(long)]
    pub id: Option<String>,

    /// Health check interval in seconds
    #[arg(long, value_name = "SECONDS", default_value = "60")]
    pub healthcheck_seconds: u64,

    /// Disable replication proxy
    #[arg(long)]
    pub disable_proxy: bool,

    /// Skip TLS certificate verification for this bucket replication target.
    /// Intended for development or test environments.
    #[arg(long)]
    pub insecure: bool,

    /// Read a local PEM CA certificate file and upload its content for this
    /// bucket replication target. The path is resolved on the CLI machine,
    /// not on the RustFS server.
    #[arg(long, value_name = "FILE")]
    pub ca_cert: Option<PathBuf>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Rule ID to update
    #[arg(long)]
    pub id: String,

    /// Replication flags (comma-separated: delete,delete-marker,existing-objects)
    #[arg(long, value_name = "FLAGS")]
    pub replicate: Option<String>,

    /// Rule priority (higher = more important)
    #[arg(long)]
    pub priority: Option<i32>,

    /// Storage class override at destination
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Bandwidth limit in bytes/sec (0 = unlimited)
    #[arg(long)]
    pub bandwidth: Option<i64>,

    /// Enable or disable synchronous replication
    #[arg(long)]
    pub sync: Option<bool>,

    /// Key prefix filter
    #[arg(long)]
    pub prefix: Option<String>,

    /// Health check interval in seconds
    #[arg(long, value_name = "SECONDS")]
    pub healthcheck_seconds: Option<u64>,

    /// Disable replication proxy
    #[arg(long)]
    pub disable_proxy: Option<bool>,

    /// Skip TLS certificate verification for this bucket replication target.
    /// Intended for development or test environments.
    #[arg(long)]
    pub insecure: bool,

    /// Read a local PEM CA certificate file and upload its content for this
    /// bucket replication target. The path is resolved on the CLI machine,
    /// not on the RustFS server.
    #[arg(long, value_name = "FILE")]
    pub ca_cert: Option<PathBuf>,

    /// Enable or disable the rule
    #[arg(long)]
    pub status: Option<ReplicationRuleStatus>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RemoveArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Rule ID to remove (omit for --all)
    #[arg(long)]
    pub id: Option<String>,

    /// Remove all replication rules
    #[arg(long)]
    pub all: bool,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Path to JSON file containing replication configuration
    pub file: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

// ==================== Output types ====================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplicateListOutput {
    bucket: String,
    rules: Vec<ReplicationRule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplicateOperationOutput {
    bucket: String,
    rule_id: String,
    action: String,
}

#[derive(Debug, Serialize)]
struct ReplicationV3Output {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: ReplicationV3Data,
}

#[derive(Debug, Serialize)]
struct ReplicationV3Data {
    operation: &'static str,
    bucket: String,
    valid: Option<bool>,
    targets: Vec<ReplicationV3Target>,
}

#[derive(Debug, Serialize)]
struct ReplicationV3Target {
    target_arn: String,
    reset_id: String,
    reset_before: Option<jiff::Timestamp>,
    started_at: Option<jiff::Timestamp>,
    last_updated_at: Option<jiff::Timestamp>,
    state: Option<rc_core::ReplicationResyncState>,
    server_state: Option<String>,
    replicated_count: Option<u64>,
    replicated_size: Option<u64>,
    failed_count: Option<u64>,
    failed_size: Option<u64>,
    current_bucket: Option<String>,
    current_object: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplicationExport {
    #[serde(flatten)]
    config: ReplicationConfiguration,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remote_targets: Vec<BucketTarget>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReplicationTargetTlsSettings {
    skip_tls_verify: Option<bool>,
    ca_cert_pem: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReplicationDiffSuccessOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: ReplicationDiffData,
}

#[derive(Debug, Serialize)]
struct ReplicationDiffData {
    operation: &'static str,
    bucket: String,
    prefix: Option<String>,
    entries: Vec<ReplicationDiffEntryOutput>,
    scan: ReplicationDiffScanOutput,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReplicationDiffEntryOutput {
    object: String,
    version_id: Option<String>,
    delete_marker: bool,
    size_bytes: u64,
    replication_status: String,
    last_modified: Option<jiff::Timestamp>,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReplicationDiffScanOutput {
    scanned_versions: usize,
    truncated: bool,
    resumable: bool,
}

#[derive(Debug, Serialize)]
struct ReplicationInspectionOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct ReplicationClusterOutput {
    state: &'static str,
    observed_nodes: Option<u32>,
    expected_nodes: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ReplicationStatusData {
    operation: &'static str,
    bucket: String,
    availability: &'static str,
    cluster: ReplicationClusterOutput,
    queue: ReplicationQueueOutput,
    totals: ReplicationTotalsOutput,
    targets: Vec<ReplicationStatusTargetOutput>,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReplicationQueueOutput {
    count: u64,
    size_bytes: u64,
    scope: String,
}

#[derive(Debug, Serialize)]
struct ReplicationTotalsOutput {
    replica_count: u64,
    replica_size_bytes: u64,
    replicated_count: u64,
    replicated_size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ReplicationStatusTargetOutput {
    target_arn: String,
    replicated_count: u64,
    replicated_size_bytes: u64,
    failed_count: u64,
    failed_size_bytes: u64,
    latency: ReplicationLatencyOutput,
    bandwidth: ReplicationBandwidthOutput,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReplicationLatencyOutput {
    average_ms: f64,
    current_ms: f64,
    maximum_ms: f64,
    scope: String,
}

#[derive(Debug, Serialize)]
struct ReplicationBandwidthOutput {
    limit_bytes_per_sec: u64,
    current_bytes_per_sec: f64,
    scope: String,
}

#[derive(Debug, Serialize)]
struct ReplicationMrfData {
    operation: &'static str,
    bucket: String,
    runtime_stats_available: bool,
    cluster: ReplicationClusterOutput,
    failed: ReplicationBacklogOutput,
    queue: ReplicationBacklogOutput,
    durable_backlog: ReplicationDurableBacklogOutput,
    per_object_entries_available: bool,
    per_target_durable_entries_available: bool,
    targets: Vec<ReplicationMrfTargetOutput>,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReplicationBacklogOutput {
    count: u64,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ReplicationDurableBacklogOutput {
    available: bool,
    count: u64,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ReplicationMrfTargetOutput {
    target_arn: String,
    failed_count: u64,
    failed_size_bytes: u64,
    observation_scope: String,
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReplicationDiffErrorOutput {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: ReplicationDiffError,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ReplicationDiffError {
    Unsupported(ReplicationDiffUnsupportedError),
    Standard(ReplicationDiffStandardError),
}

#[derive(Debug, Serialize)]
struct ReplicationDiffUnsupportedError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: &'static str,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ReplicationDiffStandardError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<&'static str>,
}

// ==================== execute ====================

/// Execute the replicate command
pub async fn execute(args: ReplicateArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        ReplicateCommands::Add(args) => execute_add(args, output_config).await,
        ReplicateCommands::Update(args) => execute_update(args, output_config).await,
        ReplicateCommands::List(args) => execute_list(args, output_config).await,
        ReplicateCommands::Status(args) => execute_status(args, output_config).await,
        ReplicateCommands::Mrf(args) => execute_mrf(args, output_config).await,
        ReplicateCommands::Diff(args) => execute_diff(args, output_config).await,
        ReplicateCommands::Remove(args) => execute_remove(args, output_config).await,
        ReplicateCommands::Export(args) => execute_export(args, output_config).await,
        ReplicateCommands::Import(args) => execute_import(args, output_config).await,
        ReplicateCommands::Check(args) => execute_check(args, output_config).await,
        ReplicateCommands::Resync(ResyncCommands::Start(args)) => {
            execute_resync_start(args, output_config).await
        }
        ReplicateCommands::Resync(ResyncCommands::Status(args)) => {
            execute_resync_status(args, output_config).await
        }
    }
}

// ==================== Diff ====================

async fn execute_diff(args: DiffArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket.",
            );
        }
    };
    let client = match setup_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    execute_diff_with_api(&bucket, args.prefix, &client, &formatter).await
}

async fn execute_diff_with_api(
    bucket: &str,
    prefix: Option<String>,
    api: &dyn ReplicationDiffApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.replication_diff(bucket, prefix.as_deref()).await {
        Ok(diff) => {
            if formatter.is_json() {
                formatter.json(&replication_diff_output(bucket, prefix, diff));
            } else {
                for line in replication_diff_lines(bucket, prefix.as_deref(), &diff, formatter) {
                    formatter.println(&line);
                }
            }
            ExitCode::Success
        }
        Err(error) => emit_replication_diff_error(&error, formatter),
    }
}

fn replication_diff_output(
    bucket: &str,
    prefix: Option<String>,
    mut diff: ReplicationDiff,
) -> ReplicationDiffSuccessOutput {
    sort_replication_diff_entries(&mut diff.entries);
    ReplicationDiffSuccessOutput {
        schema_version: 3,
        output_type: "replication",
        status: "success",
        data: ReplicationDiffData {
            operation: "diff",
            bucket: bucket.to_string(),
            prefix,
            entries: diff
                .entries
                .into_iter()
                .map(|entry| ReplicationDiffEntryOutput {
                    object: entry.object,
                    version_id: entry.version_id,
                    delete_marker: entry.delete_marker,
                    size_bytes: entry.size_bytes,
                    replication_status: entry.replication_status,
                    last_modified: entry.last_modified,
                    extensions: entry.extra,
                })
                .collect(),
            scan: ReplicationDiffScanOutput {
                scanned_versions: diff.scanned_versions,
                truncated: diff.is_truncated,
                resumable: false,
            },
            extensions: diff.extra,
        },
    }
}

fn replication_diff_lines(
    bucket: &str,
    prefix: Option<&str>,
    diff: &ReplicationDiff,
    formatter: &Formatter,
) -> Vec<String> {
    let bucket = formatter.sanitize_text(bucket);
    let scope = match prefix {
        Some(prefix) => format!("{bucket} (prefix: {})", formatter.sanitize_text(prefix)),
        None => bucket,
    };
    let mut entries = diff.entries.clone();
    sort_replication_diff_entries(&mut entries);
    let mut lines = vec![format!("Replication diff for '{scope}':")];

    if diff.is_truncated {
        lines.push(format!(
            "Partial scan: inspected {} versions; results are non-resumable. Narrow --prefix and run again for a more complete view.",
            diff.scanned_versions
        ));
    } else {
        lines.push(format!(
            "Complete scan: inspected {} versions.",
            diff.scanned_versions
        ));
    }

    if entries.is_empty() {
        lines.push(if diff.is_truncated {
            "No pending or failed versions were found in this partial scan; this does not prove the bucket has no replication backlog.".to_string()
        } else {
            "No pending or failed versions found.".to_string()
        });
        return lines;
    }

    lines.push(String::new());
    lines.push("OBJECT  VERSION  TYPE  SIZE  STATUS  LAST MODIFIED".to_string());
    for entry in entries {
        lines.push(format!(
            "{}  {}  {}  {}  {}  {}",
            formatter.sanitize_text(&entry.object),
            formatter.sanitize_text(entry.version_id.as_deref().unwrap_or("-")),
            if entry.delete_marker {
                "delete-marker"
            } else {
                "object"
            },
            entry.size_bytes,
            formatter.sanitize_text(&entry.replication_status),
            formatter.sanitize_text(
                entry
                    .last_modified
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref()
                    .unwrap_or("-")
            )
        ));
    }
    lines
}

fn sort_replication_diff_entries(entries: &mut [ReplicationDiffEntry]) {
    entries.sort_by(|left, right| {
        (
            &left.object,
            &left.version_id,
            left.delete_marker,
            &left.replication_status,
            &left.last_modified,
        )
            .cmp(&(
                &right.object,
                &right.version_id,
                right.delete_marker,
                &right.replication_status,
                &right.last_modified,
            ))
    });
}

fn emit_replication_diff_error(error: &Error, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("Failed to scan replication diff: {error}");
    if formatter.is_json() {
        formatter.json_error(&replication_diff_error_output(error, code, message));
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn replication_diff_error_output(
    error: &Error,
    code: ExitCode,
    message: String,
) -> ReplicationDiffErrorOutput {
    let error = if matches!(error, Error::UnsupportedFeature(_)) {
        ReplicationDiffError::Unsupported(ReplicationDiffUnsupportedError {
            error_type: "unsupported_feature",
            message,
            retryable: false,
            capability: "replication_diff",
            server: None,
            suggestion: Some(
                "Upgrade RustFS or verify that the replication diff route is enabled.",
            ),
        })
    } else {
        let (error_type, retryable, suggestion) = replication_diff_error_metadata(code);
        ReplicationDiffError::Standard(ReplicationDiffStandardError {
            error_type,
            message,
            retryable,
            suggestion,
        })
    };
    ReplicationDiffErrorOutput {
        schema_version: 3,
        output_type: "replication",
        status: "error",
        error,
    }
}

const fn replication_diff_error_metadata(
    code: ExitCode,
) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Verify the alias configuration and command arguments."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify the endpoint and network connectivity, then retry."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify the alias credentials and replication admin permission."),
        ),
        ExitCode::NotFound => (
            "not_found",
            false,
            Some("Verify the bucket name and that replication is configured."),
        ),
        ExitCode::Conflict => (
            "conflict",
            false,
            Some("Review the replication configuration and retry."),
        ),
        ExitCode::Interrupted => (
            "interrupted",
            true,
            Some("Run the diff again if the scan is still needed."),
        ),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
        }
    }
}

// ==================== Add ====================

async fn execute_add(args: AddArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let tls_settings =
        match build_replication_target_tls_settings(args.insecure, args.ca_cert.as_deref()) {
            Ok(settings) => settings,
            Err(error) => {
                let suggestion = if args.ca_cert.is_some() {
                    CA_CERT_LOCAL_PATH_SUGGESTION
                } else {
                    "Retry the command with either --insecure or --ca-cert, but not both."
                };
                return formatter.fail_with_suggestion(ExitCode::UsageError, &error, suggestion);
            }
        };

    let (source_alias, source_bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the replication command.",
            );
        }
    };

    let (target_alias, target_bucket) = match parse_bucket_path(&args.remote_bucket) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &format!("Invalid --remote-bucket: {error}"),
                "Use a bucket path in the form alias/bucket for --remote-bucket.",
            );
        }
    };

    // Create S3 client for source
    let s3_client =
        match setup_s3_client(&source_alias, &source_bucket, args.force, &formatter).await {
            Ok(client) => client,
            Err(code) => return code,
        };

    // Create admin client for source (to register remote target)
    let admin_client = match setup_admin_client(&source_alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    // Resolve target alias to get endpoint + credentials
    let target_alias_info = match resolve_alias(&target_alias, &formatter) {
        Ok(alias) => alias,
        Err(code) => return code,
    };

    let (target_endpoint, secure) =
        remote_target_endpoint(&target_alias_info.endpoint, target_alias_info.insecure);

    let storage_class = args
        .storage_class
        .clone()
        .unwrap_or_else(|| DEFAULT_REPLICATION_STORAGE_CLASS.to_string());

    // Build BucketTarget
    let mut target = BucketTarget {
        source_bucket: source_bucket.clone(),
        endpoint: target_endpoint,
        credentials: Some(BucketTargetCredentials {
            access_key: target_alias_info.access_key.clone(),
            secret_key: target_alias_info.secret_key.clone(),
        }),
        target_bucket: target_bucket.clone(),
        secure,
        path: DEFAULT_REMOTE_TARGET_PATH.to_string(),
        api: DEFAULT_REMOTE_TARGET_API.to_string(),
        target_type: "replication".to_string(),
        region: target_alias_info.region.clone(),
        bandwidth_limit: args.bandwidth,
        replication_sync: args.sync,
        storage_class: storage_class.clone(),
        health_check_duration: args.healthcheck_seconds,
        disable_proxy: args.disable_proxy,
        ..Default::default()
    };
    apply_replication_target_tls_settings(&mut target, &tls_settings);

    // Register remote target via admin API → get ARN
    let arn = match admin_client
        .set_remote_target(&source_bucket, target, false)
        .await
    {
        Ok(arn) => arn,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to set remote target: {error}"),
            );
        }
    };

    // Parse replication flags
    let (delete_replication, delete_marker_replication, existing_object_replication) =
        parse_replicate_flags(args.replicate.as_deref());

    let rule_id = args
        .id
        .unwrap_or_else(|| format!("rule-{}", &arn[arn.len().saturating_sub(8)..]));

    let destination_storage_class = Some(storage_class);

    let new_rule = ReplicationRule {
        id: rule_id.clone(),
        priority: args.priority,
        status: ReplicationRuleStatus::Enabled,
        prefix: args.prefix,
        tags: None,
        destination: ReplicationDestination {
            bucket_arn: arn,
            storage_class: destination_storage_class,
        },
        delete_marker_replication: Some(delete_marker_replication),
        existing_object_replication: Some(existing_object_replication),
        delete_replication: Some(delete_replication),
    };

    // Get existing config or create new
    let mut config = match s3_client.get_bucket_replication(&source_bucket).await {
        Ok(Some(config)) => config,
        Ok(None) => ReplicationConfiguration {
            role: default_replication_role(&new_rule.destination.bucket_arn),
            rules: Vec::new(),
        },
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to get replication config: {error}"),
            );
        }
    };

    if config.role.is_empty() {
        config.role = default_replication_role(&new_rule.destination.bucket_arn);
    }

    config.rules.push(new_rule);

    match s3_client
        .set_bucket_replication(&source_bucket, config)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&ReplicateOperationOutput {
                    bucket: source_bucket,
                    rule_id,
                    action: "added".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Replication rule '{}' added for '{}/{}'",
                    rule_id, source_alias, source_bucket
                ));
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to set replication config: {error}"),
        ),
    }
}

// ==================== Update ====================

async fn execute_update(args: UpdateArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let tls_settings =
        match build_replication_target_tls_settings(args.insecure, args.ca_cert.as_deref()) {
            Ok(settings) => settings,
            Err(error) => {
                let suggestion = if args.ca_cert.is_some() {
                    CA_CERT_LOCAL_PATH_SUGGESTION
                } else {
                    "Retry the command with either --insecure or --ca-cert, but not both."
                };
                return formatter.fail_with_suggestion(ExitCode::UsageError, &error, suggestion);
            }
        };

    let (source_alias, source_bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the replication command.",
            );
        }
    };

    let s3_client =
        match setup_s3_client(&source_alias, &source_bucket, args.force, &formatter).await {
            Ok(client) => client,
            Err(code) => return code,
        };

    let mut config = match s3_client.get_bucket_replication(&source_bucket).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                "No replication configuration found on this bucket",
                "Run `rc bucket replication add ...` to create the first replication rule for this bucket.",
            );
        }
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to get replication config: {error}"),
            );
        }
    };

    let rule_index = match config.rules.iter().position(|rule| rule.id == args.id) {
        Some(index) => index,
        None => {
            formatter.error(&format!("Rule '{}' not found", args.id));
            return ExitCode::NotFound;
        }
    };

    let current_target_arn = config.rules[rule_index].destination.bucket_arn.clone();

    if target_level_updates_requested(&args) {
        let admin_client = match setup_admin_client(&source_alias, &formatter) {
            Ok(client) => client,
            Err(code) => return code,
        };

        let mut target = match admin_client.list_remote_targets(&source_bucket).await {
            Ok(targets) => match targets
                .into_iter()
                .find(|target| target.arn == current_target_arn)
            {
                Some(target) => target,
                None => {
                    formatter.error(&format!(
                        "Remote target '{}' not found for rule '{}'",
                        current_target_arn, args.id
                    ));
                    return ExitCode::NotFound;
                }
            },
            Err(error) => {
                formatter.error(&format!("Failed to list remote targets: {error}"));
                return ExitCode::GeneralError;
            }
        };

        apply_target_updates(&mut target, &args, &tls_settings);

        let updated_arn = match admin_client
            .set_remote_target(&source_bucket, target, true)
            .await
        {
            Ok(arn) => arn,
            Err(error) => {
                formatter.error(&format!("Failed to update remote target: {error}"));
                return ExitCode::GeneralError;
            }
        };

        if updated_arn != current_target_arn {
            let mut arn_map = HashMap::new();
            arn_map.insert(current_target_arn.clone(), updated_arn);
            remap_replication_arns(&mut config, &arn_map);
        }
    }

    let rule = &mut config.rules[rule_index];

    // Apply updates
    if let Some(priority) = args.priority {
        rule.priority = priority;
    }
    if let Some(status) = args.status {
        rule.status = status;
    }
    if let Some(ref prefix) = args.prefix {
        rule.prefix = Some(prefix.clone());
    }
    if let Some(ref storage_class) = args.storage_class {
        rule.destination.storage_class = Some(storage_class.clone());
    }
    if let Some(ref flags) = args.replicate {
        let (delete, delete_marker, existing) = parse_replicate_flags(Some(flags));
        rule.delete_replication = Some(delete);
        rule.delete_marker_replication = Some(delete_marker);
        rule.existing_object_replication = Some(existing);
    }

    let rule_id = args.id.clone();

    match s3_client
        .set_bucket_replication(&source_bucket, config)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&ReplicateOperationOutput {
                    bucket: source_bucket,
                    rule_id,
                    action: "updated".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Replication rule '{}' updated for '{}/{}'",
                    rule_id, source_alias, source_bucket
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to update replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== List ====================

async fn execute_list(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_replication(&bucket).await {
        Ok(Some(config)) => {
            if formatter.is_json() {
                formatter.json(&ReplicateListOutput {
                    bucket,
                    rules: config.rules,
                });
            } else if config.rules.is_empty() {
                formatter.println("No replication rules found.");
            } else {
                let mut table = Table::new();
                table.set_header(vec![
                    Cell::new("ID"),
                    Cell::new("Priority"),
                    Cell::new("Status"),
                    Cell::new("Prefix"),
                    Cell::new("Flags"),
                    Cell::new("Destination"),
                    Cell::new("Storage Class"),
                ]);

                for rule in &config.rules {
                    table.add_row(vec![
                        Cell::new(&rule.id),
                        Cell::new(rule.priority),
                        Cell::new(rule.status),
                        Cell::new(rule.prefix.as_deref().unwrap_or("-")),
                        Cell::new(format_replication_flags(rule)),
                        Cell::new(&rule.destination.bucket_arn),
                        Cell::new(rule.destination.storage_class.as_deref().unwrap_or("-")),
                    ]);
                }

                formatter.println(&table.to_string());
            }
            ExitCode::Success
        }
        Ok(None) => {
            if formatter.is_json() {
                formatter.json(&ReplicateListOutput {
                    bucket,
                    rules: Vec::new(),
                });
            } else {
                formatter.println("No replication configuration found.");
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Status ====================

async fn execute_status(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let admin_client = match setup_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    execute_status_with_api(&bucket, &admin_client, &formatter).await
}

async fn execute_status_with_api(
    bucket: &str,
    api: &dyn ReplicationInspectionApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.replication_metrics(bucket).await {
        Ok(metrics) => {
            if formatter.is_json() {
                formatter.json(&replication_status_output(bucket, metrics));
            } else {
                for line in replication_status_lines(bucket, &metrics, formatter) {
                    formatter.println(&line);
                }
            }
            ExitCode::Success
        }
        Err(error) => emit_replication_inspection_error(
            &error,
            formatter,
            "read replication status",
            "replication_status",
        ),
    }
}

async fn execute_mrf(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket.",
            );
        }
    };
    let admin_client = match setup_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match admin_client.replication_mrf(&bucket).await {
        Ok(mrf) => {
            if formatter.is_json() {
                formatter.json(&replication_mrf_output(mrf));
            } else {
                for line in replication_mrf_lines(&mrf, &formatter) {
                    formatter.println(&line);
                }
            }
            ExitCode::Success
        }
        Err(error) => emit_replication_inspection_error(
            &error,
            &formatter,
            "read replication MRF",
            "replication_mrf",
        ),
    }
}

fn scope_output(scope: Option<&ReplicationMetricScope>) -> String {
    scope
        .map(|scope| scope.as_str().to_string())
        .unwrap_or_else(|| "legacy_unknown".to_string())
}

fn cluster_output(
    available: Option<bool>,
    complete: Option<bool>,
    observed: Option<u32>,
    expected: Option<u32>,
) -> ReplicationClusterOutput {
    ReplicationClusterOutput {
        state: match (available, complete) {
            (Some(false), _) => "unavailable",
            (_, Some(true)) => "complete",
            (_, Some(false)) => "partial",
            (_, None) => "legacy_unknown",
        },
        observed_nodes: observed,
        expected_nodes: expected,
    }
}

fn replication_status_output(
    bucket: &str,
    metrics: ReplicationMetrics,
) -> ReplicationInspectionOutput<ReplicationStatusData> {
    let targets = metrics
        .stats
        .into_iter()
        .map(|(target_arn, target)| {
            let failed = target.fail_stats.unwrap_or(target.failed);
            ReplicationStatusTargetOutput {
                target_arn,
                replicated_count: target.replicated_count,
                replicated_size_bytes: target.replicated_size,
                failed_count: failed.count,
                failed_size_bytes: failed.size,
                latency: ReplicationLatencyOutput {
                    average_ms: target.latency.avg,
                    current_ms: target.latency.curr,
                    maximum_ms: target.latency.max,
                    scope: scope_output(target.latency_scope.as_ref()),
                },
                bandwidth: ReplicationBandwidthOutput {
                    limit_bytes_per_sec: target.bandwidth_limit_bytes_per_sec,
                    current_bytes_per_sec: target.current_bandwidth_bytes_per_sec,
                    scope: scope_output(target.bandwidth_scope.as_ref()),
                },
                extensions: target.extra,
            }
        })
        .collect();
    ReplicationInspectionOutput {
        schema_version: 3,
        output_type: "replication",
        status: "success",
        data: ReplicationStatusData {
            operation: "status",
            bucket: bucket.to_string(),
            availability: match metrics.provider_available {
                Some(true) => "available",
                Some(false) => "unavailable",
                None => "legacy_unknown",
            },
            cluster: cluster_output(
                metrics.provider_available,
                metrics.cluster_complete,
                metrics.observed_node_count,
                metrics.expected_node_count,
            ),
            queue: ReplicationQueueOutput {
                count: metrics.q_stat.curr.count,
                size_bytes: metrics.q_stat.curr.size,
                scope: scope_output(metrics.queue_scope.as_ref()),
            },
            totals: ReplicationTotalsOutput {
                replica_count: metrics.replica_count,
                replica_size_bytes: metrics.replica_size,
                replicated_count: metrics.replicated_count,
                replicated_size_bytes: metrics.replicated_size,
            },
            targets,
            extensions: metrics.extra,
        },
    }
}

fn replication_status_lines(
    bucket: &str,
    metrics: &ReplicationMetrics,
    formatter: &Formatter,
) -> Vec<String> {
    let availability = match metrics.provider_available {
        Some(true) => "available",
        Some(false) => "unavailable",
        None => "legacy/unknown",
    };
    let cluster = cluster_output(
        metrics.provider_available,
        metrics.cluster_complete,
        metrics.observed_node_count,
        metrics.expected_node_count,
    );
    let cluster_detail = match (cluster.observed_nodes, cluster.expected_nodes) {
        (Some(observed), Some(expected)) => {
            format!("{} ({observed}/{expected} nodes)", cluster.state)
        }
        _ => cluster.state.to_string(),
    };
    let mut lines = vec![
        format!(
            "Replication status for '{}': provider={availability}, cluster={}",
            formatter.sanitize_text(bucket),
            cluster_detail
        ),
        format!(
            "Bucket queue: {} objects, {} bytes (scope: {})",
            metrics.q_stat.curr.count,
            metrics.q_stat.curr.size,
            formatter.sanitize_text(&scope_output(metrics.queue_scope.as_ref()))
        ),
        format!(
            "Totals: replicated {} / {} objects, {} / {} bytes",
            metrics.replicated_count,
            metrics.replica_count,
            metrics.replicated_size,
            metrics.replica_size
        ),
    ];
    if metrics.stats.is_empty() {
        lines.push("No target observations were supplied by the server.".into());
    } else {
        lines.push("TARGET  REPLICATED  FAILED  LATENCY SCOPE  BANDWIDTH SCOPE".into());
        for (arn, target) in &metrics.stats {
            let failed = target.fail_stats.as_ref().unwrap_or(&target.failed);
            lines.push(format!(
                "{}  {} / {} bytes  {} / {} bytes  {}  {}",
                formatter.sanitize_text(arn),
                target.replicated_count,
                target.replicated_size,
                failed.count,
                failed.size,
                formatter.sanitize_text(&scope_output(target.latency_scope.as_ref())),
                formatter.sanitize_text(&scope_output(target.bandwidth_scope.as_ref())),
            ));
        }
    }
    lines
}

fn replication_mrf_output(
    mut mrf: ReplicationMrf,
) -> ReplicationInspectionOutput<ReplicationMrfData> {
    mrf.targets.sort_by(|left, right| left.arn.cmp(&right.arn));
    ReplicationInspectionOutput {
        schema_version: 3,
        output_type: "replication",
        status: "success",
        data: ReplicationMrfData {
            operation: "mrf",
            bucket: mrf.bucket,
            runtime_stats_available: mrf.runtime_stats_available,
            cluster: cluster_output(
                Some(mrf.runtime_stats_available),
                Some(mrf.cluster_complete),
                Some(mrf.observed_node_count),
                Some(mrf.expected_node_count),
            ),
            failed: ReplicationBacklogOutput {
                count: mrf.total_failed_count,
                size_bytes: mrf.total_failed_size,
            },
            queue: ReplicationBacklogOutput {
                count: mrf.queued_count,
                size_bytes: mrf.queued_size,
            },
            durable_backlog: ReplicationDurableBacklogOutput {
                available: mrf.durable_backlog_available,
                count: mrf.durable_count,
                size_bytes: mrf.durable_size,
            },
            per_object_entries_available: mrf.per_object_entries_available,
            per_target_durable_entries_available: mrf.per_target_durable_entries_available,
            targets: mrf
                .targets
                .into_iter()
                .map(|target| ReplicationMrfTargetOutput {
                    target_arn: target.arn,
                    failed_count: target.failed_count,
                    failed_size_bytes: target.failed_size,
                    observation_scope: target.observation_scope.as_str().to_string(),
                    extensions: target.extra,
                })
                .collect(),
            extensions: mrf.extra,
        },
    }
}

fn replication_mrf_lines(mrf: &ReplicationMrf, formatter: &Formatter) -> Vec<String> {
    let cluster_state = if !mrf.runtime_stats_available {
        "unavailable"
    } else if mrf.cluster_complete {
        "complete"
    } else {
        "partial"
    };
    let mut lines = vec![
        format!(
            "Replication MRF for '{}': cluster={} ({}/{} nodes)",
            formatter.sanitize_text(&mrf.bucket),
            cluster_state,
            mrf.observed_node_count,
            mrf.expected_node_count
        ),
        format!(
            "Failed: {} objects, {} bytes; queued: {} objects, {} bytes",
            mrf.total_failed_count, mrf.total_failed_size, mrf.queued_count, mrf.queued_size
        ),
        format!(
            "Durable backlog: {} objects, {} bytes (available: {}); per-object entries available: {}",
            mrf.durable_count,
            mrf.durable_size,
            mrf.durable_backlog_available,
            mrf.per_object_entries_available
        ),
    ];
    let mut targets = mrf.targets.iter().collect::<Vec<_>>();
    targets.sort_by(|left, right| left.arn.cmp(&right.arn));
    for target in targets {
        lines.push(format!(
            "{}  failed: {} / {} bytes  scope: {}",
            formatter.sanitize_text(&target.arn),
            target.failed_count,
            target.failed_size,
            formatter.sanitize_text(target.observation_scope.as_str())
        ));
    }
    lines
}

fn emit_replication_inspection_error(
    error: &Error,
    formatter: &Formatter,
    operation: &str,
    capability: &'static str,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("Failed to {operation}: {error}");
    if formatter.is_json() {
        let output = if matches!(error, Error::UnsupportedFeature(_)) {
            ReplicationDiffErrorOutput {
                schema_version: 3,
                output_type: "replication",
                status: "error",
                error: ReplicationDiffError::Unsupported(ReplicationDiffUnsupportedError {
                    error_type: "unsupported_feature",
                    message,
                    retryable: false,
                    capability,
                    server: None,
                    suggestion: Some("Upgrade RustFS or verify that the route is enabled."),
                }),
            }
        } else {
            replication_diff_error_output(error, code, message)
        };
        formatter.json_error(&output);
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

// ==================== Remove ====================

async fn execute_remove(args: RemoveArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if args.id.is_none() && !args.all {
        formatter.error("Either --id or --all is required");
        return ExitCode::UsageError;
    }

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let admin_client = match setup_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    if args.all {
        let targets = match admin_client.list_remote_targets(&bucket).await {
            Ok(targets) => targets,
            Err(error) => {
                formatter.error(&format!("Failed to list remote targets: {error}"));
                return ExitCode::GeneralError;
            }
        };

        let config = match client.get_bucket_replication(&bucket).await {
            Ok(config) => config,
            Err(error) => {
                formatter.error(&format!("Failed to get replication config: {error}"));
                return ExitCode::GeneralError;
            }
        };

        if config.is_none() && targets.is_empty() {
            formatter.error("No replication configuration found on this bucket");
            return ExitCode::NotFound;
        }

        if config.is_some()
            && let Err(error) = client.delete_bucket_replication(&bucket).await
        {
            formatter.error(&format!("Failed to remove replication config: {error}"));
            return ExitCode::GeneralError;
        }

        for target in targets {
            if target.arn.is_empty() {
                continue;
            }
            if let Err(error) = admin_client
                .remove_remote_target(&bucket, &target.arn)
                .await
            {
                formatter.error(&format!(
                    "Failed to remove remote target '{}': {error}",
                    target.arn
                ));
                return ExitCode::GeneralError;
            }
        }

        if formatter.is_json() {
            formatter.json(&ReplicateOperationOutput {
                bucket,
                rule_id: "*".to_string(),
                action: "removed".to_string(),
            });
        } else {
            formatter.success("All replication rules removed.");
        }
        return ExitCode::Success;
    }

    // Remove specific rule by ID
    let rule_id = args.id.as_deref().unwrap_or_default();

    let mut config = match client.get_bucket_replication(&bucket).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            formatter.error("No replication configuration found on this bucket");
            return ExitCode::NotFound;
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let removed_rule = match config
        .rules
        .iter()
        .position(|rule| rule.id == rule_id)
        .map(|index| config.rules.remove(index))
    {
        Some(rule) => rule,
        None => {
            formatter.error(&format!("Rule '{}' not found", rule_id));
            return ExitCode::NotFound;
        }
    };

    let should_remove_target = !removed_rule.destination.bucket_arn.is_empty()
        && !config
            .rules
            .iter()
            .any(|rule| rule.destination.bucket_arn == removed_rule.destination.bucket_arn);

    if config.rules.is_empty() {
        match client.delete_bucket_replication(&bucket).await {
            Ok(()) => {}
            Err(error) => {
                formatter.error(&format!("Failed to remove replication config: {error}"));
                return ExitCode::GeneralError;
            }
        }
    } else {
        match client.set_bucket_replication(&bucket, config).await {
            Ok(()) => {}
            Err(error) => {
                formatter.error(&format!("Failed to update replication config: {error}"));
                return ExitCode::GeneralError;
            }
        }
    }

    if should_remove_target
        && let Err(error) = admin_client
            .remove_remote_target(&bucket, &removed_rule.destination.bucket_arn)
            .await
    {
        formatter.error(&format!(
            "Failed to remove remote target '{}': {error}",
            removed_rule.destination.bucket_arn
        ));
        return ExitCode::GeneralError;
    }

    if formatter.is_json() {
        formatter.json(&ReplicateOperationOutput {
            bucket,
            rule_id: rule_id.to_string(),
            action: "removed".to_string(),
        });
    } else {
        formatter.success(&format!("Replication rule '{}' removed.", rule_id));
    }
    ExitCode::Success
}

// ==================== Export ====================

async fn execute_export(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let admin_client = match setup_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_replication(&bucket).await {
        Ok(Some(config)) => {
            let remote_targets = match admin_client.list_remote_targets(&bucket).await {
                Ok(targets) => relevant_remote_targets(targets, &config),
                Err(error) => {
                    formatter.error(&format!("Failed to list remote targets: {error}"));
                    return ExitCode::GeneralError;
                }
            };
            formatter.json(&ReplicationExport {
                config,
                remote_targets,
            });
            ExitCode::Success
        }
        Ok(None) => {
            formatter.error("No replication configuration found on this bucket");
            ExitCode::NotFound
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Import ====================

async fn execute_import(args: ImportArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let data = match std::fs::read_to_string(&args.file) {
        Ok(data) => data,
        Err(error) => {
            formatter.error(&format!("Failed to read file '{}': {error}", args.file));
            return ExitCode::GeneralError;
        }
    };

    let import: ReplicationExport = match serde_json::from_str(&data) {
        Ok(import) => import,
        Err(error) => {
            formatter.error(&format!("Invalid JSON in '{}': {error}", args.file));
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let mut config = import.config;

    if !import.remote_targets.is_empty() {
        let admin_client = match setup_admin_client(&alias_name, &formatter) {
            Ok(client) => client,
            Err(code) => return code,
        };

        let existing_targets = match admin_client.list_remote_targets(&bucket).await {
            Ok(targets) => targets,
            Err(error) => {
                formatter.error(&format!("Failed to list remote targets: {error}"));
                return ExitCode::GeneralError;
            }
        };

        let mut arn_map = HashMap::new();
        for imported_target in import.remote_targets {
            let mut target = normalize_imported_target(imported_target, &bucket);
            let old_arn = target.arn.clone();

            let resolved_arn = if let Some(existing_target) =
                find_matching_remote_target(&existing_targets, &target)
            {
                target.arn = existing_target.arn.clone();
                match admin_client.set_remote_target(&bucket, target, true).await {
                    Ok(arn) => arn,
                    Err(error) => {
                        formatter.error(&format!("Failed to update remote target: {error}"));
                        return ExitCode::GeneralError;
                    }
                }
            } else {
                target.arn.clear();
                match admin_client.set_remote_target(&bucket, target, false).await {
                    Ok(arn) => arn,
                    Err(error) => {
                        formatter.error(&format!("Failed to create remote target: {error}"));
                        return ExitCode::GeneralError;
                    }
                }
            };

            if !old_arn.is_empty() {
                arn_map.insert(old_arn, resolved_arn);
            }
        }

        remap_replication_arns(&mut config, &arn_map);
    }

    match client.set_bucket_replication(&bucket, config).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = serde_json::json!({
                    "bucket": bucket,
                    "action": "imported",
                    "file": args.file,
                });
                formatter.json(&output);
            } else {
                formatter.success(&format!(
                    "Replication configuration imported from '{}'",
                    args.file
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Target check and resync ====================

async fn execute_check(args: CheckArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias, bucket) = match validate_resync_inputs(&args.path, None, None, None) {
        Ok(validated) => validated,
        Err(error) => return fail_replication(&formatter, &error, "check"),
    };
    if !args.yes {
        return fail_replication(
            &formatter,
            &Error::InvalidPath(
                "Replication check performs a remote write/delete probe; pass --yes to confirm"
                    .to_string(),
            ),
            "check",
        );
    }

    let client = match setup_replication_operation_client(&alias, args.force).await {
        Ok(client) => client,
        Err(error) => return fail_replication(&formatter, &error, "check"),
    };
    match client.check_bucket_replication(&bucket).await {
        Ok(()) => {
            output_replication_success(&formatter, "check", bucket.clone(), Some(true), Vec::new());
            if !formatter.is_json() {
                let display_path = formatter.sanitize_text(&format!("{alias}/{bucket}"));
                formatter.success(&format!(
                    "Replication targets for '{display_path}' passed the active validation probe."
                ));
            }
            ExitCode::Success
        }
        Err(error) => fail_replication(&formatter, &error, "check"),
    }
}

async fn execute_resync_start(args: ResyncStartArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias, bucket) = match validate_resync_inputs(
        &args.path,
        args.target_arn.as_deref(),
        args.reset_id.as_deref(),
        args.older_than.as_deref(),
    ) {
        Ok(validated) => validated,
        Err(error) => return fail_replication(&formatter, &error, "resync_start"),
    };
    if !args.yes {
        return fail_replication(
            &formatter,
            &Error::InvalidPath("Starting a replication resync requires --yes".to_string()),
            "resync_start",
        );
    }
    let older_than = match args
        .older_than
        .as_deref()
        .map(parse_resync_duration)
        .transpose()
    {
        Ok(value) => value,
        Err(error) => return fail_replication(&formatter, &error, "resync_start"),
    };
    let client = match setup_replication_operation_client(&alias, args.force).await {
        Ok(client) => client,
        Err(error) => return fail_replication(&formatter, &error, "resync_start"),
    };
    let options = ReplicationResyncStartOptions {
        target_arn: args.target_arn,
        older_than,
        reset_id: args.reset_id,
    };
    match client
        .start_bucket_replication_resync(&bucket, options)
        .await
    {
        Ok(result) => {
            let target = start_target_output(&result);
            output_replication_success(
                &formatter,
                "resync_start",
                bucket.clone(),
                None,
                vec![target],
            );
            if !formatter.is_json() {
                let display_path = formatter.sanitize_text(&format!("{alias}/{bucket}"));
                formatter.success(&format!(
                    "Replication resync started for '{display_path}' target '{}' with ID '{}'.",
                    formatter.sanitize_text(&result.target_arn),
                    formatter.sanitize_text(&result.reset_id)
                ));
            }
            ExitCode::Success
        }
        Err(error) => fail_replication(&formatter, &error, "resync_start"),
    }
}

async fn execute_resync_status(args: ResyncStatusArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (alias, bucket) =
        match validate_resync_inputs(&args.path, args.target_arn.as_deref(), None, None) {
            Ok(validated) => validated,
            Err(error) => return fail_replication(&formatter, &error, "resync_status"),
        };
    let client = match setup_replication_operation_client(&alias, args.force).await {
        Ok(client) => client,
        Err(error) => return fail_replication(&formatter, &error, "resync_status"),
    };
    match client
        .bucket_replication_resync_status(&bucket, args.target_arn.as_deref())
        .await
    {
        Ok(status) => {
            output_resync_status(&formatter, &alias, &bucket, status);
            ExitCode::Success
        }
        Err(error) => fail_replication(&formatter, &error, "resync_status"),
    }
}

fn output_resync_status(
    formatter: &Formatter,
    alias: &str,
    bucket: &str,
    status: ReplicationResyncStatus,
) {
    let targets = status
        .targets
        .iter()
        .cloned()
        .map(status_target_output)
        .collect::<Vec<_>>();
    output_replication_success(
        formatter,
        "resync_status",
        bucket.to_string(),
        None,
        targets,
    );
    if formatter.is_json() {
        return;
    }
    if status.targets.is_empty() {
        formatter.println("No replication resync status is available.");
        return;
    }
    let display_path = formatter.sanitize_text(&format!("{alias}/{bucket}"));
    formatter.println(&format!("Replication resync status for '{display_path}':"));
    for target in status.targets {
        let has_error = target.error.is_some();
        let lines = resync_status_human_lines(formatter, &target);
        let last_index = lines.len().saturating_sub(1);
        for (index, line) in lines.into_iter().enumerate() {
            if has_error && index == last_index {
                formatter.warning(&line);
            } else {
                formatter.println(&line);
            }
        }
    }
}

fn resync_status_human_lines(
    formatter: &Formatter,
    target: &ReplicationResyncTargetStatus,
) -> Vec<String> {
    let state = serde_json::to_value(&target.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    let server_value = |value: &str| {
        if value.is_empty() {
            "<empty>".to_string()
        } else {
            formatter.sanitize_text(value)
        }
    };
    let timestamp = |value: Option<&jiff::Timestamp>| {
        value
            .map(|value| formatter.sanitize_text(&value.to_string()))
            .unwrap_or_else(|| "<none>".to_string())
    };
    let optional_value = |value: Option<&str>| {
        value
            .map(server_value)
            .unwrap_or_else(|| "<none>".to_string())
    };

    let mut lines = vec![
        format!("  target ARN: {}", server_value(&target.target_arn)),
        format!("    reset ID: {}", server_value(&target.reset_id)),
        format!(
            "    reset before: {}",
            timestamp(target.reset_before.as_ref())
        ),
        format!("    started at: {}", timestamp(target.started_at.as_ref())),
        format!(
            "    last update (server EndTime): {}",
            timestamp(target.last_updated_at.as_ref())
        ),
        format!("    state: {state}"),
        format!("    server state: {}", server_value(&target.server_state)),
        format!(
            "    replicated: {} objects ({} bytes)",
            target.replicated_count, target.replicated_size
        ),
        format!(
            "    failed: {} objects ({} bytes)",
            target.failed_count, target.failed_size
        ),
        format!(
            "    current bucket: {}",
            optional_value(target.current_bucket.as_deref())
        ),
        format!(
            "    current object: {}",
            optional_value(target.current_object.as_deref())
        ),
    ];
    if let Some(error) = target.error.as_deref() {
        lines.push(format!("    server error detail: {}", server_value(error)));
    }
    lines
}

fn output_replication_success(
    formatter: &Formatter,
    operation: &'static str,
    bucket: String,
    valid: Option<bool>,
    targets: Vec<ReplicationV3Target>,
) {
    if formatter.is_json() {
        formatter.json(&ReplicationV3Output {
            schema_version: 3,
            output_type: "replication_operations",
            status: "success",
            data: ReplicationV3Data {
                operation,
                bucket,
                valid,
                targets,
            },
        });
    }
}

fn start_target_output(result: &ReplicationResyncStartResult) -> ReplicationV3Target {
    ReplicationV3Target {
        target_arn: result.target_arn.clone(),
        reset_id: result.reset_id.clone(),
        reset_before: None,
        started_at: None,
        last_updated_at: None,
        state: None,
        server_state: None,
        replicated_count: None,
        replicated_size: None,
        failed_count: None,
        failed_size: None,
        current_bucket: None,
        current_object: None,
        error: None,
    }
}

fn status_target_output(target: ReplicationResyncTargetStatus) -> ReplicationV3Target {
    ReplicationV3Target {
        target_arn: target.target_arn,
        reset_id: target.reset_id,
        reset_before: target.reset_before,
        started_at: target.started_at,
        last_updated_at: target.last_updated_at,
        state: Some(target.state),
        server_state: Some(target.server_state),
        replicated_count: Some(target.replicated_count),
        replicated_size: Some(target.replicated_size),
        failed_count: Some(target.failed_count),
        failed_size: Some(target.failed_size),
        current_bucket: target.current_bucket,
        current_object: target.current_object,
        error: target.error,
    }
}

fn fail_replication(formatter: &Formatter, error: &Error, operation: &str) -> ExitCode {
    let code = exit_code_from_replication_error(error);
    let message = format!("Replication {operation} failed: {error}");
    if formatter.is_json() {
        formatter.json_error(&replication_error_json(error, operation));
        code
    } else {
        formatter.fail(code, &message)
    }
}

fn replication_error_json(error: &Error, operation: &str) -> serde_json::Value {
    let code = exit_code_from_replication_error(error);
    let message = format!("Replication {operation} failed: {error}");
    let (error_type, retryable, suggestion) = replication_error_metadata(code);
    let error_value = if code == ExitCode::UnsupportedFeature {
        let capability = if operation == "check" {
            "bucket_replication_check"
        } else {
            "bucket_replication_resync"
        };
        serde_json::json!({
            "type": "unsupported_feature",
            "message": message,
            "retryable": false,
            "capability": capability,
            "server": null,
            "suggestion": suggestion,
        })
    } else {
        serde_json::json!({
            "type": error_type,
            "message": message,
            "retryable": retryable,
            "suggestion": suggestion,
        })
    };
    serde_json::json!({
        "schema_version": 3,
        "type": "replication_operations",
        "status": "error",
        "error": error_value,
    })
}

fn exit_code_from_replication_error(error: &Error) -> ExitCode {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => ExitCode::UsageError,
        Error::Network(_) => ExitCode::NetworkError,
        Error::Auth(_) => ExitCode::AuthError,
        Error::NotFound(_) | Error::AliasNotFound(_) => ExitCode::NotFound,
        Error::Conflict(_) | Error::AliasExists(_) => ExitCode::Conflict,
        Error::UnsupportedFeature(_) => ExitCode::UnsupportedFeature,
        _ => ExitCode::GeneralError,
    }
}

const fn replication_error_metadata(code: ExitCode) -> (&'static str, bool, Option<&'static str>) {
    match code {
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Review the command arguments and required --yes confirmation."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify endpoint connectivity and retry."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify S3 replication permissions and alias credentials."),
        ),
        ExitCode::NotFound => (
            "not_found",
            false,
            Some("Verify the bucket and replication configuration."),
        ),
        ExitCode::Conflict => (
            "conflict",
            false,
            Some("Review the replication target configuration and server error."),
        ),
        ExitCode::Interrupted => ("interrupted", true, Some("Retry the operation if needed.")),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
        }
    }
}

fn validate_resync_inputs(
    path: &str,
    target_arn: Option<&str>,
    reset_id: Option<&str>,
    older_than: Option<&str>,
) -> Result<(String, String), Error> {
    let (alias, bucket) = parse_bucket_path(path).map_err(Error::InvalidPath)?;
    validate_s3_bucket_name(&bucket)?;
    if let Some(target_arn) = target_arn {
        validate_target_arn(target_arn)?;
    }
    if let Some(reset_id) = reset_id {
        validate_reset_id(reset_id)?;
    }
    if let Some(older_than) = older_than {
        parse_resync_duration(older_than)?;
    }
    Ok((alias, bucket))
}

fn validate_s3_bucket_name(bucket: &str) -> Result<(), Error> {
    let valid_length = (3..=63).contains(&bucket.len());
    let valid_characters = bucket.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
    });
    let valid_labels = bucket
        .split('.')
        .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    if !valid_length
        || !valid_characters
        || !valid_labels
        || bucket.parse::<std::net::Ipv4Addr>().is_ok()
    {
        return Err(Error::InvalidPath(format!(
            "Invalid S3 bucket name: {bucket}"
        )));
    }
    Ok(())
}

fn validate_target_arn(target_arn: &str) -> Result<(), Error> {
    if target_arn.is_empty()
        || target_arn.trim() != target_arn
        || target_arn.len() > 2048
        || target_arn.chars().any(char::is_control)
        || target_arn.chars().any(char::is_whitespace)
        || !target_arn.starts_with("arn:")
        || !target_arn.contains(":replication:")
    {
        return Err(Error::InvalidPath(
            "--target-arn must be a bounded replication ARN without whitespace or control characters"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_reset_id(reset_id: &str) -> Result<(), Error> {
    if reset_id.is_empty()
        || reset_id.trim() != reset_id
        || reset_id.len() > 256
        || reset_id.chars().any(char::is_control)
    {
        return Err(Error::InvalidPath(
            "--reset-id must be 1 to 256 bytes without surrounding whitespace or control characters"
                .to_string(),
        ));
    }
    Ok(())
}

fn parse_resync_duration(value: &str) -> Result<std::time::Duration, Error> {
    let duration = humantime::parse_duration(value).map_err(|error| {
        Error::InvalidPath(format!("Invalid --older-than duration '{value}': {error}"))
    })?;
    if duration.is_zero() {
        return Err(Error::InvalidPath(
            "--older-than must be greater than zero".to_string(),
        ));
    }
    if duration.as_secs() > i64::MAX as u64 {
        return Err(Error::InvalidPath(
            "--older-than exceeds the RustFS duration range".to_string(),
        ));
    }
    Ok(duration)
}

// ==================== Helpers ====================

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    if path.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    if parts.len() < 2 || parts[0].is_empty() {
        return Err("Alias name is required (ALIAS/BUCKET)".to_string());
    }

    if parts.get(2).is_some_and(|key| !key.is_empty()) {
        return Err("Replication path must target a bucket, not an object path".to_string());
    }

    let bucket = parts[1].trim_end_matches('/');
    if bucket.is_empty() {
        return Err("Bucket name is required (ALIAS/BUCKET)".to_string());
    }

    Ok((parts[0].to_string(), bucket.to_string()))
}

fn resolve_alias(alias_name: &str, formatter: &Formatter) -> Result<rc_core::Alias, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
            ));
        }
    };

    match alias_manager.get(alias_name) {
        Ok(alias) => Ok(alias),
        Err(_) => Err(formatter.fail_with_suggestion(
            ExitCode::NotFound,
            &format!("Alias '{alias_name}' not found"),
            "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
        )),
    }
}

async fn setup_replication_operation_client(
    alias_name: &str,
    force: bool,
) -> Result<S3Client, Error> {
    let alias_manager = AliasManager::new()?;
    let alias = alias_manager.get(alias_name)?;
    let client = S3Client::new(alias).await?;

    match client.capabilities().await {
        Ok(capabilities) if !force && !capabilities.replication => {
            return Err(Error::UnsupportedFeature(
                "Backend does not advertise generic bucket replication support".to_string(),
            ));
        }
        Ok(_) => {}
        Err(_) if force => {}
        Err(error) => return Err(error),
    }

    Ok(client)
}

async fn setup_s3_client(
    alias_name: &str,
    bucket: &str,
    force: bool,
    formatter: &Formatter,
) -> Result<S3Client, ExitCode> {
    let alias = match resolve_alias(alias_name, formatter) {
        Ok(alias) => alias,
        Err(code) => return Err(code),
    };

    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {error}"),
            ));
        }
    };

    let caps = match client.capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            if force {
                rc_core::Capabilities::default()
            } else {
                return Err(formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to detect capabilities: {error}"),
                ));
            }
        }
    };

    if !force && !caps.replication {
        return Err(formatter.fail_with_suggestion(
            ExitCode::UnsupportedFeature,
            "Backend does not support replication. Use --force to attempt anyway.",
            "Retry with --force only if you know the backend supports bucket replication.",
        ));
    }

    match client.bucket_exists(bucket).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Bucket '{bucket}' does not exist"),
                "Check the bucket path and retry the replication command.",
            ));
        }
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to check bucket: {error}"),
            ));
        }
    }

    Ok(client)
}

fn setup_admin_client(alias_name: &str, formatter: &Formatter) -> Result<AdminClient, ExitCode> {
    let alias = resolve_alias(alias_name, formatter)?;

    match AdminClient::new(&alias) {
        Ok(client) => Ok(client),
        Err(error) => Err(formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to create admin client: {error}"),
        )),
    }
}

/// Parse --replicate flag value into (delete, delete_marker, existing_objects) booleans.
fn parse_replicate_flags(flags: Option<&str>) -> (bool, bool, bool) {
    let mut delete = false;
    let mut delete_marker = false;
    let mut existing_objects = false;

    if let Some(flags_str) = flags {
        for flag in flags_str.split(',').map(str::trim) {
            match flag.to_lowercase().as_str() {
                "delete" => delete = true,
                "delete-marker" => delete_marker = true,
                "existing-objects" => existing_objects = true,
                _ => {}
            }
        }
    }

    (delete, delete_marker, existing_objects)
}

fn default_replication_role(bucket_arn: &str) -> String {
    bucket_arn.to_string()
}

fn collect_target_arns(config: &ReplicationConfiguration) -> BTreeSet<String> {
    config
        .rules
        .iter()
        .filter_map(|rule| {
            let arn = rule.destination.bucket_arn.trim();
            if arn.is_empty() {
                None
            } else {
                Some(arn.to_string())
            }
        })
        .collect()
}

fn relevant_remote_targets(
    targets: Vec<BucketTarget>,
    config: &ReplicationConfiguration,
) -> Vec<BucketTarget> {
    let referenced = collect_target_arns(config);
    targets
        .into_iter()
        .filter(|target| referenced.contains(target.arn.as_str()))
        .collect()
}

fn target_level_updates_requested(args: &UpdateArgs) -> bool {
    args.storage_class.is_some()
        || args.bandwidth.is_some()
        || args.sync.is_some()
        || args.healthcheck_seconds.is_some()
        || args.disable_proxy.is_some()
        || args.insecure
        || args.ca_cert.is_some()
}

fn apply_target_updates(
    target: &mut BucketTarget,
    args: &UpdateArgs,
    tls_settings: &ReplicationTargetTlsSettings,
) {
    if let Some(storage_class) = &args.storage_class {
        target.storage_class = storage_class.clone();
    }
    if let Some(bandwidth) = args.bandwidth {
        target.bandwidth_limit = bandwidth;
    }
    if let Some(sync) = args.sync {
        target.replication_sync = sync;
    }
    if let Some(healthcheck_seconds) = args.healthcheck_seconds {
        target.health_check_duration = healthcheck_seconds;
    }
    if let Some(disable_proxy) = args.disable_proxy {
        target.disable_proxy = disable_proxy;
    }
    if args.insecure || args.ca_cert.is_some() {
        apply_replication_target_tls_settings(target, tls_settings);
    }
}

fn remap_replication_arns(
    config: &mut ReplicationConfiguration,
    arn_map: &HashMap<String, String>,
) {
    if let Some(updated_role) = arn_map.get(&config.role) {
        config.role = updated_role.clone();
    }

    for rule in &mut config.rules {
        if let Some(updated_arn) = arn_map.get(&rule.destination.bucket_arn) {
            rule.destination.bucket_arn = updated_arn.clone();
        }
    }
}

fn find_matching_remote_target<'a>(
    targets: &'a [BucketTarget],
    expected: &BucketTarget,
) -> Option<&'a BucketTarget> {
    targets.iter().find(|target| {
        target.endpoint == expected.endpoint
            && target.target_bucket == expected.target_bucket
            && target.secure == expected.secure
            && target.region == expected.region
            && target.target_type == expected.target_type
    })
}

fn normalize_imported_target(mut target: BucketTarget, bucket: &str) -> BucketTarget {
    target.source_bucket = bucket.to_string();
    if target.path.is_empty() {
        target.path = DEFAULT_REMOTE_TARGET_PATH.to_string();
    }
    if target.api.is_empty() {
        target.api = DEFAULT_REMOTE_TARGET_API.to_string();
    }
    target
}

fn format_replication_flags(rule: &ReplicationRule) -> String {
    let mut flags = Vec::new();

    if rule.delete_replication == Some(true) {
        flags.push("delete");
    }
    if rule.delete_marker_replication == Some(true) {
        flags.push("delete-marker");
    }
    if rule.existing_object_replication == Some(true) {
        flags.push("existing-objects");
    }

    if flags.is_empty() {
        "-".to_string()
    } else {
        flags.join(",")
    }
}

fn build_replication_target_tls_settings(
    insecure: bool,
    ca_cert: Option<&Path>,
) -> Result<ReplicationTargetTlsSettings, String> {
    if insecure && ca_cert.is_some() {
        return Err("--insecure and --ca-cert cannot be used together".to_string());
    }

    if insecure {
        return Ok(ReplicationTargetTlsSettings {
            skip_tls_verify: Some(true),
            ca_cert_pem: None,
        });
    }

    let Some(path) = ca_cert else {
        return Ok(ReplicationTargetTlsSettings::default());
    };

    let pem = std::fs::read_to_string(path)
        .map_err(|_| "--ca-cert must point to a readable local PEM certificate file".to_string())?;

    if pem.trim().is_empty() {
        return Err("--ca-cert file is empty".to_string());
    }

    if !looks_like_pem_certificate(&pem) {
        return Err("--ca-cert must point to a readable local PEM certificate file".to_string());
    }

    Ok(ReplicationTargetTlsSettings {
        skip_tls_verify: Some(false),
        ca_cert_pem: Some(pem),
    })
}

fn apply_replication_target_tls_settings(
    target: &mut BucketTarget,
    tls_settings: &ReplicationTargetTlsSettings,
) {
    target.skip_tls_verify = tls_settings.skip_tls_verify;
    target.ca_cert_pem = tls_settings.ca_cert_pem.clone();
}

fn looks_like_pem_certificate(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.contains("-----BEGIN CERTIFICATE-----") && trimmed.contains("-----END CERTIFICATE-----")
}

fn remote_target_endpoint(endpoint: &str, insecure: bool) -> (String, bool) {
    let trimmed = endpoint.trim().trim_end_matches('/');

    if let Some(rest) = trimmed.strip_prefix("https://") {
        return (strip_endpoint_path(rest), true);
    }

    if let Some(rest) = trimmed.strip_prefix("http://") {
        return (strip_endpoint_path(rest), false);
    }

    (strip_endpoint_path(trimmed), !insecure)
}

fn strip_endpoint_path(endpoint: &str) -> String {
    endpoint.split('/').next().unwrap_or(endpoint).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    enum StubDiffOutcome {
        Success(ReplicationDiff),
        Auth,
        NotFound,
        Unsupported,
        Network,
        General,
    }

    struct StubReplicationDiffApi {
        outcome: StubDiffOutcome,
    }

    #[async_trait]
    impl ReplicationDiffApi for StubReplicationDiffApi {
        async fn replication_diff(
            &self,
            _bucket: &str,
            _prefix: Option<&str>,
        ) -> rc_core::Result<ReplicationDiff> {
            match &self.outcome {
                StubDiffOutcome::Success(diff) => Ok(diff.clone()),
                StubDiffOutcome::Auth => Err(Error::Auth("Access denied".to_string())),
                StubDiffOutcome::NotFound => {
                    Err(Error::NotFound("replication is not configured".to_string()))
                }
                StubDiffOutcome::Unsupported => {
                    Err(Error::UnsupportedFeature("route unavailable".to_string()))
                }
                StubDiffOutcome::Network => Err(Error::Network("connection reset".to_string())),
                StubDiffOutcome::General => Err(Error::General("malformed response".to_string())),
            }
        }
    }

    fn diff_with_entries(entries: Vec<ReplicationDiffEntry>) -> ReplicationDiff {
        ReplicationDiff {
            entries,
            is_truncated: false,
            scanned_versions: 24,
            extra: BTreeMap::new(),
        }
    }

    fn diff_entry(object: &str, version_id: Option<&str>) -> ReplicationDiffEntry {
        ReplicationDiffEntry {
            object: object.to_string(),
            version_id: version_id.map(str::to_string),
            size_bytes: 42,
            delete_marker: false,
            replication_status: "FAILED".to_string(),
            last_modified: Some("2026-07-21T04:00:00Z".parse().expect("valid timestamp")),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn test_parse_bucket_path_success() {
        let (alias, bucket) = parse_bucket_path("local/my-bucket").expect("should parse");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");

        let (alias, bucket) = parse_bucket_path("local/my-bucket/").expect("should parse");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");
    }

    #[test]
    fn test_parse_bucket_path_errors() {
        assert!(parse_bucket_path("").is_err());
        assert!(parse_bucket_path("local").is_err());
        assert!(parse_bucket_path("/bucket").is_err());
        assert!(parse_bucket_path("local/").is_err());
        assert!(parse_bucket_path("local/my-bucket/object.txt").is_err());
    }

    #[test]
    fn replication_diff_json_is_deterministic_and_preserves_extensions() {
        let mut second = diff_entry("reports/b.json", Some("v2"));
        second.extra.insert(
            "TargetDetail".to_string(),
            serde_json::json!({ "attempts": 2 }),
        );
        let mut diff = diff_with_entries(vec![second, diff_entry("reports/a.json", Some("v1"))]);
        diff.extra
            .insert("ServerRevision".to_string(), serde_json::json!(7));

        let value = serde_json::to_value(replication_diff_output(
            "source",
            Some("reports/".to_string()),
            diff,
        ))
        .expect("replication diff output should serialize");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["type"], "replication");
        assert_eq!(value["data"]["operation"], "diff");
        assert_eq!(value["data"]["entries"][0]["object"], "reports/a.json");
        assert_eq!(
            value["data"]["entries"][1]["extensions"]["TargetDetail"]["attempts"],
            2
        );
        assert_eq!(value["data"]["extensions"]["ServerRevision"], 7);
        assert_eq!(value["data"]["scan"]["resumable"], false);
    }

    #[test]
    fn truncated_empty_human_output_is_partial_and_sanitized() {
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..Default::default()
        });
        let diff = ReplicationDiff {
            entries: Vec::new(),
            is_truncated: true,
            scanned_versions: 10000,
            extra: BTreeMap::new(),
        };

        let lines = replication_diff_lines(
            "bucket\n\u{1b}[31m",
            Some("archive\r\n2026/"),
            &diff,
            &formatter,
        );

        assert!(lines.iter().any(|line| line.contains("Partial scan")));
        assert!(lines.iter().any(|line| line.contains("non-resumable")));
        assert!(lines.iter().any(|line| line.contains("does not prove")));
        assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
        assert!(lines.iter().all(|line| !line.contains('\r')));
        assert!(lines.iter().all(|line| !line.contains('\n')));
    }

    #[tokio::test]
    async fn replication_diff_command_preserves_success_and_error_exit_codes() {
        let formatter = Formatter::new(OutputConfig {
            quiet: true,
            ..Default::default()
        });
        let cases = [
            (
                StubDiffOutcome::Success(diff_with_entries(Vec::new())),
                ExitCode::Success,
            ),
            (StubDiffOutcome::Auth, ExitCode::AuthError),
            (StubDiffOutcome::NotFound, ExitCode::NotFound),
            (StubDiffOutcome::Unsupported, ExitCode::UnsupportedFeature),
            (StubDiffOutcome::Network, ExitCode::NetworkError),
            (StubDiffOutcome::General, ExitCode::GeneralError),
        ];

        for (outcome, expected) in cases {
            let api = StubReplicationDiffApi { outcome };
            assert_eq!(
                execute_diff_with_api("source", None, &api, &formatter).await,
                expected
            );
        }
    }

    #[test]
    fn replication_diff_unsupported_error_uses_specialized_v3_shape() {
        let error = Error::UnsupportedFeature("route unavailable".to_string());
        let value = serde_json::to_value(replication_diff_error_output(
            &error,
            ExitCode::UnsupportedFeature,
            format!("Failed to scan replication diff: {error}"),
        ))
        .expect("replication diff error should serialize");

        assert_eq!(value["type"], "replication");
        assert_eq!(value["error"]["type"], "unsupported_feature");
        assert_eq!(value["error"]["capability"], "replication_diff");
        assert_eq!(value["error"]["server"], serde_json::Value::Null);
    }

    #[test]
    fn test_parse_replicate_flags_none() {
        let (d, dm, eo) = parse_replicate_flags(None);
        assert!(!d);
        assert!(!dm);
        assert!(!eo);
    }

    #[test]
    fn test_parse_replicate_flags_all() {
        let (d, dm, eo) = parse_replicate_flags(Some("delete,delete-marker,existing-objects"));
        assert!(d);
        assert!(dm);
        assert!(eo);
    }

    #[test]
    fn test_parse_replicate_flags_partial() {
        let (d, dm, eo) = parse_replicate_flags(Some("delete-marker"));
        assert!(!d);
        assert!(dm);
        assert!(!eo);
    }

    #[test]
    fn test_parse_replicate_flags_case_insensitive() {
        let (d, _, _) = parse_replicate_flags(Some("DELETE"));
        assert!(d);
    }

    #[test]
    fn test_default_replication_role_uses_destination_arn() {
        let arn = "arn:rustfs:replication:us-east-1:123:test";
        assert_eq!(default_replication_role(arn), arn);
    }

    #[test]
    fn test_build_replication_target_tls_settings_accepts_insecure() {
        let settings = build_replication_target_tls_settings(true, None).expect("tls settings");
        assert_eq!(
            settings,
            ReplicationTargetTlsSettings {
                skip_tls_verify: Some(true),
                ca_cert_pem: None,
            }
        );
    }

    #[test]
    fn test_build_replication_target_tls_settings_rejects_mutually_exclusive_flags() {
        let cert = NamedTempFile::new().expect("temp cert");
        let error = build_replication_target_tls_settings(true, Some(cert.path())).unwrap_err();
        assert_eq!(error, "--insecure and --ca-cert cannot be used together");
    }

    #[test]
    fn test_build_replication_target_tls_settings_rejects_missing_file() {
        let missing = std::env::temp_dir().join("replication-missing-ca.pem");
        let error =
            build_replication_target_tls_settings(false, Some(missing.as_path())).unwrap_err();
        assert_eq!(
            error,
            "--ca-cert must point to a readable local PEM certificate file"
        );
    }

    #[test]
    fn test_build_replication_target_tls_settings_rejects_empty_file() {
        let cert = NamedTempFile::new().expect("temp cert");
        let error = build_replication_target_tls_settings(false, Some(cert.path())).unwrap_err();
        assert_eq!(error, "--ca-cert file is empty");
    }

    #[test]
    fn test_build_replication_target_tls_settings_rejects_non_pem_content() {
        let cert = NamedTempFile::new().expect("temp cert");
        std::fs::write(cert.path(), "not a pem").expect("write invalid cert");
        let error = build_replication_target_tls_settings(false, Some(cert.path())).unwrap_err();
        assert_eq!(
            error,
            "--ca-cert must point to a readable local PEM certificate file"
        );
    }

    #[test]
    fn test_build_replication_target_tls_settings_reads_pem_content() {
        let cert = NamedTempFile::new().expect("temp cert");
        let pem = "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----\n";
        std::fs::write(cert.path(), pem).expect("write cert");

        let settings =
            build_replication_target_tls_settings(false, Some(cert.path())).expect("tls settings");

        assert_eq!(settings.skip_tls_verify, Some(false));
        assert_eq!(settings.ca_cert_pem.as_deref(), Some(pem));
    }

    #[test]
    fn test_collect_target_arns_deduplicates_destinations() {
        let config = ReplicationConfiguration {
            role: String::new(),
            rules: vec![
                ReplicationRule {
                    id: "rule-1".to_string(),
                    priority: 1,
                    status: ReplicationRuleStatus::Enabled,
                    prefix: None,
                    tags: None,
                    destination: ReplicationDestination {
                        bucket_arn: "arn:one".to_string(),
                        storage_class: None,
                    },
                    delete_marker_replication: None,
                    existing_object_replication: None,
                    delete_replication: None,
                },
                ReplicationRule {
                    id: "rule-2".to_string(),
                    priority: 2,
                    status: ReplicationRuleStatus::Enabled,
                    prefix: None,
                    tags: None,
                    destination: ReplicationDestination {
                        bucket_arn: "arn:one".to_string(),
                        storage_class: None,
                    },
                    delete_marker_replication: None,
                    existing_object_replication: None,
                    delete_replication: None,
                },
            ],
        };

        let arns = collect_target_arns(&config);
        assert_eq!(arns.len(), 1);
        assert!(arns.contains("arn:one"));
    }

    #[test]
    fn test_remap_replication_arns_updates_role_and_rules() {
        let mut config = ReplicationConfiguration {
            role: "arn:old".to_string(),
            rules: vec![ReplicationRule {
                id: "rule-1".to_string(),
                priority: 1,
                status: ReplicationRuleStatus::Enabled,
                prefix: None,
                tags: None,
                destination: ReplicationDestination {
                    bucket_arn: "arn:old".to_string(),
                    storage_class: None,
                },
                delete_marker_replication: None,
                existing_object_replication: None,
                delete_replication: None,
            }],
        };

        let mut arn_map = HashMap::new();
        arn_map.insert("arn:old".to_string(), "arn:new".to_string());
        remap_replication_arns(&mut config, &arn_map);

        assert_eq!(config.role, "arn:new");
        assert_eq!(config.rules[0].destination.bucket_arn, "arn:new");
    }

    #[test]
    fn test_replication_export_parses_legacy_config_shape() {
        let payload = r#"{
            "role": "arn:role",
            "rules": []
        }"#;

        let export: ReplicationExport = serde_json::from_str(payload).expect("parse export");
        assert_eq!(export.config.role, "arn:role");
        assert!(export.remote_targets.is_empty());
    }

    #[test]
    fn test_find_matching_remote_target_matches_endpoint_bucket_and_region() {
        let targets = vec![BucketTarget {
            source_bucket: "source".to_string(),
            endpoint: "remote:9000".to_string(),
            target_bucket: "dest".to_string(),
            secure: true,
            target_type: "replication".to_string(),
            region: "us-east-1".to_string(),
            arn: "arn:one".to_string(),
            ..Default::default()
        }];

        let expected = BucketTarget {
            source_bucket: "other".to_string(),
            endpoint: "remote:9000".to_string(),
            target_bucket: "dest".to_string(),
            secure: true,
            target_type: "replication".to_string(),
            region: "us-east-1".to_string(),
            ..Default::default()
        };

        let matched = find_matching_remote_target(&targets, &expected).expect("matching target");
        assert_eq!(matched.arn, "arn:one");
    }

    #[test]
    fn test_target_level_updates_requested_includes_tls_flags() {
        let args = UpdateArgs {
            path: "local/bucket".to_string(),
            id: "rule-1".to_string(),
            replicate: None,
            priority: None,
            storage_class: None,
            bandwidth: None,
            sync: None,
            prefix: None,
            healthcheck_seconds: None,
            disable_proxy: None,
            insecure: true,
            ca_cert: None,
            status: None,
            force: false,
        };

        assert!(target_level_updates_requested(&args));
    }

    #[test]
    fn test_apply_target_updates_clears_existing_ca_when_switching_to_insecure() {
        let mut target = BucketTarget {
            skip_tls_verify: Some(false),
            ca_cert_pem: Some(
                "-----BEGIN CERTIFICATE-----\nold\n-----END CERTIFICATE-----\n".to_string(),
            ),
            ..Default::default()
        };
        let args = UpdateArgs {
            path: "local/bucket".to_string(),
            id: "rule-1".to_string(),
            replicate: None,
            priority: None,
            storage_class: None,
            bandwidth: None,
            sync: None,
            prefix: None,
            healthcheck_seconds: None,
            disable_proxy: None,
            insecure: true,
            ca_cert: None,
            status: None,
            force: false,
        };
        let tls_settings = build_replication_target_tls_settings(true, None).expect("tls settings");

        apply_target_updates(&mut target, &args, &tls_settings);

        assert_eq!(target.skip_tls_verify, Some(true));
        assert_eq!(target.ca_cert_pem, None);
    }

    #[test]
    fn test_format_replication_flags_includes_delete_replication() {
        let rule = ReplicationRule {
            id: "rule-1".to_string(),
            priority: 1,
            status: ReplicationRuleStatus::Enabled,
            prefix: None,
            tags: None,
            destination: ReplicationDestination {
                bucket_arn: "arn:rustfs:replication:us-east-1:123:test".to_string(),
                storage_class: Some("STANDARD".to_string()),
            },
            delete_marker_replication: Some(true),
            existing_object_replication: Some(true),
            delete_replication: Some(true),
        };

        assert_eq!(
            format_replication_flags(&rule),
            "delete,delete-marker,existing-objects"
        );
    }

    #[test]
    fn test_remote_target_endpoint_strips_scheme_and_path() {
        let (endpoint, secure) = remote_target_endpoint("https://localhost:9005/path/", false);
        assert_eq!(endpoint, "localhost:9005");
        assert!(secure);
    }

    #[test]
    fn test_remote_target_endpoint_supports_plain_host_port() {
        let (endpoint, secure) = remote_target_endpoint("localhost:9005", true);
        assert_eq!(endpoint, "localhost:9005");
        assert!(!secure);
    }

    #[test]
    fn test_add_defaults_destination_storage_class_to_standard() {
        let rule = ReplicationRule {
            id: "rule-1".to_string(),
            priority: 1,
            status: ReplicationRuleStatus::Enabled,
            prefix: None,
            tags: None,
            destination: ReplicationDestination {
                bucket_arn: "arn:rustfs:replication:us-east-1:123:test".to_string(),
                storage_class: Some("STANDARD".to_string()),
            },
            delete_marker_replication: Some(false),
            existing_object_replication: Some(false),
            delete_replication: Some(false),
        };

        assert_eq!(rule.destination.storage_class.as_deref(), Some("STANDARD"));
    }

    #[tokio::test]
    async fn test_execute_add_invalid_path_returns_usage_error() {
        let args = ReplicateArgs {
            command: ReplicateCommands::Add(AddArgs {
                path: "no-slash".to_string(),
                remote_bucket: "target/bucket".to_string(),
                replicate: None,
                priority: 1,
                storage_class: None,
                bandwidth: 0,
                sync: false,
                prefix: None,
                id: None,
                healthcheck_seconds: 60,
                disable_proxy: false,
                insecure: false,
                ca_cert: None,
                force: false,
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn test_execute_remove_requires_id_or_all() {
        let args = ReplicateArgs {
            command: ReplicateCommands::Remove(RemoveArgs {
                path: "local/bucket".to_string(),
                id: None,
                all: false,
                force: false,
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn resync_input_validation_is_local_and_conservative() {
        assert!(
            validate_resync_inputs(
                "local/source-bucket",
                Some("arn:rustfs:replication::id:target"),
                Some("caller-reset-1"),
                Some("7d10h31s")
            )
            .is_ok()
        );
        assert!(validate_resync_inputs("local/Bad_Bucket", None, None, None).is_err());
        assert!(
            validate_resync_inputs(
                "local/source-bucket",
                Some("arn:rustfs:replication::id:target\n"),
                None,
                None
            )
            .is_err()
        );
        assert!(
            validate_resync_inputs("local/source-bucket", None, Some(" reset-1"), None).is_err()
        );
    }

    #[test]
    fn resync_duration_accepts_large_values_and_rejects_server_overflow() {
        assert_eq!(
            parse_resync_duration("1h").expect("one hour"),
            std::time::Duration::from_secs(3600)
        );
        assert!(parse_resync_duration("1000y").is_ok());
        assert!(parse_resync_duration("0s").is_err());
        assert!(parse_resync_duration("-1h").is_err());
        assert!(parse_resync_duration("9223372036854775808s").is_err());
    }

    #[test]
    fn replication_v3_status_output_preserves_server_state() {
        let target = status_target_output(ReplicationResyncTargetStatus {
            target_arn: "arn:rustfs:replication::id:backup".to_string(),
            reset_id: "reset-1".to_string(),
            reset_before: None,
            started_at: None,
            last_updated_at: None,
            state: rc_core::ReplicationResyncState::Unknown,
            server_state: "FutureState".to_string(),
            replicated_count: 3,
            replicated_size: 30,
            failed_count: 2,
            failed_size: 20,
            current_bucket: Some("source-bucket".to_string()),
            current_object: Some("last.txt".to_string()),
            error: Some("target unavailable".to_string()),
        });
        let value = serde_json::to_value(ReplicationV3Output {
            schema_version: 3,
            output_type: "replication_operations",
            status: "success",
            data: ReplicationV3Data {
                operation: "resync_status",
                bucket: "source-bucket".to_string(),
                valid: None,
                targets: vec![target],
            },
        })
        .expect("serialize replication v3 output");

        assert_eq!(value["schema_version"], 3);
        assert_eq!(value["type"], "replication_operations");
        assert_eq!(value["data"]["targets"][0]["state"], "unknown");
        assert_eq!(value["data"]["targets"][0]["server_state"], "FutureState");
    }

    #[test]
    fn replication_operation_errors_classify_auth_not_found_and_missing_routes() {
        let cases = [
            (
                Error::Auth("access denied".to_string()),
                ExitCode::AuthError,
                "auth_error",
            ),
            (
                Error::NotFound("replication configuration missing".to_string()),
                ExitCode::NotFound,
                "not_found",
            ),
            (
                Error::UnsupportedFeature("extension route missing".to_string()),
                ExitCode::UnsupportedFeature,
                "unsupported_feature",
            ),
        ];

        for (error, expected_code, expected_type) in cases {
            assert_eq!(exit_code_from_replication_error(&error), expected_code);
            let value = replication_error_json(&error, "resync_status");
            assert_eq!(value["schema_version"], 3);
            assert_eq!(value["type"], "replication_operations");
            assert_eq!(value["status"], "error");
            assert_eq!(value["error"]["type"], expected_type);
        }

        let unsupported = replication_error_json(
            &Error::UnsupportedFeature("extension route missing".to_string()),
            "check",
        );
        assert_eq!(
            unsupported["error"]["capability"],
            "bucket_replication_check"
        );
    }

    #[test]
    fn resync_status_human_lines_retain_and_sanitize_server_fields() {
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..OutputConfig::default()
        });
        let target = ReplicationResyncTargetStatus {
            target_arn: "arn:target\nspoof".to_string(),
            reset_id: "reset\rid".to_string(),
            reset_before: Some("2026-07-01T00:00:00Z".parse().expect("timestamp")),
            started_at: Some("2026-07-02T00:00:00Z".parse().expect("timestamp")),
            last_updated_at: Some("2026-07-02T00:01:00Z".parse().expect("timestamp")),
            state: rc_core::ReplicationResyncState::Unknown,
            server_state: "Future\tState".to_string(),
            replicated_count: 3,
            replicated_size: 30,
            failed_count: 2,
            failed_size: 20,
            current_bucket: Some("source\nbucket".to_string()),
            current_object: Some("last\r.txt".to_string()),
            error: Some("target\tunavailable".to_string()),
        };

        let lines = resync_status_human_lines(&formatter, &target);
        let output = lines.join("\n");
        for expected in [
            "arn:target\\nspoof",
            "reset\\rid",
            "reset before: 2026-07-01T00:00:00Z",
            "started at: 2026-07-02T00:00:00Z",
            "last update (server EndTime): 2026-07-02T00:01:00Z",
            "server state: Future\\tState",
            "current bucket: source\\nbucket",
            "current object: last\\r.txt",
            "server error detail: target\\tunavailable",
        ] {
            assert!(output.contains(expected), "missing human field: {expected}");
        }
        assert!(!output.contains('\r'));
        assert!(!output.contains('\t'));
    }

    #[tokio::test]
    async fn check_requires_confirmation_before_alias_or_network_setup() {
        let args = ReplicateArgs {
            command: ReplicateCommands::Check(CheckArgs {
                path: "missing-alias/source-bucket".to_string(),
                yes: false,
                force: false,
            }),
        };

        let code = execute(args, OutputConfig::default()).await;

        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn resync_start_requires_confirmation_before_alias_or_network_setup() {
        let args = ReplicateArgs {
            command: ReplicateCommands::Resync(ResyncCommands::Start(ResyncStartArgs {
                path: "missing-alias/source-bucket".to_string(),
                target_arn: None,
                older_than: None,
                reset_id: None,
                yes: false,
                force: false,
            })),
        };

        let code = execute(args, OutputConfig::default()).await;

        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn status_output_preserves_partial_truth_without_inventing_target_queue_or_uptime() {
        let metrics: ReplicationMetrics = serde_json::from_str(
            r#"{"stats":{"arn:b":{"replicated_size":3,"replicated_count":2,
            "failed":{"count":1,"size":4},"fail_stats":{"count":1,"size":4},
            "latency":{"avg":1,"curr":2,"max":3},
            "xfer_rate_lrg":{"avg":0,"curr":0,"peak":0},
            "xfer_rate_sml":{"avg":0,"curr":0,"peak":0},
            "bandwidth_limit_bytes_per_sec":10,"current_bandwidth_bytes_per_sec":5,
            "latency_scope":"partial_cluster","bandwidth_scope":"node_local"}},
            "replica_size":4,"replica_count":3,"replicated_size":3,"replicated_count":2,
            "q_stat":{"curr":{"count":1,"bytes":4},"avg":{"count":1,"bytes":4},
            "max":{"count":1,"bytes":4},"last_minute":{"count":1,"bytes":4}},
            "provider_available":true,"cluster_complete":false,
            "observed_node_count":1,"expected_node_count":2,"queue_scope":"partial_cluster"}"#,
        )
        .expect("metrics");

        let value = serde_json::to_value(replication_status_output("source", metrics))
            .expect("status JSON");

        assert_eq!(value["data"]["availability"], "available");
        assert_eq!(value["data"]["cluster"]["state"], "partial");
        assert_eq!(value["data"]["queue"]["count"], 1);
        assert_eq!(value["data"]["targets"][0]["failed_count"], 1);
        assert!(value["data"]["targets"][0].get("queue").is_none());
        assert!(value["data"]["targets"][0].get("uptime").is_none());
        assert!(value["data"].get("healthy").is_none());
    }

    #[test]
    fn status_output_marks_legacy_availability_unknown() {
        let metrics: ReplicationMetrics = serde_json::from_str(
            r#"{"stats":{},"replica_size":0,"replica_count":0,
            "replicated_size":0,"replicated_count":0,
            "q_stat":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},
            "max":{"count":0,"bytes":0},"last_minute":{"count":0,"bytes":0}}}"#,
        )
        .expect("legacy metrics");
        let value = serde_json::to_value(replication_status_output("source", metrics))
            .expect("status JSON");
        assert_eq!(value["data"]["availability"], "legacy_unknown");
        assert_eq!(value["data"]["cluster"]["state"], "legacy_unknown");
        assert_eq!(value["data"]["queue"]["scope"], "legacy_unknown");
    }

    #[test]
    fn status_output_keeps_unavailable_provider_distinct_from_valid_empty() {
        let unavailable: ReplicationMetrics = serde_json::from_str(
            r#"{"stats":{},"replica_size":0,"replica_count":0,
            "replicated_size":0,"replicated_count":0,
            "q_stat":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},
            "max":{"count":0,"bytes":0},"last_minute":{"count":0,"bytes":0}},
            "provider_available":false,"cluster_complete":false,
            "observed_node_count":0,"expected_node_count":2,"queue_scope":"unavailable"}"#,
        )
        .expect("unavailable metrics");
        let unavailable =
            serde_json::to_value(replication_status_output("source", unavailable)).expect("JSON");
        assert_eq!(unavailable["data"]["availability"], "unavailable");
        assert_eq!(unavailable["data"]["cluster"]["state"], "unavailable");

        let available: ReplicationMetrics = serde_json::from_str(
            r#"{"stats":{},"replica_size":0,"replica_count":0,
            "replicated_size":0,"replicated_count":0,
            "q_stat":{"curr":{"count":0,"bytes":0},"avg":{"count":0,"bytes":0},
            "max":{"count":0,"bytes":0},"last_minute":{"count":0,"bytes":0}},
            "provider_available":true,"cluster_complete":true,
            "observed_node_count":2,"expected_node_count":2,"queue_scope":"cluster_aggregated"}"#,
        )
        .expect("valid empty metrics");
        let available =
            serde_json::to_value(replication_status_output("source", available)).expect("JSON");
        assert_eq!(available["data"]["availability"], "available");
        assert_eq!(available["data"]["cluster"]["state"], "complete");
    }

    #[test]
    fn mrf_output_is_sorted_and_does_not_fabricate_object_rows() {
        let mrf: ReplicationMrf = serde_json::from_str(
            r#"{"Bucket":"source","Targets":[
            {"ARN":"z","FailedCount":1,"FailedSize":2,"ObservationScope":"node_local"},
            {"ARN":"a","FailedCount":2,"FailedSize":3,"ObservationScope":"partial_cluster"}],
            "TotalFailedCount":3,"TotalFailedSize":5,"QueuedCount":4,"QueuedSize":6,
            "PerObjectEntriesAvailable":false,"RuntimeStatsAvailable":true,
            "ClusterComplete":false,"ObservedNodeCount":1,"ExpectedNodeCount":2,
            "DurableBacklogAvailable":true,"DurableCount":7,"DurableSize":8,
            "PerTargetDurableEntriesAvailable":false}"#,
        )
        .expect("MRF");
        let value =
            serde_json::to_value(replication_mrf_output(mrf)).expect("deterministic MRF JSON");
        assert_eq!(value["data"]["targets"][0]["target_arn"], "a");
        assert_eq!(value["data"]["per_object_entries_available"], false);
        assert!(value["data"].get("entries").is_none());
        assert!(value["data"]["targets"][0].get("queued_count").is_none());
    }

    #[test]
    fn inspection_human_output_sanitizes_server_strings() {
        let mrf: ReplicationMrf = serde_json::from_str(
            r#"{"Bucket":"source\nspoof","Targets":[
            {"ARN":"arn:\tspoof","FailedCount":0,"FailedSize":0,"ObservationScope":"future\rvalue"}],
            "TotalFailedCount":0,"TotalFailedSize":0,"QueuedCount":0,"QueuedSize":0,
            "PerObjectEntriesAvailable":false,"RuntimeStatsAvailable":true,
            "ClusterComplete":true,"ObservedNodeCount":1,"ExpectedNodeCount":1,
            "DurableBacklogAvailable":false,"DurableCount":0,"DurableSize":0,
            "PerTargetDurableEntriesAvailable":false}"#,
        )
        .expect("MRF");
        let formatter = Formatter::new(OutputConfig {
            no_color: true,
            ..OutputConfig::default()
        });
        let output = replication_mrf_lines(&mrf, &formatter).join("\n");
        assert!(output.contains("source\\nspoof"));
        assert!(output.contains("arn:\\tspoof"));
        assert!(output.contains("future\\rvalue"));
        assert!(!output.contains('\r'));
        assert!(!output.contains('\t'));
    }
}
