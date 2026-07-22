//! cp command - Copy objects
//!
//! Copies objects between local filesystem and S3, or between S3 locations.

use clap::Args;
use jiff::Timestamp;
use rc_core::alias::RetryConfig;
use rc_core::{
    AliasManager, Error, ObjectEncryptionRequest, ObjectStore as _, ParsedPath, RemotePath,
    TransferCandidate, TransferControls, TransferExecutor, TransferOutcomeState, TransferPlan,
    TransferSelection, parse_path,
};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig, ProgressBar};

const CP_AFTER_HELP: &str = "\
Examples:
  rc object copy ./report.json local/my-bucket/reports/
  rc cp ./report.json local/my-bucket/reports/
  rc object copy local/source-bucket/archive.tar.gz ./downloads/archive.tar.gz";

const REMOTE_PATH_SUGGESTION: &str =
    "Use a local filesystem path or a remote path in the form alias/bucket[/key].";
const DEFAULT_TRANSFER_CONCURRENCY: usize = 4;
const DEFAULT_RETRY_ATTEMPTS: u32 = 3;
const DEFAULT_RETRY_INITIAL_BACKOFF_MS: u64 = 100;
const DEFAULT_RETRY_MAX_BACKOFF_MS: u64 = 10_000;
const MAX_SINGLE_COPY_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// Copy objects
#[derive(Args, Clone, Debug)]
#[command(after_help = CP_AFTER_HELP)]
pub struct CpArgs {
    /// Source paths (local paths or alias/bucket/key)
    #[arg(required = true, num_args = 1.., value_name = "SOURCE")]
    pub sources: Vec<String>,

    /// Destination path (local path or alias/bucket/key)
    pub target: String,

    /// Copy recursively
    #[arg(short, long)]
    pub recursive: bool,

    /// Preserve file attributes
    #[arg(short, long)]
    pub preserve: bool,

    /// Continue on errors
    #[arg(long)]
    pub continue_on_error: bool,

    /// Overwrite destination if it exists
    #[arg(long, default_value = "true")]
    pub overwrite: bool,

    /// Only show what would be copied (dry run)
    #[arg(long)]
    pub dry_run: bool,

    /// Storage class for destination (S3 only)
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Content type for uploaded files
    #[arg(long)]
    pub content_type: Option<String>,

    /// Apply SSE-S3 to the remote destination path
    #[arg(long = "enc-s3")]
    pub enc_s3: Vec<String>,

    /// Apply SSE-KMS to the remote destination path as TARGET=KMS_KEY_ID
    #[arg(long = "enc-kms")]
    pub enc_kms: Vec<String>,

    /// Include source-relative paths matching this glob (repeatable)
    #[arg(long)]
    pub include: Vec<String>,

    /// Exclude source-relative paths matching this glob; exclusions always win (repeatable)
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Select objects modified more recently than this age (for example 1h or 7d)
    #[arg(long)]
    pub newer_than: Option<String>,

    /// Select objects modified less recently than this age (for example 1h or 7d)
    #[arg(long)]
    pub older_than: Option<String>,

    /// Select object state at or before a UTC timestamp or age
    #[arg(long)]
    pub rewind: Option<String>,

    /// Maximum number of transfers in flight across the command
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Aggregate transfer start rate in bytes per second (for example 10MiB/s)
    #[arg(long)]
    pub rate_limit: Option<String>,

    /// Maximum attempts for transient transfer failures
    #[arg(long)]
    pub retry_attempts: Option<u32>,

    /// Initial transient retry backoff in milliseconds
    #[arg(long)]
    pub retry_initial_backoff_ms: Option<u64>,

    /// Maximum transient retry backoff in milliseconds
    #[arg(long)]
    pub retry_max_backoff_ms: Option<u64>,

    /// Return not-found when selection produces no transfer candidates
    #[arg(long)]
    pub fail_empty: bool,

    /// Print deterministic aggregate transfer counters (human output)
    #[arg(long)]
    pub summary: bool,
}

impl CpArgs {
    pub(crate) fn single(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            sources: vec![source.into()],
            target: target.into(),
            recursive: false,
            preserve: false,
            continue_on_error: false,
            overwrite: true,
            dry_run: false,
            storage_class: None,
            content_type: None,
            enc_s3: Vec::new(),
            enc_kms: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            newer_than: None,
            older_than: None,
            rewind: None,
            concurrency: None,
            rate_limit: None,
            retry_attempts: None,
            retry_initial_backoff_ms: None,
            retry_max_backoff_ms: None,
            fail_empty: false,
            summary: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct CpOutput {
    status: &'static str,
    source: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_human: Option<String>,
}

/// Execute the cp command
pub async fn execute(args: CpArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if args.preserve {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--preserve is not implemented; refusing to continue with a silently ignored option",
        );
    }
    if args.storage_class.is_some() {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--storage-class is not implemented; refusing to continue with a silently ignored option",
        );
    }

    if formatter.is_json() && uses_transfer_planner(&args) {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "Bulk transfer planning currently supports human output only; JSON batch output requires the versioned output contract",
        );
    }

    let selection = match build_transfer_selection(&args, Timestamp::now()) {
        Ok(selection) => selection,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };
    let controls = match build_transfer_controls(&args) {
        Ok(controls) => controls,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };

    let alias_manager = AliasManager::new();

    // Parse every operand before starting any transfer.
    let mut sources = Vec::with_capacity(args.sources.len());
    for source in &args.sources {
        match parse_cp_path(source, alias_manager.as_ref().ok()) {
            Ok(path) => sources.push(path),
            Err(error) => {
                return formatter.fail_with_suggestion(
                    ExitCode::UsageError,
                    &format!("Invalid source path '{source}': {error}"),
                    REMOTE_PATH_SUGGESTION,
                );
            }
        }
    }

    let target = match parse_cp_path(&args.target, alias_manager.as_ref().ok()) {
        Ok(p) => p,
        Err(e) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &format!("Invalid target path: {e}"),
                REMOTE_PATH_SUGGESTION,
            );
        }
    };

    let target_is_container = is_container_target(&args.target, &target);
    if args.sources.len() > 1 && !target_is_container {
        return formatter.fail(
            ExitCode::UsageError,
            "Multiple copy sources require a directory or remote prefix destination ending in '/'",
        );
    }

    let target_encryption = match parse_destination_encryption(&args.enc_s3, &args.enc_kms, &target)
    {
        Ok(encryption) => encryption,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };

    let single_local_to_local = matches!(
        (sources.as_slice(), &target),
        ([ParsedPath::Local(_)], ParsedPath::Local(_))
    );
    if uses_transfer_planner(&args) && !single_local_to_local {
        let alias_manager = match alias_manager.as_ref() {
            Ok(manager) => manager,
            Err(error) => {
                return formatter.fail(
                    ExitCode::UsageError,
                    &format!("Failed to load aliases: {error}"),
                );
            }
        };
        return execute_transfer_plan(
            &args,
            sources,
            target,
            target_is_container,
            target_encryption,
            selection,
            controls,
            &formatter,
            alias_manager,
        )
        .await;
    }

    let Some(source) = sources.first() else {
        return formatter.fail(ExitCode::UsageError, "At least one copy source is required");
    };

    execute_single_copy(
        source,
        &target,
        &args,
        &formatter,
        target_encryption.as_ref(),
    )
    .await
}

fn uses_transfer_planner(args: &CpArgs) -> bool {
    args.sources.len() > 1
        || args.recursive
        || !args.include.is_empty()
        || !args.exclude.is_empty()
        || args.newer_than.is_some()
        || args.older_than.is_some()
        || args.rewind.is_some()
        || args.concurrency.is_some()
        || args.rate_limit.is_some()
        || args.retry_attempts.is_some()
        || args.retry_initial_backoff_ms.is_some()
        || args.retry_max_backoff_ms.is_some()
        || args.fail_empty
        || args.summary
}

async fn execute_single_copy(
    source: &ParsedPath,
    target: &ParsedPath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
) -> ExitCode {
    // Determine copy direction.
    match (source, target) {
        (ParsedPath::Local(src), ParsedPath::Remote(dst)) => {
            copy_local_to_s3_prepared(src, dst, args, formatter, encryption).await
        }
        (ParsedPath::Remote(src), ParsedPath::Local(dst)) => {
            copy_s3_to_local(src, dst, args, formatter).await
        }
        (ParsedPath::Remote(src), ParsedPath::Remote(dst)) => {
            if args.recursive {
                return formatter.fail(
                    ExitCode::UnsupportedFeature,
                    "Recursive S3-to-S3 copy is not implemented",
                );
            }
            copy_s3_to_s3_prepared(src, dst, args, formatter, encryption).await
        }
        (ParsedPath::Local(_), ParsedPath::Local(_)) => formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Cannot copy between two local paths. Use system cp command.",
            "Use your local shell cp command when both paths are on the filesystem.",
        ),
    }
}

#[derive(Debug, Clone)]
enum CpOperation {
    LocalToRemote {
        source: PathBuf,
        target: RemotePath,
        encryption: Option<ObjectEncryptionRequest>,
    },
    RemoteToLocal {
        source: RemotePath,
        target: PathBuf,
    },
    RemoteToRemote {
        source: RemotePath,
        target: RemotePath,
        encryption: Option<ObjectEncryptionRequest>,
    },
}

#[allow(clippy::too_many_arguments)]
async fn execute_transfer_plan(
    args: &CpArgs,
    sources: Vec<ParsedPath>,
    target: ParsedPath,
    target_is_container: bool,
    encryption: Option<ObjectEncryptionRequest>,
    selection: TransferSelection,
    controls: TransferControls,
    formatter: &Formatter,
    alias_manager: &AliasManager,
) -> ExitCode {
    let candidates = match build_transfer_candidates(
        &sources,
        &target,
        target_is_container,
        args.recursive,
        encryption,
        alias_manager,
    )
    .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            return formatter.fail(
                exit_code_for_core_error(&error),
                &format!("Failed to plan copy: {error}"),
            );
        }
    };

    let plan = TransferPlan::build(candidates, &selection);
    if let Err(error) = validate_plan_targets(&plan) {
        return formatter.fail(ExitCode::UsageError, &error.to_string());
    }
    if plan.items.is_empty() {
        if args.fail_empty {
            return formatter.fail(
                ExitCode::NotFound,
                "No copy sources matched the requested selection",
            );
        }
        if args.summary || args.recursive || args.sources.len() > 1 {
            print_transfer_summary(formatter, &plan.summary);
        }
        return ExitCode::Success;
    }

    if args.dry_run {
        for item in &plan.items {
            formatter.println(&format!(
                "Would copy: {} -> {}",
                formatter.style_file(&item.source),
                formatter.style_file(&item.target)
            ));
        }
        if args.summary || args.recursive || args.sources.len() > 1 {
            print_transfer_summary(formatter, &plan.summary);
        }
        return ExitCode::Success;
    }

    let clients = match create_planned_client_cache(&plan.items, alias_manager).await {
        Ok(clients) => Arc::new(clients),
        Err(error) => {
            return formatter.fail(
                exit_code_for_core_error(&error),
                &format!("Failed to prepare copy clients: {error}"),
            );
        }
    };
    let executor = match TransferExecutor::new(controls) {
        Ok(executor) => executor,
        Err(error) => {
            return formatter.fail(ExitCode::UsageError, &error.to_string());
        }
    };
    let operation_args = Arc::new(args.clone());
    let report = executor
        .execute(plan, {
            let operation_args = Arc::clone(&operation_args);
            let clients = Arc::clone(&clients);
            move |item| {
                let operation_args = Arc::clone(&operation_args);
                let clients = Arc::clone(&clients);
                async move { execute_planned_operation(item, &operation_args, &clients).await }
            }
        })
        .await;

    for outcome in &report.outcomes {
        match &outcome.state {
            TransferOutcomeState::Success { bytes_transferred } => {
                print_planned_success(formatter, &outcome.item, *bytes_transferred);
            }
            TransferOutcomeState::Failed { error } => {
                formatter.error_with_code(exit_code_for_core_error(error), &error.to_string());
            }
            TransferOutcomeState::Cancelled => formatter.warning(&format!(
                "Cancelled before transfer: {}",
                outcome.item.source
            )),
        }
    }

    if args.summary || args.recursive || args.sources.len() > 1 {
        print_transfer_summary(formatter, &report.summary);
    }

    report
        .first_failure()
        .map_or(ExitCode::Success, exit_code_for_core_error)
}

async fn execute_planned_operation(
    item: TransferCandidate<CpOperation>,
    args: &CpArgs,
    clients: &HashMap<String, Arc<S3Client>>,
) -> rc_core::Result<u64> {
    let client = planned_client(clients, operation_alias(&item.payload))?;
    match &item.payload {
        CpOperation::LocalToRemote {
            source,
            target,
            encryption,
        } => perform_planned_upload(client, source, target, encryption.as_ref(), args).await,
        CpOperation::RemoteToLocal { source, target } => {
            perform_planned_download(client, source, target, args).await
        }
        CpOperation::RemoteToRemote {
            source,
            target,
            encryption,
        } => {
            perform_planned_remote_copy(
                client,
                source,
                target,
                encryption.as_ref(),
                item.size_bytes,
            )
            .await
        }
    }
}

async fn perform_planned_upload(
    client: &S3Client,
    source: &Path,
    target: &RemotePath,
    encryption: Option<&ObjectEncryptionRequest>,
    args: &CpArgs,
) -> rc_core::Result<u64> {
    let metadata = tokio::fs::metadata(source).await?;
    let file_size = metadata.len();
    let guessed_type = mime_guess::from_path(source)
        .first()
        .map(|mime| mime.essence_str().to_string());
    let content_type = select_upload_content_type(
        args.content_type.as_deref(),
        guessed_type.as_deref(),
        file_size,
    );
    let info = client
        .put_object_from_path(target, source, content_type, encryption, |_| {})
        .await?;
    Ok(info
        .size_bytes
        .and_then(|size| u64::try_from(size).ok())
        .unwrap_or(file_size))
}

async fn perform_planned_download(
    client: &S3Client,
    source: &RemotePath,
    target: &Path,
    args: &CpArgs,
) -> rc_core::Result<u64> {
    if target.exists() && !args.overwrite {
        return Err(Error::Conflict(format!(
            "Destination exists: {}. Use --overwrite to replace.",
            target.display()
        )));
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    client
        .download_object_to_path(source, target, |_, _| {})
        .await
}

async fn perform_planned_remote_copy(
    client: &S3Client,
    source: &RemotePath,
    target: &RemotePath,
    encryption: Option<&ObjectEncryptionRequest>,
    planned_size: Option<u64>,
) -> rc_core::Result<u64> {
    if source.alias != target.alias {
        return Err(Error::UnsupportedFeature(
            "Cross-alias S3-to-S3 copy is not supported".to_string(),
        ));
    }
    if requires_multipart_copy(planned_size) {
        return Err(Error::UnsupportedFeature(
            "Objects larger than 5 GiB require multipart copy, which is not implemented"
                .to_string(),
        ));
    }
    let copied = client.copy_object(source, target, encryption).await?;
    Ok(copied
        .size_bytes
        .and_then(|size| u64::try_from(size).ok())
        .or(planned_size)
        .unwrap_or_default())
}

fn requires_multipart_copy(planned_size: Option<u64>) -> bool {
    planned_size.is_some_and(|size| size > MAX_SINGLE_COPY_SIZE)
}

fn operation_alias(operation: &CpOperation) -> &str {
    match operation {
        CpOperation::LocalToRemote { target, .. } => &target.alias,
        CpOperation::RemoteToLocal { source, .. } | CpOperation::RemoteToRemote { source, .. } => {
            &source.alias
        }
    }
}

fn planned_client_aliases(items: &[TransferCandidate<CpOperation>]) -> BTreeSet<String> {
    items
        .iter()
        .map(|item| operation_alias(&item.payload).to_string())
        .collect()
}

async fn create_planned_client_cache(
    items: &[TransferCandidate<CpOperation>],
    alias_manager: &AliasManager,
) -> rc_core::Result<HashMap<String, Arc<S3Client>>> {
    let mut clients = HashMap::new();
    for alias_name in planned_client_aliases(items) {
        match create_leaf_s3_client(alias_manager, &alias_name).await {
            Ok(client) => {
                clients.insert(alias_name, Arc::new(client));
            }
            // Keep a missing alias as an item-level failure so continue-on-error and summaries
            // retain their existing behavior without reloading configuration in each worker.
            Err(Error::AliasNotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(clients)
}

fn planned_client<'a>(
    clients: &'a HashMap<String, Arc<S3Client>>,
    alias_name: &str,
) -> rc_core::Result<&'a S3Client> {
    clients
        .get(alias_name)
        .map(Arc::as_ref)
        .ok_or_else(|| Error::AliasNotFound(alias_name.to_string()))
}

async fn create_leaf_s3_client(
    alias_manager: &AliasManager,
    alias_name: &str,
) -> rc_core::Result<S3Client> {
    let mut alias = alias_manager
        .get(alias_name)
        .map_err(|_| Error::AliasNotFound(alias_name.to_string()))?;
    // The shared executor classifies failures and owns the exact attempt budget. Disabling adapter
    // retries here prevents nested retries from exceeding the command-level policy.
    alias.retry = Some(RetryConfig {
        max_attempts: 1,
        initial_backoff_ms: 1,
        max_backoff_ms: 1,
    });
    S3Client::new(alias).await
}

fn print_planned_success(
    formatter: &Formatter,
    item: &TransferCandidate<CpOperation>,
    bytes_transferred: u64,
) {
    let size_bytes = i64::try_from(bytes_transferred).ok();
    let size_human = size_bytes.map(|size| humansize::format_size(size as u64, humansize::BINARY));
    if formatter.is_json() {
        formatter.json(&CpOutput {
            status: "success",
            source: item.source.clone(),
            target: item.target.clone(),
            size_bytes,
            size_human,
        });
    } else {
        formatter.println(&format!(
            "{} -> {} ({})",
            formatter.style_file(&item.source),
            formatter.style_file(&item.target),
            formatter.style_size(size_human.as_deref().unwrap_or_default())
        ));
    }
}

fn print_transfer_summary(formatter: &Formatter, summary: &rc_core::TransferSummary) {
    formatter.println(&format!(
        "Summary: {} planned, {} skipped, {} succeeded, {} failed, {} cancelled, {} transferred",
        summary.planned,
        summary.skipped,
        summary.successful,
        summary.failed,
        summary.cancelled,
        humansize::format_size(summary.transferred_bytes, humansize::BINARY)
    ));
}

fn exit_code_for_core_error(error: &Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

fn build_transfer_selection(args: &CpArgs, now: Timestamp) -> Result<TransferSelection, String> {
    let newer_than = args
        .newer_than
        .as_deref()
        .map(|value| parse_age_cutoff(value, now))
        .transpose()?;
    let older_than = args
        .older_than
        .as_deref()
        .map(|value| parse_age_cutoff(value, now))
        .transpose()?;
    let rewind = args
        .rewind
        .as_deref()
        .map(|value| parse_rewind_cutoff(value, now))
        .transpose()?;

    TransferSelection::new(&args.include, &args.exclude, newer_than, older_than, rewind)
        .map_err(|error| error.to_string())
}

fn build_transfer_controls(args: &CpArgs) -> Result<TransferControls, String> {
    let bytes_per_second = args
        .rate_limit
        .as_deref()
        .map(parse_byte_rate)
        .transpose()?;
    let controls = TransferControls {
        concurrency: args.concurrency.unwrap_or(DEFAULT_TRANSFER_CONCURRENCY),
        bytes_per_second,
        retry: RetryConfig {
            max_attempts: args.retry_attempts.unwrap_or(DEFAULT_RETRY_ATTEMPTS),
            initial_backoff_ms: args
                .retry_initial_backoff_ms
                .unwrap_or(DEFAULT_RETRY_INITIAL_BACKOFF_MS),
            max_backoff_ms: args
                .retry_max_backoff_ms
                .unwrap_or(DEFAULT_RETRY_MAX_BACKOFF_MS),
        },
        continue_on_error: args.continue_on_error,
    };
    controls.validate().map_err(|error| error.to_string())?;
    Ok(controls)
}

pub(super) fn parse_age_cutoff(value: &str, now: Timestamp) -> Result<Timestamp, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Transfer age must not be empty".to_string());
    }
    let suffix_index = value
        .find(|character: char| character.is_ascii_alphabetic())
        .unwrap_or(value.len());
    let number = &value[..suffix_index];
    let suffix = &value[suffix_index..];
    let amount: i64 = number
        .parse()
        .map_err(|_| format!("Invalid transfer age number: {number}"))?;
    if amount < 0 {
        return Err("Transfer age must not be negative".to_string());
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("Unknown transfer age suffix: {suffix}")),
    };
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "Transfer age is too large".to_string())?;
    now.checked_sub(jiff::Span::new().seconds(seconds))
        .map_err(|error| format!("Transfer age overflow: {error}"))
}

fn parse_rewind_cutoff(value: &str, now: Timestamp) -> Result<Timestamp, String> {
    value
        .parse::<Timestamp>()
        .or_else(|_| parse_age_cutoff(value, now))
        .map_err(|error| format!("Invalid rewind value '{value}': {error}"))
}

pub(super) fn parse_byte_rate(value: &str) -> Result<u64, String> {
    let normalized = value.trim().to_ascii_lowercase();
    let normalized = normalized
        .strip_suffix("/s")
        .or_else(|| normalized.strip_suffix("ps"))
        .unwrap_or(&normalized);
    let suffix_index = normalized
        .find(|character: char| character.is_ascii_alphabetic())
        .unwrap_or(normalized.len());
    let number = &normalized[..suffix_index];
    let suffix = &normalized[suffix_index..];
    let amount: u64 = number
        .parse()
        .map_err(|_| format!("Invalid transfer rate number: {number}"))?;
    let multiplier = match suffix {
        "" | "b" => 1u64,
        "k" | "kb" => 1_000,
        "ki" | "kib" => 1_024,
        "m" | "mb" => 1_000_000,
        "mi" | "mib" => 1_048_576,
        "g" | "gb" => 1_000_000_000,
        "gi" | "gib" => 1_073_741_824,
        _ => return Err(format!("Unknown transfer rate suffix: {suffix}")),
    };
    let rate = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "Transfer rate is too large".to_string())?;
    if rate == 0 {
        return Err("Transfer rate must be greater than zero".to_string());
    }
    Ok(rate)
}

fn is_container_target(raw: &str, target: &ParsedPath) -> bool {
    match target {
        ParsedPath::Local(path) => path.is_dir() || raw.ends_with(['/', '\\']),
        ParsedPath::Remote(path) => path.key.is_empty() || raw.ends_with('/'),
    }
}

async fn build_transfer_candidates(
    sources: &[ParsedPath],
    target: &ParsedPath,
    target_is_container: bool,
    recursive: bool,
    encryption: Option<ObjectEncryptionRequest>,
    alias_manager: &AliasManager,
) -> rc_core::Result<Vec<TransferCandidate<CpOperation>>> {
    let mut candidates = Vec::new();
    let mut planning_clients = HashMap::new();
    let multiple_sources = sources.len() > 1;

    for source in sources {
        match source {
            ParsedPath::Local(source) => build_local_candidates(
                source,
                target,
                target_is_container,
                recursive,
                multiple_sources,
                encryption.clone(),
                &mut candidates,
            )?,
            ParsedPath::Remote(source) => {
                build_remote_candidates(
                    source,
                    target,
                    target_is_container,
                    recursive,
                    multiple_sources,
                    encryption.clone(),
                    alias_manager,
                    &mut planning_clients,
                    &mut candidates,
                )
                .await?;
            }
        }
    }

    Ok(candidates)
}

fn validate_plan_targets(plan: &TransferPlan<CpOperation>) -> rc_core::Result<()> {
    let mut targets = HashSet::with_capacity(plan.items.len());
    for candidate in &plan.items {
        if !targets.insert(candidate.target.clone()) {
            return Err(Error::InvalidPath(format!(
                "Multiple sources resolve to the same destination '{}'",
                candidate.target
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_local_candidates(
    source: &Path,
    target: &ParsedPath,
    target_is_container: bool,
    recursive: bool,
    multiple_sources: bool,
    encryption: Option<ObjectEncryptionRequest>,
    candidates: &mut Vec<TransferCandidate<CpOperation>>,
) -> rc_core::Result<()> {
    let ParsedPath::Remote(target) = target else {
        return Err(Error::InvalidPath(
            "Cannot copy between two local paths. Use the system cp command.".to_string(),
        ));
    };
    let metadata = std::fs::metadata(source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(format!("Source not found: {}", source.display()))
        } else {
            Error::Io(error)
        }
    })?;

    if metadata.is_file() {
        let name = local_file_name(source)?;
        let destination = if target_is_container {
            remote_child(target, &name)
        } else {
            target.clone()
        };
        candidates.push(local_transfer_candidate(
            source.to_path_buf(),
            destination,
            name,
            metadata,
            encryption,
        ));
        return Ok(());
    }

    if !recursive {
        return Err(Error::InvalidPath(
            "Source is a directory. Use -r/--recursive to copy directories.".to_string(),
        ));
    }
    if !target_is_container {
        return Err(Error::InvalidPath(
            "Recursive copy requires a directory or remote prefix destination ending in '/'"
                .to_string(),
        ));
    }

    let mut files = Vec::new();
    collect_local_files(source, source, &mut files)?;
    files.sort_by(|left, right| left.1.cmp(&right.1));
    let source_root = multiple_sources
        .then(|| local_file_name(source))
        .transpose()?;
    for (path, relative, metadata) in files {
        let relative = relative.replace('\\', "/");
        let target_relative = source_root
            .as_deref()
            .map_or_else(|| relative.clone(), |root| format!("{root}/{relative}"));
        candidates.push(local_transfer_candidate(
            path,
            remote_child(target, &target_relative),
            relative,
            metadata,
            encryption.clone(),
        ));
    }
    Ok(())
}

fn local_file_name(path: &Path) -> rc_core::Result<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::InvalidPath(format!("Path has no file name: {}", path.display())))
}

fn local_transfer_candidate(
    source: PathBuf,
    target: RemotePath,
    relative_path: String,
    metadata: std::fs::Metadata,
    encryption: Option<ObjectEncryptionRequest>,
) -> TransferCandidate<CpOperation> {
    TransferCandidate {
        payload: CpOperation::LocalToRemote {
            source: source.clone(),
            target: target.clone(),
            encryption,
        },
        source: source.display().to_string(),
        target: target.to_string(),
        relative_path,
        modified: metadata
            .modified()
            .ok()
            .and_then(|time| time.try_into().ok()),
        size_bytes: Some(metadata.len()),
    }
}

fn collect_local_files(
    directory: &Path,
    base: &Path,
    files: &mut Vec<(PathBuf, String, std::fs::Metadata)>,
) -> rc_core::Result<()> {
    let mut entries = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(Error::InvalidPath(format!(
                "Symbolic links are not supported in recursive copy: {}",
                path.display()
            )));
        }
        let metadata = entry.metadata()?;
        if file_type.is_file() {
            let relative = path
                .strip_prefix(base)
                .map_err(|error| Error::InvalidPath(error.to_string()))?
                .to_string_lossy()
                .to_string();
            files.push((path, relative, metadata));
        } else if file_type.is_dir() {
            collect_local_files(&path, base, files)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn build_remote_candidates(
    source: &RemotePath,
    target: &ParsedPath,
    target_is_container: bool,
    recursive: bool,
    multiple_sources: bool,
    encryption: Option<ObjectEncryptionRequest>,
    alias_manager: &AliasManager,
    planning_clients: &mut HashMap<String, Arc<S3Client>>,
    candidates: &mut Vec<TransferCandidate<CpOperation>>,
) -> rc_core::Result<()> {
    let is_prefix = source.key.is_empty() || source.key.ends_with('/') || recursive;
    let client = planning_client(planning_clients, alias_manager, &source.alias).await?;

    if is_prefix {
        if !recursive {
            return Err(Error::InvalidPath(
                "Remote prefix copy requires -r/--recursive".to_string(),
            ));
        }
        let ParsedPath::Local(target_root) = target else {
            return Err(Error::UnsupportedFeature(
                "Recursive S3-to-S3 copy is not implemented".to_string(),
            ));
        };
        if !target_is_container {
            return Err(Error::InvalidPath(
                "Recursive copy requires a directory destination ending in '/'".to_string(),
            ));
        }

        let source_root = if multiple_sources {
            source
                .key
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|value| !value.is_empty())
                .unwrap_or(&source.bucket)
        } else {
            ""
        };
        let mut continuation_token = None;
        loop {
            let result = client
                .list_objects(
                    source,
                    rc_core::ListOptions {
                        recursive: true,
                        max_keys: Some(1_000),
                        continuation_token: continuation_token.clone(),
                        ..Default::default()
                    },
                )
                .await?;
            for object in result.items.into_iter().filter(|object| !object.is_dir) {
                let relative = safe_download_relative_path(&object.key, &source.key)
                    .map_err(Error::InvalidPath)?;
                let relative_string = relative.to_string_lossy().replace('\\', "/");
                let target_relative = if source_root.is_empty() {
                    relative.clone()
                } else {
                    PathBuf::from(source_root).join(&relative)
                };
                let destination = safe_download_destination(target_root, &target_relative)
                    .await
                    .map_err(Error::InvalidPath)?;
                let object_source = RemotePath::new(&source.alias, &source.bucket, &object.key);
                candidates.push(TransferCandidate {
                    payload: CpOperation::RemoteToLocal {
                        source: object_source.clone(),
                        target: destination.clone(),
                    },
                    source: object_source.to_string(),
                    target: destination.display().to_string(),
                    relative_path: relative_string,
                    modified: object.last_modified,
                    size_bytes: object.size_bytes.and_then(|size| u64::try_from(size).ok()),
                });
            }
            if !result.truncated {
                break;
            }
            continuation_token = result.continuation_token;
        }
        return Ok(());
    }

    let object = client.head_object(source).await?;
    let name = source
        .key
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::InvalidPath(format!("Object key is empty: {source}")))?;
    let size_bytes = object.size_bytes.and_then(|size| u64::try_from(size).ok());
    let modified = object.last_modified;

    match target {
        ParsedPath::Local(target) => {
            let destination = if target_is_container {
                let relative = safe_download_relative_path(name, "").map_err(Error::InvalidPath)?;
                safe_download_destination(target, &relative)
                    .await
                    .map_err(Error::InvalidPath)?
            } else {
                target.clone()
            };
            candidates.push(TransferCandidate {
                payload: CpOperation::RemoteToLocal {
                    source: source.clone(),
                    target: destination.clone(),
                },
                source: source.to_string(),
                target: destination.display().to_string(),
                relative_path: name.to_string(),
                modified,
                size_bytes,
            });
        }
        ParsedPath::Remote(target) => {
            if source.alias != target.alias {
                return Err(Error::UnsupportedFeature(
                    "Cross-alias S3-to-S3 copy is not supported".to_string(),
                ));
            }
            let destination = if target_is_container {
                remote_child(target, name)
            } else {
                target.clone()
            };
            candidates.push(TransferCandidate {
                payload: CpOperation::RemoteToRemote {
                    source: source.clone(),
                    target: destination.clone(),
                    encryption,
                },
                source: source.to_string(),
                target: destination.to_string(),
                relative_path: name.to_string(),
                modified,
                size_bytes,
            });
        }
    }
    Ok(())
}

async fn planning_client(
    clients: &mut HashMap<String, Arc<S3Client>>,
    alias_manager: &AliasManager,
    alias_name: &str,
) -> rc_core::Result<Arc<S3Client>> {
    if let Some(client) = clients.get(alias_name) {
        return Ok(Arc::clone(client));
    }
    let alias = alias_manager
        .get(alias_name)
        .map_err(|_| Error::AliasNotFound(alias_name.to_string()))?;
    let client = Arc::new(S3Client::new(alias).await?);
    clients.insert(alias_name.to_string(), Arc::clone(&client));
    Ok(client)
}

fn remote_child(parent: &RemotePath, relative: &str) -> RemotePath {
    let key = if parent.key.is_empty() {
        relative.to_string()
    } else if parent.key.ends_with('/') {
        format!("{}{}", parent.key, relative)
    } else {
        format!("{}/{}", parent.key, relative)
    };
    RemotePath::new(&parent.alias, &parent.bucket, key)
}

fn parse_cp_path(path: &str, alias_manager: Option<&AliasManager>) -> rc_core::Result<ParsedPath> {
    let parsed = parse_path(path)?;

    let ParsedPath::Remote(remote) = &parsed else {
        return Ok(parsed);
    };

    if let Some(manager) = alias_manager
        && matches!(manager.exists(&remote.alias), Ok(true))
    {
        return Ok(parsed);
    }

    if Path::new(path).exists() {
        return Ok(ParsedPath::Local(PathBuf::from(path)));
    }

    Ok(parsed)
}

async fn copy_local_to_s3_prepared(
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
) -> ExitCode {
    // Check if source exists
    if !src.exists() {
        return formatter.fail_with_suggestion(
            ExitCode::NotFound,
            &format!("Source not found: {}", src.display()),
            "Check the local source path and retry the copy command.",
        );
    }

    // If source is a directory, require recursive flag
    if src.is_dir() && !args.recursive {
        return formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Source is a directory. Use -r/--recursive to copy directories.",
            "Retry with -r or --recursive to copy a directory tree.",
        );
    }

    // Load alias and create client
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&dst.alias) {
        Ok(a) => a,
        Err(_) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", dst.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };

    if src.is_file() {
        // Single file upload
        upload_file(&client, src, dst, args, formatter, encryption).await
    } else {
        // Directory upload
        upload_directory(&client, src, dst, args, formatter, encryption).await
    }
}

/// Multipart upload threshold: files larger than this size use multipart upload.
const MULTIPART_THRESHOLD: u64 = rc_s3::multipart::DEFAULT_PART_SIZE;
/// Download progress threshold: avoid flicker for tiny downloads while surfacing meaningful waits.
const DOWNLOAD_PROGRESS_THRESHOLD: u64 = 4 * 1024 * 1024;

fn update_download_progress(
    progress: &mut Option<ProgressBar>,
    output_config: &OutputConfig,
    bytes_downloaded: u64,
    total_size: Option<u64>,
) {
    let Some(total_size) = total_size else {
        return;
    };

    if total_size < DOWNLOAD_PROGRESS_THRESHOLD {
        return;
    }

    let progress_bar =
        progress.get_or_insert_with(|| ProgressBar::new(output_config.clone(), total_size));
    progress_bar.set_position(bytes_downloaded);
}

fn print_upload_success(
    formatter: &Formatter,
    info: &rc_core::ObjectInfo,
    src_display: &str,
    dst_display: &str,
) {
    if formatter.is_json() {
        let output = CpOutput {
            status: "success",
            source: src_display.to_string(),
            target: dst_display.to_string(),
            size_bytes: info.size_bytes,
            size_human: info.size_human.clone(),
        };
        formatter.json(&output);
    } else {
        let styled_src = formatter.style_file(src_display);
        let styled_dst = formatter.style_file(dst_display);
        let styled_size = formatter.style_size(&info.size_human.clone().unwrap_or_default());
        formatter.println(&format!("{styled_src} -> {styled_dst} ({styled_size})"));
    }
}

async fn upload_file(
    client: &S3Client,
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
) -> ExitCode {
    // Determine destination key
    let dst_key = if dst.key.is_empty() || dst.key.ends_with('/') {
        // If destination is a directory, use source filename
        let filename = src.file_name().unwrap_or_default().to_string_lossy();
        format!("{}{}", dst.key, filename)
    } else {
        dst.key.clone()
    };

    let target = RemotePath::new(&dst.alias, &dst.bucket, &dst_key);
    let src_display = src.display().to_string();
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst_key);

    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!("Would copy: {styled_src} -> {styled_dst}"));
        return ExitCode::Success;
    }

    // Get file size for progress bar decision
    let file_size = match std::fs::metadata(src) {
        Ok(m) => m.len(),
        Err(e) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read {src_display}: {e}"),
            );
        }
    };

    // Determine content type
    let guessed_type: Option<String> = mime_guess::from_path(src)
        .first()
        .map(|m| m.essence_str().to_string());
    let content_type = select_upload_content_type(
        args.content_type.as_deref(),
        guessed_type.as_deref(),
        file_size,
    );

    // Show progress bar for large files
    let progress = if file_size > MULTIPART_THRESHOLD {
        tracing::debug!(
            file_size,
            threshold = MULTIPART_THRESHOLD,
            "Using multipart upload for large file"
        );
        Some(ProgressBar::new(formatter.output_config(), file_size))
    } else {
        tracing::debug!(file_size, "Using single put_object for small file");
        None
    };

    // Upload
    match client
        .put_object_from_path(&target, src, content_type, encryption, |bytes_sent| {
            if let Some(ref pb) = progress {
                pb.set_position(bytes_sent);
            }
        })
        .await
    {
        Ok(info) => {
            if let Some(ref pb) = progress {
                pb.finish_and_clear();
            }
            print_upload_success(formatter, &info, &src_display, &dst_display);
            ExitCode::Success
        }
        Err(e) => {
            if let Some(ref pb) = progress {
                pb.finish_and_clear();
            }
            formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to upload {src_display}: {e}"),
            )
        }
    }
}

fn select_upload_content_type<'a>(
    explicit_type: Option<&'a str>,
    guessed_type: Option<&'a str>,
    file_size: u64,
) -> Option<&'a str> {
    if file_size > MULTIPART_THRESHOLD {
        explicit_type
    } else {
        explicit_type.or(guessed_type)
    }
}

async fn upload_directory(
    client: &S3Client,
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
) -> ExitCode {
    use std::fs;

    let mut success_count = 0;
    let mut error_count = 0;

    // Walk directory
    fn walk_dir(dir: &Path, base: &Path) -> std::io::Result<Vec<(std::path::PathBuf, String)>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let relative = path.strip_prefix(base).unwrap_or(&path);
                let relative_str = relative.to_string_lossy().to_string();
                files.push((path, relative_str));
            } else if path.is_dir() {
                files.extend(walk_dir(&path, base)?);
            }
        }
        Ok(files)
    }

    let files = match walk_dir(src, src) {
        Ok(f) => f,
        Err(e) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read directory: {e}"),
            );
        }
    };

    for (file_path, relative_path) in files {
        // Build destination key
        let dst_key = if dst.key.is_empty() {
            relative_path.replace('\\', "/")
        } else if dst.key.ends_with('/') {
            format!("{}{}", dst.key, relative_path.replace('\\', "/"))
        } else {
            format!("{}/{}", dst.key, relative_path.replace('\\', "/"))
        };

        let target = RemotePath::new(&dst.alias, &dst.bucket, &dst_key);

        let result = upload_file(client, &file_path, &target, args, formatter, encryption).await;

        if result == ExitCode::Success {
            success_count += 1;
        } else {
            error_count += 1;
            if !args.continue_on_error {
                return result;
            }
        }
    }

    if error_count > 0 {
        formatter.warning(&format!(
            "Completed with errors: {success_count} succeeded, {error_count} failed"
        ));
        ExitCode::GeneralError
    } else {
        if !formatter.is_json() {
            formatter.success(&format!("Uploaded {success_count} file(s)."));
        }
        ExitCode::Success
    }
}

async fn copy_s3_to_local(
    src: &RemotePath,
    dst: &Path,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    // Load alias and create client
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&src.alias) {
        Ok(a) => a,
        Err(_) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", src.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };

    // Check if source is a prefix (directory-like)
    let is_prefix = src.key.is_empty() || src.key.ends_with('/');

    if is_prefix || args.recursive {
        // Download multiple objects
        download_prefix(&client, src, dst, args, formatter).await
    } else {
        // Download single object
        download_file(&client, src, dst, args, formatter).await
    }
}

pub(super) async fn download_file(
    client: &S3Client,
    src: &RemotePath,
    dst: &Path,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);

    // Determine destination path
    let dst_path = if dst.is_dir() || dst.to_string_lossy().ends_with('/') {
        let filename = src.key.rsplit('/').next().unwrap_or(&src.key);
        let filename = match safe_download_relative_path(filename, "") {
            Ok(filename) => filename,
            Err(error) => {
                return formatter.fail(
                    ExitCode::UsageError,
                    &format!("Unsafe object key '{}': {error}", src.key),
                );
            }
        };
        dst.join(filename)
    } else {
        dst.to_path_buf()
    };

    let dst_display = dst_path.display().to_string();

    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!("Would copy: {styled_src} -> {styled_dst}"));
        return ExitCode::Success;
    }

    // Check if destination exists
    if dst_path.exists() && !args.overwrite {
        return formatter.fail_with_suggestion(
            ExitCode::Conflict,
            &format!("Destination exists: {dst_display}. Use --overwrite to replace."),
            "Retry with --overwrite if replacing the destination file is intended.",
        );
    }

    // Create parent directories
    if let Some(parent) = dst_path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to create directory: {e}"),
        );
    }

    let output_config = formatter.output_config();
    let mut progress = None;

    // Download object
    let result = client
        .download_object_to_path(src, &dst_path, |bytes_downloaded, total_size| {
            update_download_progress(&mut progress, &output_config, bytes_downloaded, total_size);
        })
        .await;

    if let Some(ref pb) = progress {
        pb.finish_and_clear();
    }

    match result {
        Ok(size) => {
            let size = size as i64;

            if formatter.is_json() {
                let output = CpOutput {
                    status: "success",
                    source: src_display,
                    target: dst_display,
                    size_bytes: Some(size),
                    size_human: Some(humansize::format_size(size as u64, humansize::BINARY)),
                };
                formatter.json(&output);
            } else {
                let styled_src = formatter.style_file(&src_display);
                let styled_dst = formatter.style_file(&dst_display);
                let styled_size =
                    formatter.style_size(&humansize::format_size(size as u64, humansize::BINARY));
                formatter.println(&format!("{styled_src} -> {styled_dst} ({styled_size})"));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                formatter.fail_with_suggestion(
                    ExitCode::NotFound,
                    &format!("Object not found: {src_display}"),
                    "Check the object key and bucket path, then retry the copy command.",
                )
            } else {
                formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to download {src_display}: {e}"),
                )
            }
        }
    }
}

async fn download_prefix(
    client: &S3Client,
    src: &RemotePath,
    dst: &Path,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    use rc_core::ListOptions;

    let mut success_count = 0;
    let mut error_count = 0;
    let mut continuation_token: Option<String> = None;

    loop {
        let options = ListOptions {
            recursive: true,
            max_keys: Some(1000),
            continuation_token: continuation_token.clone(),
            ..Default::default()
        };

        match client.list_objects(src, options).await {
            Ok(result) => {
                for item in result.items {
                    if item.is_dir {
                        continue;
                    }

                    // Calculate relative path from prefix
                    let relative_path = match safe_download_relative_path(&item.key, &src.key) {
                        Ok(path) => path,
                        Err(error) => {
                            error_count += 1;
                            formatter.error(&format!(
                                "Refusing unsafe object key '{}': {error}",
                                item.key
                            ));
                            if !args.continue_on_error {
                                return ExitCode::UsageError;
                            }
                            continue;
                        }
                    };
                    let dst_path = match safe_download_destination(dst, &relative_path).await {
                        Ok(path) => path,
                        Err(error) => {
                            error_count += 1;
                            formatter.error(&format!(
                                "Refusing unsafe destination for '{}': {error}",
                                item.key
                            ));
                            if !args.continue_on_error {
                                return ExitCode::UsageError;
                            }
                            continue;
                        }
                    };

                    let obj_src = RemotePath::new(&src.alias, &src.bucket, &item.key);
                    let result = download_file(client, &obj_src, &dst_path, args, formatter).await;

                    if result == ExitCode::Success {
                        success_count += 1;
                    } else {
                        error_count += 1;
                        if !args.continue_on_error {
                            return result;
                        }
                    }
                }

                if result.truncated {
                    continuation_token = result.continuation_token;
                } else {
                    break;
                }
            }
            Err(e) => {
                return formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to list objects: {e}"),
                );
            }
        }
    }

    if error_count > 0 {
        formatter.warning(&format!(
            "Completed with errors: {success_count} succeeded, {error_count} failed"
        ));
        ExitCode::GeneralError
    } else if success_count == 0 {
        formatter.warning("No objects found to download.");
        ExitCode::Success
    } else {
        if !formatter.is_json() {
            formatter.success(&format!("Downloaded {success_count} file(s)."));
        }
        ExitCode::Success
    }
}

pub(super) fn safe_download_relative_path(key: &str, prefix: &str) -> Result<PathBuf, String> {
    let relative = key
        .strip_prefix(prefix)
        .ok_or_else(|| format!("key is outside requested prefix '{prefix}'"))?
        .trim_start_matches('/');

    let mut path = PathBuf::new();
    for component in relative.split(['/', '\\']) {
        if component.is_empty() {
            continue;
        }
        if matches!(component, "." | "..") {
            return Err("path traversal components are not allowed".to_string());
        }
        if component.contains(':') {
            return Err("colon characters are not allowed in download paths".to_string());
        }
        if component.ends_with(['.', ' ']) {
            return Err("download path components must not end in a dot or space".to_string());
        }
        let stem = component.split('.').next().unwrap_or_default();
        let stem = stem.to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err("reserved Windows device names are not allowed".to_string());
        }
        path.push(component);
    }

    if path.as_os_str().is_empty() {
        return Err("object key does not contain a file path".to_string());
    }

    Ok(path)
}

pub(super) async fn safe_download_destination(
    root: &Path,
    relative: &Path,
) -> Result<PathBuf, String> {
    let mut destination = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("destination path contains a non-normal component".to_string());
        };
        destination.push(component);
        match tokio::fs::symlink_metadata(&destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "destination component '{}' is a symbolic link",
                    destination.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect destination '{}': {error}",
                    destination.display()
                ));
            }
        }
    }

    Ok(destination)
}

async fn copy_s3_to_s3_prepared(
    src: &RemotePath,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
) -> ExitCode {
    // For S3-to-S3, we need to handle same or different aliases
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    // For now, only support same-alias copies (server-side copy)
    if src.alias != dst.alias {
        return formatter.fail_with_suggestion(
            ExitCode::UnsupportedFeature,
            "Cross-alias S3-to-S3 copy not yet supported. Use download + upload.",
            "Copy via a local path or split the operation into download and upload steps.",
        );
    }

    let alias = match alias_manager.get(&src.alias) {
        Ok(a) => a,
        Err(_) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", src.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };

    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst.key);

    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!("Would copy: {styled_src} -> {styled_dst}"));
        return ExitCode::Success;
    }

    match client.head_object(src).await {
        Ok(info)
            if info
                .size_bytes
                .is_some_and(|size| size > 5 * 1024 * 1024 * 1024) =>
        {
            return formatter.fail(
                ExitCode::UnsupportedFeature,
                "Objects larger than 5 GiB require multipart copy, which is not implemented",
            );
        }
        Ok(_) => {}
        Err(rc_core::Error::NotFound(_)) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Source not found: {src_display}"),
                "Check the source bucket and object key, then retry the copy command.",
            );
        }
        Err(error) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to inspect source object: {error}"),
            );
        }
    }

    match client.copy_object(src, dst, encryption).await {
        Ok(info) => {
            if formatter.is_json() {
                let output = CpOutput {
                    status: "success",
                    source: src_display,
                    target: dst_display,
                    size_bytes: info.size_bytes,
                    size_human: info.size_human,
                };
                formatter.json(&output);
            } else {
                let styled_src = formatter.style_file(&src_display);
                let styled_dst = formatter.style_file(&dst_display);
                let styled_size = formatter.style_size(&info.size_human.unwrap_or_default());
                formatter.println(&format!("{styled_src} -> {styled_dst} ({styled_size})"));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                formatter.fail_with_suggestion(
                    ExitCode::NotFound,
                    &format!("Source not found: {src_display}"),
                    "Check the source bucket and object key, then retry the copy command.",
                )
            } else {
                formatter.fail(ExitCode::NetworkError, &format!("Failed to copy: {e}"))
            }
        }
    }
}

fn parse_kms_target(value: &str) -> Result<(String, String), String> {
    let (target, key_id) = value
        .split_once('=')
        .ok_or_else(|| "Expected TARGET=KMS_KEY_ID for --enc-kms".to_string())?;

    if target.is_empty() || key_id.is_empty() {
        return Err("Expected TARGET=KMS_KEY_ID for --enc-kms".to_string());
    }

    Ok((target.to_string(), key_id.to_string()))
}

pub(crate) fn parse_destination_encryption(
    enc_s3: &[String],
    enc_kms: &[String],
    target: &ParsedPath,
) -> Result<Option<ObjectEncryptionRequest>, String> {
    if enc_s3.is_empty() && enc_kms.is_empty() {
        return Ok(None);
    }

    let remote = match target {
        ParsedPath::Remote(remote) => remote,
        ParsedPath::Local(_) => {
            return Err("Destination encryption flags must reference a remote destination".into());
        }
    };

    let target_display = remote.to_string();
    let s3_matches = enc_s3.iter().any(|value| value == &target_display);
    let kms_targets = enc_kms
        .iter()
        .map(|value| parse_kms_target(value))
        .collect::<Result<Vec<_>, _>>()?;
    let kms_match = kms_targets
        .iter()
        .find(|(candidate, _)| candidate == &target_display);

    if !enc_s3.is_empty() && !s3_matches {
        return Err(format!(
            "--enc-s3 target must exactly match the remote destination: {target_display}"
        ));
    }

    if !enc_kms.is_empty() && kms_match.is_none() {
        return Err(format!(
            "--enc-kms target must exactly match the remote destination: {target_display}"
        ));
    }

    match (s3_matches, kms_match) {
        (true, Some(_)) => Err(format!(
            "--enc-s3 and --enc-kms cannot target the same destination: {target_display}"
        )),
        (true, None) => Ok(Some(ObjectEncryptionRequest::SseS3)),
        (false, Some((_, key_id))) => Ok(Some(ObjectEncryptionRequest::SseKms {
            key_id: key_id.clone(),
        })),
        (false, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::{Alias, ConfigManager};
    use tempfile::TempDir;

    fn temp_alias_manager() -> (AliasManager, TempDir) {
        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let config_manager = ConfigManager::with_path(config_path);
        let alias_manager = AliasManager::with_config_manager(config_manager);
        (alias_manager, temp_dir)
    }

    #[test]
    fn test_parse_local_path() {
        let result = parse_path("./file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_remote_path() {
        let result = parse_path("myalias/bucket/file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_parse_local_absolute_path() {
        // Use platform-appropriate absolute path
        #[cfg(unix)]
        let path = "/home/user/file.txt";
        #[cfg(windows)]
        let path = "C:\\Users\\user\\file.txt";

        let result = parse_path(path).unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
        if let ParsedPath::Local(p) = result {
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn test_parse_local_relative_path() {
        let result = parse_path("../file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_remote_path_bucket_only() {
        let result = parse_path("myalias/bucket/").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
        if let ParsedPath::Remote(r) = result {
            assert_eq!(r.alias, "myalias");
            assert_eq!(r.bucket, "bucket");
            assert!(r.key.is_empty());
        }
    }

    #[test]
    fn test_parse_remote_path_with_deep_key() {
        let result = parse_path("myalias/bucket/dir1/dir2/file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
        if let ParsedPath::Remote(r) = result {
            assert_eq!(r.alias, "myalias");
            assert_eq!(r.bucket, "bucket");
            assert_eq!(r.key, "dir1/dir2/file.txt");
        }
    }

    #[test]
    fn test_download_progress_created_for_large_transfer() {
        let output_config = OutputConfig::default();
        let mut progress = None;

        update_download_progress(
            &mut progress,
            &output_config,
            1024,
            Some(DOWNLOAD_PROGRESS_THRESHOLD),
        );

        let progress = progress.expect("large download should create progress bar");
        assert!(progress.is_visible());
        progress.finish_and_clear();
    }

    #[test]
    fn test_download_progress_skips_small_transfer() {
        let output_config = OutputConfig::default();
        let mut progress = None;

        update_download_progress(
            &mut progress,
            &output_config,
            1024,
            Some(DOWNLOAD_PROGRESS_THRESHOLD - 1),
        );

        assert!(progress.is_none());
    }

    #[test]
    fn test_download_progress_skips_unknown_total_size() {
        let output_config = OutputConfig::default();
        let mut progress = None;

        update_download_progress(&mut progress, &output_config, 1024, None);

        assert!(progress.is_none());
    }

    #[test]
    fn test_download_progress_respects_no_progress_config() {
        let output_config = OutputConfig {
            no_progress: true,
            ..Default::default()
        };
        let mut progress = None;

        update_download_progress(
            &mut progress,
            &output_config,
            1024,
            Some(DOWNLOAD_PROGRESS_THRESHOLD),
        );

        let progress = progress.expect("large download should create progress state");
        assert!(!progress.is_visible());
    }

    #[test]
    fn download_relative_path_preserves_safe_nested_keys() {
        let relative = safe_download_relative_path("reports/2026/july/data.csv", "reports/")
            .expect("safe key should resolve");

        assert_eq!(
            relative,
            PathBuf::from("2026").join("july").join("data.csv")
        );
    }

    #[test]
    fn download_relative_path_rejects_traversal_and_absolute_keys() {
        for key in [
            "reports/../../escaped",
            "reports/..\\..\\escaped",
            "reports/C:/escaped",
            "reports/safe:stream",
            "reports/CON.txt",
            "reports/trailing.",
            "/absolute/path",
        ] {
            assert!(
                safe_download_relative_path(key, "reports/").is_err(),
                "unsafe key should be rejected: {key}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_destination_rejects_existing_symlink_components() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create destination root");
        let outside = tempfile::tempdir().expect("create outside directory");
        symlink(outside.path(), root.path().join("linked")).expect("create test symlink");

        let result =
            safe_download_destination(root.path(), &PathBuf::from("linked/file.txt")).await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_destination_rejects_existing_symlink_file() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create destination root");
        let outside = tempfile::tempdir().expect("create outside directory");
        let outside_file = outside.path().join("file.txt");
        std::fs::write(&outside_file, b"outside").expect("write outside file");
        symlink(&outside_file, root.path().join("file.txt")).expect("create test symlink");

        let result = safe_download_destination(root.path(), &PathBuf::from("file.txt")).await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn recursive_source_collection_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create source root");
        let outside = tempfile::NamedTempFile::new().expect("create outside file");
        symlink(outside.path(), root.path().join("linked.txt")).expect("create source symlink");
        let mut files = Vec::new();

        let result = collect_local_files(root.path(), root.path(), &mut files);

        assert!(result.is_err());
        assert!(files.is_empty());
    }

    #[test]
    fn test_select_upload_content_type_uses_guess_for_small_files() {
        let selected =
            select_upload_content_type(None, Some("text/plain"), MULTIPART_THRESHOLD - 1);

        assert_eq!(selected, Some("text/plain"));
    }

    #[test]
    fn test_select_upload_content_type_skips_guess_for_multipart_files() {
        let selected =
            select_upload_content_type(None, Some("text/plain"), MULTIPART_THRESHOLD + 1);

        assert_eq!(selected, None);
    }

    #[test]
    fn test_select_upload_content_type_uses_guess_at_multipart_boundary() {
        let selected = select_upload_content_type(None, Some("text/plain"), MULTIPART_THRESHOLD);

        assert_eq!(selected, Some("text/plain"));
    }

    #[test]
    fn test_select_upload_content_type_keeps_explicit_type_for_multipart_files() {
        let selected = select_upload_content_type(
            Some("application/octet-stream"),
            Some("text/plain"),
            MULTIPART_THRESHOLD + 1,
        );

        assert_eq!(selected, Some("application/octet-stream"));
    }

    #[test]
    fn test_parse_cp_path_prefers_existing_local_path_when_alias_missing() {
        let (alias_manager, temp_dir) = temp_alias_manager();
        let full = temp_dir.path().join("issue-2094-local").join("file.txt");
        let full_str = full.to_string_lossy().to_string();

        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&full, b"test").expect("write local file");

        let parsed = parse_cp_path(&full_str, Some(&alias_manager)).expect("parse path");
        assert!(matches!(parsed, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_cp_path_keeps_remote_when_alias_exists() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        alias_manager
            .set(Alias::new("target", "http://localhost:9000", "a", "b"))
            .expect("set alias");

        let parsed = parse_cp_path("target/bucket/file.txt", Some(&alias_manager))
            .expect("parse remote path");
        assert!(matches!(parsed, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_parse_cp_path_keeps_remote_when_local_missing() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        let parsed = parse_cp_path("missing/bucket/file.txt", Some(&alias_manager))
            .expect("parse remote path");
        assert!(matches!(parsed, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_cp_args_defaults() {
        let args = CpArgs::single("src", "dst");
        assert!(args.overwrite);
        assert!(!args.recursive);
        assert!(!args.dry_run);
        assert_eq!(args.concurrency, None);
        assert_eq!(args.retry_attempts, None);
        let controls = build_transfer_controls(&args).expect("default controls");
        assert_eq!(controls.concurrency, DEFAULT_TRANSFER_CONCURRENCY);
        assert_eq!(controls.retry.max_attempts, DEFAULT_RETRY_ATTEMPTS);
        assert_eq!(
            controls.retry.initial_backoff_ms,
            DEFAULT_RETRY_INITIAL_BACKOFF_MS
        );
        assert_eq!(controls.retry.max_backoff_ms, DEFAULT_RETRY_MAX_BACKOFF_MS);
    }

    #[test]
    fn explicit_default_transfer_controls_enable_planner_routing() {
        let defaults = CpArgs::single("src", "dst");
        assert!(!uses_transfer_planner(&defaults));

        let mut explicit_concurrency = defaults.clone();
        explicit_concurrency.concurrency = Some(DEFAULT_TRANSFER_CONCURRENCY);
        let mut explicit_attempts = defaults.clone();
        explicit_attempts.retry_attempts = Some(DEFAULT_RETRY_ATTEMPTS);
        let mut explicit_initial_backoff = defaults.clone();
        explicit_initial_backoff.retry_initial_backoff_ms = Some(DEFAULT_RETRY_INITIAL_BACKOFF_MS);
        let mut explicit_max_backoff = defaults;
        explicit_max_backoff.retry_max_backoff_ms = Some(DEFAULT_RETRY_MAX_BACKOFF_MS);

        for args in [
            explicit_concurrency,
            explicit_attempts,
            explicit_initial_backoff,
            explicit_max_backoff,
        ] {
            assert!(uses_transfer_planner(&args));
        }
    }

    #[test]
    fn planned_client_aliases_are_deduplicated() {
        let first = TransferCandidate {
            payload: CpOperation::LocalToRemote {
                source: PathBuf::from("first.txt"),
                target: RemotePath::new("shared", "bucket", "first.txt"),
                encryption: None,
            },
            source: "first.txt".to_string(),
            target: "shared/bucket/first.txt".to_string(),
            relative_path: "first.txt".to_string(),
            modified: None,
            size_bytes: Some(1),
        };
        let second = TransferCandidate {
            payload: CpOperation::RemoteToLocal {
                source: RemotePath::new("shared", "bucket", "second.txt"),
                target: PathBuf::from("second.txt"),
            },
            source: "shared/bucket/second.txt".to_string(),
            target: "second.txt".to_string(),
            relative_path: "second.txt".to_string(),
            modified: None,
            size_bytes: Some(2),
        };

        assert_eq!(
            planned_client_aliases(&[first, second])
                .into_iter()
                .collect::<Vec<_>>(),
            ["shared"]
        );
    }

    #[tokio::test]
    async fn planning_client_reuses_one_connection_pool_per_alias() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        alias_manager
            .set(Alias::new(
                "shared",
                "http://localhost:9000",
                "access",
                "secret",
            ))
            .expect("set alias");
        let mut clients = HashMap::new();

        let first = planning_client(&mut clients, &alias_manager, "shared")
            .await
            .expect("create first client");
        let second = planning_client(&mut clients, &alias_manager, "shared")
            .await
            .expect("reuse first client");

        assert_eq!(clients.len(), 1);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn planned_remote_copy_uses_candidate_size_for_multipart_guard() {
        let candidate = TransferCandidate {
            payload: CpOperation::RemoteToRemote {
                source: RemotePath::new("shared", "source", "large.bin"),
                target: RemotePath::new("shared", "target", "large.bin"),
                encryption: None,
            },
            source: "shared/source/large.bin".to_string(),
            target: "shared/target/large.bin".to_string(),
            relative_path: "large.bin".to_string(),
            modified: None,
            size_bytes: Some(MAX_SINGLE_COPY_SIZE + 1),
        };

        assert!(requires_multipart_copy(candidate.size_bytes));
    }

    #[test]
    fn transfer_age_and_rewind_use_utc_cutoffs() {
        let now: Timestamp = "2026-07-21T12:00:00Z".parse().expect("valid UTC timestamp");

        assert_eq!(
            parse_age_cutoff("1h", now).expect("valid age").to_string(),
            "2026-07-21T11:00:00Z"
        );
        assert_eq!(
            parse_rewind_cutoff("2026-07-20T08:30:00+08:00", now)
                .expect("valid offset timestamp")
                .to_string(),
            "2026-07-20T00:30:00Z"
        );
    }

    #[test]
    fn transfer_rate_parser_supports_decimal_and_binary_units() {
        assert_eq!(parse_byte_rate("10MB/s").expect("decimal rate"), 10_000_000);
        assert_eq!(parse_byte_rate("10MiB/s").expect("binary rate"), 10_485_760);
        assert!(parse_byte_rate("0").is_err());
        assert!(parse_byte_rate("10widgets/s").is_err());
    }

    #[tokio::test]
    async fn planner_rejects_two_sources_that_resolve_to_one_target() {
        let (alias_manager, temp_dir) = temp_alias_manager();
        let first_dir = temp_dir.path().join("first");
        let second_dir = temp_dir.path().join("second");
        std::fs::create_dir_all(&first_dir).expect("create first source dir");
        std::fs::create_dir_all(&second_dir).expect("create second source dir");
        let first = first_dir.join("same.txt");
        let second = second_dir.join("same.txt");
        std::fs::write(&first, b"first").expect("write first source");
        std::fs::write(&second, b"second").expect("write second source");

        let candidates = build_transfer_candidates(
            &[ParsedPath::Local(first), ParsedPath::Local(second)],
            &ParsedPath::Remote(RemotePath::new("target", "bucket", "prefix/")),
            true,
            false,
            None,
            &alias_manager,
        )
        .await
        .expect("sources can be expanded before selection");
        let plan = TransferPlan::build(candidates, &TransferSelection::default());
        let error = validate_plan_targets(&plan).expect_err("colliding destinations must fail");

        assert!(error.to_string().contains("same destination"));
    }

    #[test]
    fn parse_enc_kms_target_requires_equals_separator() {
        let error = parse_kms_target("local/bucket/file.txt").expect_err("missing key separator");
        assert!(error.contains("Expected TARGET=KMS_KEY_ID"));
    }

    #[test]
    fn destination_encryption_rejects_local_targets() {
        let error = parse_destination_encryption(
            &[String::from("./local.txt")],
            &[],
            &ParsedPath::Local(std::path::PathBuf::from("./local.txt")),
        )
        .expect_err("local target should be rejected");

        assert!(error.contains("must reference a remote destination"));
    }

    #[test]
    fn destination_encryption_detects_conflicting_flags_for_same_target() {
        let target = ParsedPath::Remote(RemotePath::new("local", "bucket", "file.txt"));
        let error = parse_destination_encryption(
            &[String::from("local/bucket/file.txt")],
            &[String::from("local/bucket/file.txt=kms-key")],
            &target,
        )
        .expect_err("same target conflict should fail");

        assert!(error.contains("cannot target the same destination"));
    }

    #[test]
    fn destination_encryption_rejects_unmatched_s3_target() {
        let target = ParsedPath::Remote(RemotePath::new("local", "bucket", "file.txt"));
        let error =
            parse_destination_encryption(&[String::from("local/bucket/typo.txt")], &[], &target)
                .expect_err("unmatched s3 target should fail");

        assert!(error.contains("must exactly match the remote destination"));
    }

    #[test]
    fn destination_encryption_rejects_unmatched_kms_target() {
        let target = ParsedPath::Remote(RemotePath::new("local", "bucket", "file.txt"));
        let error = parse_destination_encryption(
            &[],
            &[String::from("local/bucket/typo.txt=kms-key")],
            &target,
        )
        .expect_err("unmatched kms target should fail");

        assert!(error.contains("must exactly match the remote destination"));
    }

    #[test]
    fn test_cp_output_serialization() {
        let output = CpOutput {
            status: "success",
            source: "src/file.txt".to_string(),
            target: "dst/file.txt".to_string(),
            size_bytes: Some(1024),
            size_human: Some("1 KiB".to_string()),
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"size_bytes\":1024"));
    }

    #[test]
    fn test_cp_output_skips_none_fields() {
        let output = CpOutput {
            status: "success",
            source: "src".to_string(),
            target: "dst".to_string(),
            size_bytes: None,
            size_human: None,
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(!json.contains("size_bytes"));
        assert!(!json.contains("size_human"));
    }
}
