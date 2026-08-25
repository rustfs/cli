//! cp command - Copy objects
//!
//! Copies objects between local filesystem and S3, or between S3 locations.

use clap::Args;
use jiff::Timestamp;
use rc_core::alias::RetryConfig;
use rc_core::{
    AliasManager, Error, MetadataDirective, MultipartCopyCancellation, MultipartCopyOptions,
    ObjectAttributes, ObjectEncryptionRequest, ObjectInfo, ObjectKeyPolicy, ObjectStore as _,
    ObjectWriteOptions, ParsedPath, RemotePath, SseCustomerKey, TransferCancellation,
    TransferCandidate, TransferControls, TransferCopyOptions, TransferExecutor,
    TransferOutcomeState, TransferPlan, TransferReadOptions, TransferSelection,
    normalize_relative_key, parse_path, relative_local_path_from_key,
};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as AsyncMutex;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig, ProgressBar, V3SuccessEnvelope};
use crate::secret_input::{SecretLocator, resolve_secret_locator};

use super::transfer_fidelity::{MetadataDirectiveArg, TaggingDirectiveArg, TransferFidelityArgs};

const CP_AFTER_HELP: &str = "\
Examples:
  rc object copy ./report.json local/my-bucket/reports/
  rc cp ./report.json local/my-bucket/reports/
  rc object copy local/source-bucket/archive.tar.gz ./downloads/archive.tar.gz";

pub(crate) const GET_AFTER_HELP: &str = "\
Examples:
  rc get local/my-bucket/report.json ./report.json
  rc get local/my-bucket/archive.tar.gz ./downloads/archive.tar.gz";

pub(crate) const PUT_AFTER_HELP: &str = "\
Examples:
  rc put ./report.json local/my-bucket/reports/
  rc put ./january.csv ./february.csv local/my-bucket/reports/";

const REMOTE_PATH_SUGGESTION: &str =
    "Use a local filesystem path or a remote path in the form alias/bucket[/key].";
const DEFAULT_TRANSFER_CONCURRENCY: usize = 4;
const DEFAULT_RETRY_ATTEMPTS: u32 = 3;
const DEFAULT_RETRY_INITIAL_BACKOFF_MS: u64 = 100;
const DEFAULT_RETRY_MAX_BACKOFF_MS: u64 = 10_000;
#[cfg(test)]
const MAX_SINGLE_COPY_SIZE: u64 = rc_core::S3_SINGLE_COPY_MAX_SIZE;

/// Copy objects
#[derive(Args, Clone)]
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
    #[arg(short, long, conflicts_with = "metadata_directive")]
    pub preserve: bool,

    /// Source metadata handling for remote copies
    #[arg(long, value_enum)]
    pub(crate) metadata_directive: Option<MetadataDirectiveArg>,

    /// Source tag handling for remote copies
    #[arg(long, value_enum)]
    pub(crate) tagging_directive: Option<TaggingDirectiveArg>,

    /// Continue on errors
    #[arg(long)]
    pub continue_on_error: bool,

    /// Overwrite destination if it exists
    #[arg(
        long,
        default_value_t = true,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub overwrite: bool,

    /// Skip remote destinations that already exist
    #[arg(long)]
    pub skip_existing: bool,

    /// Only show what would be copied (dry run)
    #[arg(long)]
    pub dry_run: bool,

    /// Storage class for destination (S3 only)
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Content type for uploaded files
    #[arg(long)]
    pub content_type: Option<String>,

    #[command(flatten)]
    pub(crate) fidelity: TransferFidelityArgs,

    /// Apply SSE-S3 to the remote destination path
    #[arg(long = "enc-s3")]
    pub enc_s3: Vec<String>,

    /// Apply SSE-KMS to the remote destination path as TARGET=KMS_KEY_ID
    #[arg(long = "enc-kms")]
    pub enc_kms: Vec<String>,

    /// Read a 32-byte SSE-C source key from a protected file
    #[arg(long = "enc-c-source-key-file")]
    pub enc_c_source_key_file: Option<PathBuf>,

    /// Read a 32-byte SSE-C source key from the named environment variable
    #[arg(long = "enc-c-source-key-env")]
    pub enc_c_source_key_env: Option<String>,

    /// Read a 32-byte SSE-C destination key from a protected file
    #[arg(long = "enc-c-destination-key-file")]
    pub enc_c_destination_key_file: Option<PathBuf>,

    /// Read a 32-byte SSE-C destination key from the named environment variable
    #[arg(long = "enc-c-destination-key-env")]
    pub enc_c_destination_key_env: Option<String>,

    #[arg(skip)]
    pub(crate) source_customer_key: Option<SseCustomerKey>,

    #[arg(skip)]
    pub(crate) destination_customer_key: Option<SseCustomerKey>,

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

    /// Reject object keys that cannot be created on Windows filesystems
    #[arg(long)]
    pub portable_names: bool,
}

impl fmt::Debug for CpArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CpArgs { .. }")
    }
}

impl CpArgs {
    pub(crate) fn single(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            sources: vec![source.into()],
            target: target.into(),
            recursive: false,
            preserve: false,
            metadata_directive: None,
            tagging_directive: None,
            continue_on_error: false,
            overwrite: true,
            skip_existing: false,
            dry_run: false,
            storage_class: None,
            content_type: None,
            fidelity: TransferFidelityArgs::default(),
            enc_s3: Vec::new(),
            enc_kms: Vec::new(),
            enc_c_source_key_file: None,
            enc_c_source_key_env: None,
            enc_c_destination_key_file: None,
            enc_c_destination_key_env: None,
            source_customer_key: None,
            destination_customer_key: None,
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
            portable_names: false,
        }
    }

    fn local_key_policy(&self) -> ObjectKeyPolicy {
        ObjectKeyPolicy::for_local_destination(self.portable_names)
    }
}

/// Download one remote object through the canonical copy implementation.
#[derive(Args, Debug)]
#[command(
    override_usage = "rc get [OPTIONS] <SOURCE> <TARGET>",
    after_help = GET_AFTER_HELP
)]
pub struct GetArgs {
    #[command(flatten)]
    pub transfer: CpArgs,
}

/// Upload one or more local paths through the canonical copy implementation.
#[derive(Args, Debug)]
#[command(after_help = PUT_AFTER_HELP)]
pub struct PutArgs {
    #[command(flatten)]
    pub transfer: CpArgs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferAlias {
    Copy,
    Get,
    Put,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_version_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct VersionCopyData {
    operation: &'static str,
    source: String,
    target: String,
    source_version_id: Option<String>,
    version_id: Option<String>,
    size_bytes: Option<i64>,
    size_human: Option<String>,
}

/// Execute the cp command
pub async fn execute(args: CpArgs, output_config: OutputConfig) -> ExitCode {
    execute_with_alias(args, output_config, TransferAlias::Copy).await
}

/// Execute the mc-compatible `get` command through the canonical copy path.
pub async fn execute_get(args: GetArgs, output_config: OutputConfig) -> ExitCode {
    execute_with_alias(args.transfer, output_config, TransferAlias::Get).await
}

/// Execute the mc-compatible `put` command through the canonical copy path.
pub async fn execute_put(args: PutArgs, output_config: OutputConfig) -> ExitCode {
    execute_with_alias(args.transfer, output_config, TransferAlias::Put).await
}

async fn execute_with_alias(
    mut args: CpArgs,
    output_config: OutputConfig,
    alias: TransferAlias,
) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if alias == TransferAlias::Get && args.sources.len() != 1 {
        return formatter.fail(
            ExitCode::UsageError,
            "get requires exactly one remote source and one local target",
        );
    }

    if let Err(error) = validate_destination_storage_class(args.storage_class.as_deref()) {
        return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
    }
    if let Err(error) = object_write_options(
        &args.fidelity,
        args.content_type.as_deref(),
        None,
        None,
        args.storage_class.clone(),
    ) {
        return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
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
        let parsed = match alias {
            // `put` defines every source operand as a local path, including
            // relative paths containing a slash that do not exist yet.
            TransferAlias::Put => Ok(ParsedPath::Local(PathBuf::from(source))),
            TransferAlias::Copy | TransferAlias::Get => {
                parse_cp_path(source, alias_manager.as_ref().ok())
            }
        };
        match parsed {
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

    let parsed_target = match alias {
        // `get` defines its target operand as a local path. Do not reinterpret
        // a non-existent `directory/file` path as an alias and bucket.
        TransferAlias::Get => Ok(ParsedPath::Local(PathBuf::from(&args.target))),
        TransferAlias::Copy | TransferAlias::Put => {
            parse_cp_path(&args.target, alias_manager.as_ref().ok())
        }
    };
    let target = match parsed_target {
        Ok(p) => p,
        Err(e) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &format!("Invalid target path: {e}"),
                REMOTE_PATH_SUGGESTION,
            );
        }
    };
    if let Err(error) = validate_alias_direction(alias, &sources, &target) {
        return formatter.fail(ExitCode::UsageError, error);
    }
    if let Err(error) = validate_fidelity_directions(&args, &sources, &target) {
        return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
    }
    let source_key_locator = match resolve_secret_locator(
        args.enc_c_source_key_file.clone(),
        args.enc_c_source_key_env.clone(),
    ) {
        Ok(locator) => locator,
        Err(error) => {
            return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
        }
    };
    let destination_key_locator = match resolve_secret_locator(
        args.enc_c_destination_key_file.clone(),
        args.enc_c_destination_key_env.clone(),
    ) {
        Ok(locator) => locator,
        Err(error) => {
            return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
        }
    };
    if source_key_locator.is_some()
        && sources
            .iter()
            .any(|path| matches!(path, ParsedPath::Local(_)))
    {
        return formatter.fail(
            ExitCode::UsageError,
            "SSE-C source keys require every source to be remote",
        );
    }
    if destination_key_locator.is_some() && matches!(target, ParsedPath::Local(_)) {
        return formatter.fail(
            ExitCode::UsageError,
            "SSE-C destination keys require a remote destination",
        );
    }
    if args.storage_class.is_some() && matches!(target, ParsedPath::Local(_)) {
        return formatter.fail(
            ExitCode::UsageError,
            "--storage-class requires a remote destination",
        );
    }

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
    if destination_key_locator.is_some() && target_encryption.is_some() {
        return formatter.fail(
            ExitCode::UsageError,
            "SSE-C destination keys cannot be combined with --enc-s3 or --enc-kms",
        );
    }
    if !args.dry_run
        && (source_key_locator.is_some() || destination_key_locator.is_some())
        && sources
            .iter()
            .all(|path| matches!(path, ParsedPath::Remote(_)))
        && matches!(target, ParsedPath::Remote(_))
    {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "RustFS beta.10 server-side SSE-C copy is not compatibility-proven; tracked by rustfs/backlog#1467",
        );
    }
    if !args.dry_run {
        args.source_customer_key = match load_customer_key(source_key_locator.as_ref()) {
            Ok(key) => key,
            Err(error) => {
                return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
            }
        };
        args.destination_customer_key = match load_customer_key(destination_key_locator.as_ref()) {
            Ok(key) => key,
            Err(error) => {
                return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
            }
        };
    }

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

fn validate_alias_direction(
    alias: TransferAlias,
    sources: &[ParsedPath],
    target: &ParsedPath,
) -> Result<(), &'static str> {
    match alias {
        TransferAlias::Copy => Ok(()),
        TransferAlias::Get if sources.len() == 1 && sources[0].is_remote() && target.is_local() => {
            Ok(())
        }
        TransferAlias::Get => Err("get requires exactly one remote source and one local target"),
        TransferAlias::Put if sources.iter().all(ParsedPath::is_local) && target.is_remote() => {
            Ok(())
        }
        TransferAlias::Put => Err("put requires one or more local sources and one remote target"),
    }
}

fn uses_transfer_planner(args: &CpArgs) -> bool {
    args.sources.len() > 1
        || args.recursive
        || args.skip_existing
        || !args.overwrite
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

pub(super) fn validate_destination_storage_class(value: Option<&str>) -> rc_core::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    match value {
        "STANDARD" | "REDUCED_REDUNDANCY" => Ok(()),
        "DEEP_ARCHIVE"
        | "EXPRESS_ONEZONE"
        | "FSX_ONTAP"
        | "FSX_OPENZFS"
        | "GLACIER"
        | "GLACIER_IR"
        | "INTELLIGENT_TIERING"
        | "ONEZONE_IA"
        | "OUTPOSTS"
        | "SNOW"
        | "STANDARD_IA" => Err(Error::UnsupportedFeature(format!(
            "RustFS beta.10 does not provide meaningful storage policy '{value}'"
        ))),
        value => Err(Error::InvalidPath(format!(
            "Unknown destination storage class '{value}'"
        ))),
    }
}

fn object_write_options(
    fidelity: &TransferFidelityArgs,
    content_type: Option<&str>,
    encryption: Option<&ObjectEncryptionRequest>,
    customer_key: Option<&SseCustomerKey>,
    storage_class: Option<String>,
) -> rc_core::Result<ObjectWriteOptions> {
    fidelity.build_write_options(content_type, encryption, customer_key, storage_class)
}

fn requested_metadata_directive(args: &CpArgs) -> Option<MetadataDirective> {
    if args.preserve {
        Some(MetadataDirective::Copy)
    } else {
        args.metadata_directive.map(Into::into)
    }
}

fn transfer_copy_options(
    args: &CpArgs,
    source_version_id: Option<String>,
    encryption: Option<&ObjectEncryptionRequest>,
) -> rc_core::Result<TransferCopyOptions> {
    let metadata_directive = requested_metadata_directive(args);
    let tagging_directive = args.tagging_directive.map(Into::into);
    let mut destination = object_write_options(
        &args.fidelity,
        args.content_type.as_deref(),
        encryption,
        args.destination_customer_key.as_ref(),
        args.storage_class.clone(),
    )?;
    if matches!(metadata_directive, Some(MetadataDirective::Replace))
        && destination.attributes.is_none()
    {
        destination.attributes = Some(Default::default());
    }
    if matches!(tagging_directive, Some(rc_core::TaggingDirective::Replace))
        && destination.tags.is_none()
    {
        destination.tags = Some(Default::default());
    }
    let options = TransferCopyOptions {
        source: TransferReadOptions {
            version_id: source_version_id,
            customer_key: args.source_customer_key.clone(),
            ..TransferReadOptions::default()
        },
        metadata_directive,
        tagging_directive,
        destination,
    };
    options.validate()?;
    Ok(options)
}

fn validate_fidelity_directions(
    args: &CpArgs,
    sources: &[ParsedPath],
    target: &ParsedPath,
) -> rc_core::Result<()> {
    let all_remote = sources
        .iter()
        .all(|source| matches!(source, ParsedPath::Remote(_)));
    let any_remote = sources
        .iter()
        .any(|source| matches!(source, ParsedPath::Remote(_)));
    let target_remote = matches!(target, ParsedPath::Remote(_));
    let has_copy_directive =
        args.preserve || args.metadata_directive.is_some() || args.tagging_directive.is_some();
    if has_copy_directive && !(all_remote && target_remote) {
        return Err(Error::InvalidPath(
            "Metadata and tagging copy directives require remote sources and a remote destination"
                .to_string(),
        ));
    }
    if !target_remote
        && (args.storage_class.is_some()
            || args.content_type.is_some()
            || args.fidelity.has_write_policy()
            || !args.enc_s3.is_empty()
            || !args.enc_kms.is_empty()
            || args.enc_c_destination_key_file.is_some()
            || args.enc_c_destination_key_env.is_some())
    {
        return Err(Error::InvalidPath(
            "Destination transfer policies require a remote destination".to_string(),
        ));
    }
    let same_alias_remote_copy = target_remote
        && sources.iter().any(|source| {
            matches!(
                source,
                ParsedPath::Remote(source) if target.as_remote().is_some_and(|target| source.alias == target.alias)
            )
        });
    if any_remote && target_remote {
        let copy_options = transfer_copy_options(args, None, None)?;
        if same_alias_remote_copy
            && matches!(
                copy_options.metadata_directive,
                Some(MetadataDirective::Replace)
            )
        {
            return Err(Error::UnsupportedFeature(
                "RustFS beta.10 does not preserve complete metadata REPLACE semantics; tracked by rustfs/backlog#1463"
                    .to_string(),
            ));
        }
        if copy_options.tagging_directive.is_some() || copy_options.destination.tags.is_some() {
            return Err(Error::UnsupportedFeature(
                "RustFS beta.10 does not preserve CopyObject tagging directives; tracked by rustfs/backlog#1462"
                    .to_string(),
            ));
        }
        if copy_options.destination.checksum.is_some() {
            return Err(Error::UnsupportedFeature(
                "RustFS beta.10 does not preserve CopyObject checksum selection; tracked by rustfs/backlog#1466"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn load_customer_key(locator: Option<&SecretLocator>) -> rc_core::Result<Option<SseCustomerKey>> {
    locator.map(SecretLocator::load_customer_key).transpose()
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
        source_info: Box<ObjectInfo>,
        encryption: Option<ObjectEncryptionRequest>,
    },
}

#[derive(Debug, Clone)]
struct PlannedCopyDetail {
    source_version_id: Option<String>,
    destination_version_id: Option<String>,
    upload_id: Option<String>,
}

type PlannedCopyDetails = Arc<AsyncMutex<HashMap<(String, String), PlannedCopyDetail>>>;

#[derive(Debug)]
struct PlannedCopyProgress {
    positions: StdMutex<HashMap<(String, String), u64>>,
    bar: Option<ProgressBar>,
}

impl PlannedCopyProgress {
    fn new(output_config: OutputConfig, total: u64) -> Self {
        Self {
            positions: StdMutex::new(HashMap::new()),
            bar: (total > 0).then(|| ProgressBar::new(output_config, total)),
        }
    }

    fn reset(&self, key: &(String, String)) {
        self.set(key, 0);
    }

    fn set(&self, key: &(String, String), bytes: u64) {
        let mut positions = self
            .positions
            .lock()
            .expect("planned copy progress lock should not be poisoned");
        positions.insert(key.clone(), bytes);
        let aggregate = positions.values().copied().fold(0_u64, u64::saturating_add);
        if let Some(bar) = &self.bar {
            bar.set_position(aggregate);
        }
    }

    fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

type SharedPlannedCopyProgress = Arc<PlannedCopyProgress>;

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
        args.source_customer_key.as_ref(),
        alias_manager,
        args.local_key_policy(),
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

    let mut plan = TransferPlan::build(candidates, &selection);
    if let Err(error) = validate_storage_class_plan(&plan, args.storage_class.as_deref()) {
        return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
    }
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

    let clients = match create_planned_client_cache(&plan.items, alias_manager).await {
        Ok(clients) => Arc::new(clients),
        Err(error) => {
            return formatter.fail(
                exit_code_for_core_error(&error),
                &format!("Failed to prepare copy clients: {error}"),
            );
        }
    };
    let skipped_existing = if args.skip_existing || !args.overwrite {
        match skip_existing_remote_targets(
            &mut plan,
            &clients,
            args.destination_customer_key.as_ref(),
        )
        .await
        {
            Ok(skipped) => skipped,
            Err(error) => {
                return formatter.fail(
                    exit_code_for_core_error(&error),
                    &format!("Failed to inspect copy destinations: {error}"),
                );
            }
        }
    } else {
        Vec::new()
    };

    if args.dry_run {
        for item in &plan.items {
            formatter.println(&format!(
                "Would copy: {} -> {}{}",
                formatter.style_file(&item.source),
                formatter.style_file(&item.target),
                transfer_policy_suffix(args)
            ));
        }
        for item in &skipped_existing {
            print_skipped_existing(formatter, item, true);
        }
        if args.summary || args.recursive || args.sources.len() > 1 {
            print_transfer_summary(formatter, &plan.summary);
        }
        return ExitCode::Success;
    }

    for item in &skipped_existing {
        print_skipped_existing(formatter, item, false);
    }
    if plan.items.is_empty() {
        if args.summary || args.recursive || args.sources.len() > 1 {
            print_transfer_summary(formatter, &plan.summary);
        }
        return ExitCode::Success;
    }

    let total_bytes = plan
        .items
        .iter()
        .filter_map(|item| {
            if matches!(item.payload, CpOperation::RemoteToRemote { .. }) {
                item.size_bytes
            } else {
                None
            }
        })
        .sum::<u64>();
    let executor = match TransferExecutor::new(controls) {
        Ok(executor) => executor,
        Err(error) => {
            return formatter.fail(ExitCode::UsageError, &error.to_string());
        }
    };
    let operation_args = Arc::new(args.clone());
    let transfer_cancellation = TransferCancellation::new();
    let multipart_cancellation = MultipartCopyCancellation::new();
    let signal_task = tokio::spawn({
        let transfer_cancellation = transfer_cancellation.clone();
        let multipart_cancellation = multipart_cancellation.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                multipart_cancellation.cancel();
                transfer_cancellation.cancel();
            }
        }
    });
    let copy_details = PlannedCopyDetails::default();
    let copy_progress = Arc::new(PlannedCopyProgress::new(
        formatter.output_config(),
        total_bytes,
    ));
    let report = executor
        .execute_with_cancellation(plan, transfer_cancellation, {
            let operation_args = Arc::clone(&operation_args);
            let clients = Arc::clone(&clients);
            let multipart_cancellation = multipart_cancellation.clone();
            let copy_details = Arc::clone(&copy_details);
            let copy_progress = Arc::clone(&copy_progress);
            move |item| {
                let operation_args = Arc::clone(&operation_args);
                let clients = Arc::clone(&clients);
                let multipart_cancellation = multipart_cancellation.clone();
                let copy_details = Arc::clone(&copy_details);
                let copy_progress = Arc::clone(&copy_progress);
                async move {
                    execute_planned_operation(
                        item,
                        &operation_args,
                        &clients,
                        &multipart_cancellation,
                        &copy_details,
                        &copy_progress,
                    )
                    .await
                }
            }
        })
        .await;
    signal_task.abort();
    let _ = signal_task.await;
    copy_progress.finish();

    let copy_details = copy_details.lock().await;
    for outcome in &report.outcomes {
        match &outcome.state {
            TransferOutcomeState::Success { bytes_transferred } => {
                let key = (outcome.item.source.clone(), outcome.item.target.clone());
                print_planned_success(
                    formatter,
                    &outcome.item,
                    *bytes_transferred,
                    copy_details.get(&key),
                );
            }
            TransferOutcomeState::Failed { error } => {
                formatter.error_with_code(exit_code_for_core_error(error), &error.to_string());
            }
            TransferOutcomeState::Cancelled { error } => {
                if let Some(error) = error {
                    formatter.warning(&format!(
                        "Cancelled transfer: {} ({error})",
                        outcome.item.source
                    ));
                } else {
                    formatter.warning(&format!(
                        "Cancelled before transfer: {}",
                        outcome.item.source
                    ));
                }
            }
        }
    }

    if args.summary || args.recursive || args.sources.len() > 1 {
        print_transfer_summary(formatter, &report.summary);
    }

    if report.was_cancelled {
        ExitCode::Interrupted
    } else {
        report
            .first_failure()
            .map_or(ExitCode::Success, exit_code_for_core_error)
    }
}

fn transfer_policy_suffix(args: &CpArgs) -> String {
    let mut policies = Vec::new();
    if let Some(value) = args.storage_class.as_deref() {
        policies.push(format!("storage-class={value}"));
    }
    if let Some(directive) = requested_metadata_directive(args) {
        policies.push(format!(
            "metadata={}",
            match directive {
                MetadataDirective::Copy => "copy",
                MetadataDirective::Replace => "replace",
            }
        ));
    } else if args.fidelity.has_attribute_policy() || args.content_type.is_some() {
        policies.push(format!("metadata=write({})", args.fidelity.metadata.len()));
    }
    if let Some(directive) = args.tagging_directive {
        policies.push(format!(
            "tags={}({})",
            match directive {
                TaggingDirectiveArg::Copy => "copy",
                TaggingDirectiveArg::Replace => "replace",
            },
            args.fidelity.tags.len()
        ));
    } else if args.fidelity.has_tag_policy() {
        policies.push(format!("tags=write({})", args.fidelity.tags.len()));
    }
    if args.fidelity.checksum.is_some() {
        policies.push("checksum=sha256".to_string());
    }
    if let Some(mode) = args.fidelity.retention_mode.as_deref() {
        policies.push(format!("retention={}", mode.to_ascii_lowercase()));
    }
    if let Some(state) = args.fidelity.legal_hold.as_deref() {
        policies.push(format!("legal-hold={}", state.to_ascii_lowercase()));
    }
    if args.enc_c_destination_key_file.is_some() || args.enc_c_destination_key_env.is_some() {
        policies.push("encryption=sse-c".to_string());
    } else if !args.enc_kms.is_empty() {
        policies.push("encryption=sse-kms".to_string());
    } else if !args.enc_s3.is_empty() {
        policies.push("encryption=sse-s3".to_string());
    }
    if policies.is_empty() {
        String::new()
    } else {
        format!(" [policy:{}]", policies.join(","))
    }
}

fn validate_storage_class_plan(
    plan: &TransferPlan<CpOperation>,
    storage_class: Option<&str>,
) -> rc_core::Result<()> {
    if storage_class.is_none() {
        return Ok(());
    }
    for item in &plan.items {
        let size = item.size_bytes.ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "Cannot guarantee storage class for a transfer with unknown size: {}",
                item.source
            ))
        })?;
        let multipart = match &item.payload {
            CpOperation::LocalToRemote { .. } => size > MULTIPART_THRESHOLD,
            CpOperation::RemoteToRemote { source, target, .. } if source.alias != target.alias => {
                size > MULTIPART_THRESHOLD
            }
            CpOperation::RemoteToRemote { .. } => rc_core::requires_multipart_copy(size),
            CpOperation::RemoteToLocal { .. } => false,
        };
        if multipart {
            return Err(Error::UnsupportedFeature(format!(
                "RustFS beta.10 does not persist storage class for multipart transfer: {}",
                item.source
            )));
        }
    }
    Ok(())
}

async fn execute_planned_operation(
    item: TransferCandidate<CpOperation>,
    args: &CpArgs,
    clients: &HashMap<String, Arc<S3Client>>,
    multipart_cancellation: &MultipartCopyCancellation,
    copy_details: &PlannedCopyDetails,
    copy_progress: &SharedPlannedCopyProgress,
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
            source_info,
            encryption,
        } => {
            let source_client = planned_client(clients, &source.alias)?;
            let target_client = planned_client(clients, &target.alias)?;
            let progress_key = (item.source.clone(), item.target.clone());
            copy_progress.reset(&progress_key);
            let result = perform_planned_remote_copy(
                source_client,
                target_client,
                source,
                target,
                source_info,
                encryption.as_ref(),
                multipart_cancellation,
                &|bytes| copy_progress.set(&progress_key, bytes),
                args,
            )
            .await?;
            copy_progress.set(&progress_key, result.bytes_copied);
            copy_details.lock().await.insert(
                (item.source, item.target),
                PlannedCopyDetail {
                    source_version_id: result.source_version_id,
                    destination_version_id: result.destination_version_id,
                    upload_id: result.upload_id,
                },
            );
            Ok(result.bytes_copied)
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
    let options = object_write_options(
        &args.fidelity,
        content_type,
        encryption,
        args.destination_customer_key.as_ref(),
        args.storage_class.clone(),
    )?;
    let info = client
        .put_object_from_path_with_options(target, source, &options, |_| {})
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
        .download_object_to_path_with_transfer_options(
            source,
            target,
            &TransferReadOptions {
                customer_key: args.source_customer_key.clone(),
                ..TransferReadOptions::default()
            },
            |_, _| {},
        )
        .await
}

struct PlannedRemoteCopyResult {
    bytes_copied: u64,
    source_version_id: Option<String>,
    destination_version_id: Option<String>,
    upload_id: Option<String>,
    object: ObjectInfo,
}

#[allow(clippy::too_many_arguments)]
async fn perform_planned_remote_copy(
    source_client: &S3Client,
    target_client: &S3Client,
    source: &RemotePath,
    target: &RemotePath,
    source_info: &ObjectInfo,
    encryption: Option<&ObjectEncryptionRequest>,
    cancellation: &MultipartCopyCancellation,
    on_progress: &(dyn Fn(u64) + Send + Sync),
    args: &CpArgs,
) -> rc_core::Result<PlannedRemoteCopyResult> {
    if source.alias != target.alias {
        return perform_cross_alias_remote_copy(
            source_client,
            target_client,
            source,
            target,
            source_info,
            encryption,
            on_progress,
            args,
        )
        .await;
    }
    if args.source_customer_key.is_some() || args.destination_customer_key.is_some() {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 server-side SSE-C copy is not compatibility-proven; tracked by rustfs/backlog#1467"
                .to_string(),
        ));
    }
    let planned_size = source_info
        .size_bytes
        .and_then(|size| u64::try_from(size).ok())
        .ok_or_else(|| Error::InvalidPath(format!("Source size is unavailable: {source}")))?;
    if rc_core::requires_multipart_copy(planned_size) {
        if args.storage_class.is_some() {
            return Err(Error::UnsupportedFeature(
                "RustFS beta.10 does not persist storage class for multipart copies".to_string(),
            ));
        }
        let current = source_client.head_object(source).await?;
        if !source_identity_matches(source_info, &current) {
            return Err(Error::Conflict(format!(
                "Source changed after copy planning: {source}"
            )));
        }
        let options = multipart_options_from_source(&current)?;
        let transfer = transfer_copy_options(args, current.version_id.clone(), encryption)?;
        let copied = source_client
            .multipart_copy_with_transfer_options(
                source,
                target,
                &options,
                &transfer,
                cancellation,
                on_progress,
            )
            .await?;
        return Ok(PlannedRemoteCopyResult {
            bytes_copied: copied.bytes_copied,
            source_version_id: options.source_version_id,
            destination_version_id: copied.object.version_id.clone(),
            upload_id: Some(copied.upload_id),
            object: copied.object,
        });
    }
    let options = transfer_copy_options(args, source_info.version_id.clone(), encryption)?;
    let copied = source_client
        .copy_object_with_transfer_options(source, target, &options)
        .await?;
    let bytes_copied = copied
        .size_bytes
        .and_then(|size| u64::try_from(size).ok())
        .unwrap_or(planned_size);
    on_progress(bytes_copied);
    Ok(PlannedRemoteCopyResult {
        bytes_copied,
        source_version_id: copied
            .source_version_id
            .clone()
            .or_else(|| source_info.version_id.clone()),
        destination_version_id: copied.version_id.clone(),
        upload_id: None,
        object: copied,
    })
}

#[allow(clippy::too_many_arguments)]
async fn perform_cross_alias_remote_copy(
    source_client: &S3Client,
    target_client: &S3Client,
    source: &RemotePath,
    target: &RemotePath,
    source_info: &ObjectInfo,
    encryption: Option<&ObjectEncryptionRequest>,
    on_progress: &(dyn Fn(u64) + Send + Sync),
    args: &CpArgs,
) -> rc_core::Result<PlannedRemoteCopyResult> {
    // Server-side CopyObject cannot target a different alias/endpoint. Stream
    // through a bounded temporary file so the destination write is a normal
    // upload with the destination alias credentials.
    if args.source_customer_key.is_some() || args.destination_customer_key.is_some() {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 server-side SSE-C copy is not compatibility-proven; tracked by rustfs/backlog#1467"
                .to_string(),
        ));
    }
    let planned_size = source_info
        .size_bytes
        .and_then(|size| u64::try_from(size).ok())
        .ok_or_else(|| Error::InvalidPath(format!("Source size is unavailable: {source}")))?;
    if args.storage_class.is_some() && planned_size > MULTIPART_THRESHOLD {
        return Err(Error::UnsupportedFeature(
            "RustFS beta.10 does not persist storage class for multipart uploads".to_string(),
        ));
    }

    let current = source_client.head_object(source).await?;
    if !source_identity_matches(source_info, &current) {
        return Err(Error::Conflict(format!(
            "Source changed after copy planning: {source}"
        )));
    }

    let staging = tempfile::Builder::new()
        .prefix("rc-cp-cross-alias-")
        .suffix(".part")
        .tempfile()?
        .into_temp_path();
    async {
        let downloaded = source_client
            .download_object_to_path_with_transfer_options(
                source,
                &staging,
                &TransferReadOptions {
                    version_id: current.version_id.clone(),
                    customer_key: args.source_customer_key.clone(),
                    ..TransferReadOptions::default()
                },
                |copied, _total| on_progress(copied),
            )
            .await?;
        if downloaded != planned_size {
            return Err(Error::Conflict(format!(
                "Source changed after copy planning: {source}"
            )));
        }
        let after = source_client
            .head_object_with_transfer_options(
                source,
                &TransferReadOptions {
                    version_id: current.version_id.clone(),
                    customer_key: args.source_customer_key.clone(),
                    ..TransferReadOptions::default()
                },
            )
            .await?;
        if !source_identity_matches(&current, &after) {
            return Err(Error::Conflict(format!(
                "Source changed after copy planning: {source}"
            )));
        }
        let options = piped_copy_write_options(args, &current, encryption)?;
        let object = target_client
            .put_object_from_path_with_options(target, &staging, &options, |copied| {
                on_progress(copied)
            })
            .await?;
        let bytes_copied = object
            .size_bytes
            .and_then(|size| u64::try_from(size).ok())
            .unwrap_or(downloaded);
        on_progress(bytes_copied);
        Ok(PlannedRemoteCopyResult {
            bytes_copied,
            source_version_id: current.version_id.clone(),
            destination_version_id: object.version_id.clone(),
            upload_id: None,
            object,
        })
    }
    .await
}

fn source_identity_matches(planned: &ObjectInfo, current: &ObjectInfo) -> bool {
    if planned.size_bytes != current.size_bytes {
        return false;
    }
    match (&planned.etag, &current.etag) {
        (Some(planned), Some(current)) => planned == current,
        // Without an ETag an unversioned object has no stable read identity;
        // only an identical explicit version can prove that it is unchanged.
        (None, None) => {
            planned.version_id.is_some()
                && planned.version_id.as_ref() == current.version_id.as_ref()
        }
        _ => false,
    }
}

fn piped_copy_write_options(
    args: &CpArgs,
    source: &ObjectInfo,
    encryption: Option<&ObjectEncryptionRequest>,
) -> rc_core::Result<ObjectWriteOptions> {
    let mut options = object_write_options(
        &args.fidelity,
        args.content_type.as_deref(),
        encryption,
        args.destination_customer_key.as_ref(),
        args.storage_class.clone(),
    )?;
    if matches!(
        requested_metadata_directive(args),
        Some(MetadataDirective::Replace)
    ) {
        return Ok(options);
    }
    let mut attributes = options.attributes.take().unwrap_or_default();
    if attributes.content_type.is_none() {
        attributes.content_type = source.content_type.clone();
    }
    if attributes.user_metadata.is_empty()
        && let Some(metadata) = &source.metadata
    {
        attributes.user_metadata.clone_from(metadata);
    }
    if attributes != ObjectAttributes::default() {
        options.attributes = Some(attributes);
    }
    Ok(options)
}

#[cfg(test)]
fn requires_multipart_copy(planned_size: Option<u64>) -> bool {
    planned_size.is_some_and(rc_core::requires_multipart_copy)
}

fn multipart_options_from_source(source: &ObjectInfo) -> rc_core::Result<MultipartCopyOptions> {
    let source_size = source
        .size_bytes
        .and_then(|size| u64::try_from(size).ok())
        .ok_or_else(|| {
            Error::InvalidPath("Multipart copy source size is unavailable".to_string())
        })?;
    let source_etag = source.etag.clone().ok_or_else(|| {
        Error::InvalidPath("Multipart copy source ETag is unavailable".to_string())
    })?;
    let mut options = MultipartCopyOptions::new(source_size, source_etag)?;
    options.source_version_id = source.version_id.clone();
    options.content_type = source.content_type.clone();
    options.metadata = source.metadata.clone().unwrap_or_default();
    Ok(options)
}

fn operation_alias(operation: &CpOperation) -> &str {
    match operation {
        CpOperation::LocalToRemote { target, .. } => &target.alias,
        CpOperation::RemoteToLocal { source, .. } | CpOperation::RemoteToRemote { source, .. } => {
            &source.alias
        }
    }
}

fn operation_client_aliases(operation: &CpOperation) -> Vec<&str> {
    match operation {
        CpOperation::LocalToRemote { target, .. } => vec![target.alias.as_str()],
        CpOperation::RemoteToLocal { source, .. } => vec![source.alias.as_str()],
        CpOperation::RemoteToRemote { source, target, .. } => {
            if source.alias == target.alias {
                vec![source.alias.as_str()]
            } else {
                vec![source.alias.as_str(), target.alias.as_str()]
            }
        }
    }
}

fn planned_client_aliases(items: &[TransferCandidate<CpOperation>]) -> BTreeSet<String> {
    items
        .iter()
        .flat_map(|item| {
            operation_client_aliases(&item.payload)
                .into_iter()
                .map(ToOwned::to_owned)
        })
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
    detail: Option<&PlannedCopyDetail>,
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
            version_id: None,
            source_version_id: None,
        });
    } else {
        let mut line = format!(
            "{} -> {} ({})",
            formatter.style_file(&item.source),
            formatter.style_file(&item.target),
            formatter.style_size(size_human.as_deref().unwrap_or_default())
        );
        if let Some(detail) = detail {
            if let Some(version_id) = &detail.source_version_id {
                line.push_str(&format!(" source-version={version_id}"));
            }
            if let Some(version_id) = &detail.destination_version_id {
                line.push_str(&format!(" destination-version={version_id}"));
            }
            if let Some(upload_id) = &detail.upload_id {
                line.push_str(&format!(" upload-id={upload_id}"));
            }
        }
        formatter.println(&line);
    }
}

fn print_skipped_existing(
    formatter: &Formatter,
    item: &TransferCandidate<CpOperation>,
    dry_run: bool,
) {
    let action = if dry_run {
        "Would skip existing"
    } else {
        "Skipped existing"
    };
    formatter.println(&format!(
        "{action}: {} -> {}",
        formatter.style_file(&item.source),
        formatter.style_file(&item.target)
    ));
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

pub(super) fn exit_code_for_core_error(error: &Error) -> ExitCode {
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

#[allow(clippy::too_many_arguments)]
async fn build_transfer_candidates(
    sources: &[ParsedPath],
    target: &ParsedPath,
    target_is_container: bool,
    recursive: bool,
    encryption: Option<ObjectEncryptionRequest>,
    source_customer_key: Option<&SseCustomerKey>,
    alias_manager: &AliasManager,
    key_policy: ObjectKeyPolicy,
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
                    source_customer_key,
                    alias_manager,
                    &mut planning_clients,
                    &mut candidates,
                    key_policy,
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
        if let CpOperation::RemoteToRemote { source, target, .. } = &candidate.payload
            && source == target
        {
            return Err(Error::InvalidPath(format!(
                "Source and destination resolve to the same object '{}'",
                candidate.source
            )));
        }
        if !targets.insert(candidate.target.clone()) {
            return Err(Error::InvalidPath(format!(
                "Multiple sources resolve to the same destination '{}'",
                candidate.target
            )));
        }
    }
    Ok(())
}

async fn skip_existing_remote_targets(
    plan: &mut TransferPlan<CpOperation>,
    clients: &HashMap<String, Arc<S3Client>>,
    destination_customer_key: Option<&SseCustomerKey>,
) -> rc_core::Result<Vec<TransferCandidate<CpOperation>>> {
    let mut retained = Vec::with_capacity(plan.items.len());
    let mut skipped = Vec::new();
    for candidate in plan.items.drain(..) {
        let destination = match &candidate.payload {
            CpOperation::LocalToRemote { target, .. }
            | CpOperation::RemoteToRemote { target, .. } => Some(target),
            CpOperation::RemoteToLocal { .. } => None,
        };
        let Some(destination) = destination else {
            retained.push(candidate);
            continue;
        };
        let client = planned_client(clients, &destination.alias)?;
        match client
            .head_object_with_transfer_options(
                destination,
                &TransferReadOptions {
                    customer_key: destination_customer_key.cloned(),
                    ..TransferReadOptions::default()
                },
            )
            .await
        {
            Ok(_) => skipped.push(candidate),
            Err(Error::NotFound(_)) | Err(Error::VersionNotFound { .. }) => {
                retained.push(candidate);
            }
            Err(error) => return Err(error),
        }
    }
    plan.items = retained;
    plan.summary.planned = plan.items.len();
    plan.summary.skipped = plan.summary.skipped.saturating_add(skipped.len());
    Ok(skipped)
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
            remote_child(target, &name, ObjectKeyPolicy::for_remote_destination())?
        } else {
            normalize_remote_target(target, ObjectKeyPolicy::for_remote_destination())?
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
            remote_child(
                target,
                &target_relative,
                ObjectKeyPolicy::for_remote_destination(),
            )?,
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
    source_customer_key: Option<&SseCustomerKey>,
    alias_manager: &AliasManager,
    planning_clients: &mut HashMap<String, Arc<S3Client>>,
    candidates: &mut Vec<TransferCandidate<CpOperation>>,
    key_policy: ObjectKeyPolicy,
) -> rc_core::Result<()> {
    let is_prefix = source.key.is_empty() || source.key.ends_with('/') || recursive;
    let client = planning_client(planning_clients, alias_manager, &source.alias).await?;

    if is_prefix {
        if !recursive {
            return Err(Error::InvalidPath(
                "Remote prefix copy requires -r/--recursive".to_string(),
            ));
        }
        if !target_is_container {
            return Err(Error::InvalidPath(
                "Recursive copy requires a directory or remote prefix destination ending in '/'"
                    .to_string(),
            ));
        }

        let listing_source = recursive_listing_source(source);
        if let ParsedPath::Remote(target) = target
            && remote_copy_scopes_overlap(&listing_source, target)
        {
            return Err(Error::Conflict(format!(
                "Recursive source '{}' overlaps destination '{}'",
                listing_source, target
            )));
        }

        let source_root = recursive_source_root(&listing_source, multiple_sources);
        let mut continuation_token = None;
        let mut seen_tokens = HashSet::new();
        loop {
            let result = client
                .list_objects(
                    &listing_source,
                    rc_core::ListOptions {
                        recursive: true,
                        max_keys: Some(1_000),
                        continuation_token: continuation_token.clone(),
                        ..Default::default()
                    },
                )
                .await?;
            for object in result.items.into_iter().filter(|object| !object.is_dir) {
                let object_source =
                    RemotePath::new(&listing_source.alias, &listing_source.bucket, &object.key);
                match target {
                    ParsedPath::Local(target_root) => {
                        let relative = safe_download_relative_path(
                            &object.key,
                            &listing_source.key,
                            key_policy,
                        )
                        .map_err(Error::InvalidPath)?;
                        let relative_string = relative.to_string_lossy().replace('\\', "/");
                        let target_relative = if source_root.is_empty() {
                            relative.clone()
                        } else {
                            PathBuf::from(&source_root).join(&relative)
                        };
                        let destination = safe_download_destination(target_root, &target_relative)
                            .await
                            .map_err(Error::InvalidPath)?;
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
                    ParsedPath::Remote(target) => {
                        let (destination, relative) = recursive_remote_target(
                            &listing_source,
                            target,
                            &object.key,
                            multiple_sources,
                            ObjectKeyPolicy::for_remote_destination(),
                        )?;
                        let size_bytes =
                            object.size_bytes.and_then(|size| u64::try_from(size).ok());
                        candidates.push(TransferCandidate {
                            payload: CpOperation::RemoteToRemote {
                                source: object_source.clone(),
                                target: destination.clone(),
                                source_info: Box::new(object.clone()),
                                encryption: encryption.clone(),
                            },
                            source: object_source.to_string(),
                            target: destination.to_string(),
                            relative_path: relative,
                            modified: object.last_modified,
                            size_bytes,
                        });
                    }
                }
            }
            if !result.truncated {
                break;
            }
            let next_token = result.continuation_token.ok_or_else(|| {
                Error::InvalidPath(
                    "Truncated object listing did not include a continuation token".to_string(),
                )
            })?;
            if !seen_tokens.insert(next_token.clone()) {
                return Err(Error::Conflict(
                    "Object listing repeated a continuation token".to_string(),
                ));
            }
            continuation_token = Some(next_token);
        }
        return Ok(());
    }

    let object = client
        .head_object_with_transfer_options(
            source,
            &TransferReadOptions {
                customer_key: source_customer_key.cloned(),
                ..TransferReadOptions::default()
            },
        )
        .await?;
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
                let relative = safe_download_relative_path(name, "", key_policy)
                    .map_err(Error::InvalidPath)?;
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
            let destination = if target_is_container {
                remote_child(target, name, ObjectKeyPolicy::for_remote_destination())?
            } else {
                normalize_remote_target(target, ObjectKeyPolicy::for_remote_destination())?
            };
            candidates.push(TransferCandidate {
                payload: CpOperation::RemoteToRemote {
                    source: source.clone(),
                    target: destination.clone(),
                    source_info: Box::new(object),
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

fn remote_child(
    parent: &RemotePath,
    relative: &str,
    policy: ObjectKeyPolicy,
) -> rc_core::Result<RemotePath> {
    let parent_key = normalize_remote_prefix(&parent.key, policy)?;
    let relative = normalize_relative_key(relative, policy)?;
    let key = if parent_key.is_empty() {
        relative
    } else {
        format!("{parent_key}/{relative}")
    };
    Ok(RemotePath::new(&parent.alias, &parent.bucket, key))
}

fn normalize_remote_target(
    target: &RemotePath,
    policy: ObjectKeyPolicy,
) -> rc_core::Result<RemotePath> {
    let key = normalize_remote_prefix(&target.key, policy)?;
    if target.key.ends_with('/') && !key.is_empty() {
        Ok(RemotePath::new(
            &target.alias,
            &target.bucket,
            format!("{key}/"),
        ))
    } else {
        Ok(RemotePath::new(&target.alias, &target.bucket, key))
    }
}

fn normalize_remote_prefix(key: &str, policy: ObjectKeyPolicy) -> rc_core::Result<String> {
    if key.is_empty() {
        return Ok(String::new());
    }
    normalize_relative_key(key, policy)
}

fn recursive_listing_source(source: &RemotePath) -> RemotePath {
    let key = if source.key.is_empty() || source.key.ends_with('/') {
        source.key.clone()
    } else {
        format!("{}/", source.key)
    };
    RemotePath::new(&source.alias, &source.bucket, key)
}

fn recursive_source_root(source: &RemotePath, multiple_sources: bool) -> String {
    if !multiple_sources {
        return String::new();
    }
    source
        .key
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(&source.bucket)
        .to_string()
}

fn recursive_remote_target(
    source: &RemotePath,
    target: &RemotePath,
    object_key: &str,
    multiple_sources: bool,
    policy: ObjectKeyPolicy,
) -> rc_core::Result<(RemotePath, String)> {
    let relative = object_key.strip_prefix(&source.key).ok_or_else(|| {
        Error::InvalidPath(format!(
            "Listed object key '{object_key}' is outside source prefix '{}'",
            source.key
        ))
    })?;
    if relative.is_empty() {
        return Err(Error::InvalidPath(format!(
            "Listed object key '{object_key}' does not identify a child of source prefix '{}'",
            source.key
        )));
    }
    let destination_relative = match recursive_source_root(source, multiple_sources) {
        root if root.is_empty() => relative.to_string(),
        root => format!("{root}/{relative}"),
    };
    let normalized_relative = normalize_relative_key(relative, policy)?;
    let normalized_destination_relative = normalize_relative_key(&destination_relative, policy)?;
    Ok((
        remote_child(target, &normalized_destination_relative, policy)?,
        normalized_relative,
    ))
}

fn remote_copy_scopes_overlap(source: &RemotePath, target: &RemotePath) -> bool {
    if source.alias != target.alias || source.bucket != target.bucket {
        return false;
    }
    let source_prefix = recursive_listing_source(source).key;
    let target_prefix = recursive_listing_source(target).key;
    source_prefix.is_empty()
        || target_prefix.is_empty()
        || source_prefix.starts_with(&target_prefix)
        || target_prefix.starts_with(&source_prefix)
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
        print_copy_json(formatter, info, src_display, dst_display);
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
    if args.storage_class.is_some() && file_size > MULTIPART_THRESHOLD {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "RustFS beta.10 does not persist storage class for multipart uploads",
        );
    }
    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!(
            "Would copy: {styled_src} -> {styled_dst}{}",
            transfer_policy_suffix(args)
        ));
        return ExitCode::Success;
    }

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
    let upload_result = match object_write_options(
        &args.fidelity,
        content_type,
        encryption,
        args.destination_customer_key.as_ref(),
        args.storage_class.clone(),
    ) {
        Ok(options) => {
            client
                .put_object_from_path_with_options(&target, src, &options, |bytes_sent| {
                    if let Some(ref pb) = progress {
                        pb.set_position(bytes_sent);
                    }
                })
                .await
        }
        Err(error) => Err(error),
    };
    match upload_result {
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
                exit_code_for_core_error(&e),
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
        let filename = match safe_download_relative_path(filename, "", args.local_key_policy()) {
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
        .download_object_to_path_with_transfer_options(
            src,
            &dst_path,
            &TransferReadOptions {
                customer_key: args.source_customer_key.clone(),
                ..TransferReadOptions::default()
            },
            |bytes_downloaded, total_size| {
                update_download_progress(
                    &mut progress,
                    &output_config,
                    bytes_downloaded,
                    total_size,
                );
            },
        )
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
                    version_id: None,
                    source_version_id: None,
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
                    let relative_path = match safe_download_relative_path(
                        &item.key,
                        &src.key,
                        args.local_key_policy(),
                    ) {
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

pub(super) fn safe_download_relative_path(
    key: &str,
    prefix: &str,
    policy: ObjectKeyPolicy,
) -> Result<PathBuf, String> {
    relative_local_path_from_key(key, prefix, policy).map_err(|error| error.to_string())
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
    if args.source_customer_key.is_some() || args.destination_customer_key.is_some() {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "RustFS beta.10 server-side SSE-C copy is not compatibility-proven; tracked by rustfs/backlog#1467",
        );
    }

    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    // Same-alias copies use server-side CopyObject. Different aliases download
    // through a temporary file and upload with the destination credentials.
    let source_alias = match alias_manager.get(&src.alias) {
        Ok(a) => a,
        Err(_) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", src.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };
    let source_client = match S3Client::new(source_alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };
    let target_client;
    let target_client_ref = if src.alias == dst.alias {
        &source_client
    } else {
        let destination_alias = match alias_manager.get(&dst.alias) {
            Ok(a) => a,
            Err(_) => {
                return formatter.fail_with_suggestion(
                    ExitCode::NotFound,
                    &format!("Alias '{}' not found", dst.alias),
                    "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
                );
            }
        };
        target_client = match S3Client::new(destination_alias).await {
            Ok(c) => c,
            Err(e) => {
                return formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to create destination S3 client: {e}"),
                );
            }
        };
        &target_client
    };

    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst.key);

    if args.dry_run && args.storage_class.is_none() {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!(
            "Would copy: {styled_src} -> {styled_dst}{}",
            transfer_policy_suffix(args)
        ));
        return ExitCode::Success;
    }

    let source_info = match source_client.head_object(src).await {
        Ok(info) => info,
        Err(Error::NotFound(_)) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Source not found: {src_display}"),
                "Check the source bucket and object key, then retry the copy command.",
            );
        }
        Err(error) => {
            return formatter.fail(
                exit_code_for_core_error(&error),
                &format!("Failed to inspect source object: {error}"),
            );
        }
    };
    let source_size = source_info
        .size_bytes
        .and_then(|size| u64::try_from(size).ok());
    if args.storage_class.is_some()
        && source_size.is_none_or(|size| {
            if src.alias == dst.alias {
                rc_core::requires_multipart_copy(size)
            } else {
                size > MULTIPART_THRESHOLD
            }
        })
    {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "RustFS beta.10 does not persist storage class for multipart or unknown-size copies",
        );
    }
    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!(
            "Would copy: {styled_src} -> {styled_dst}{}",
            transfer_policy_suffix(args)
        ));
        return ExitCode::Success;
    }

    let cancellation = MultipartCopyCancellation::new();
    let transfer_cancellation = TransferCancellation::new();
    let signal_task = tokio::spawn({
        let cancellation = cancellation.clone();
        let transfer_cancellation = transfer_cancellation.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancellation.cancel();
                transfer_cancellation.cancel();
            }
        }
    });
    let ignore_progress = |_: u64| {};
    let copy = perform_planned_remote_copy(
        &source_client,
        target_client_ref,
        src,
        dst,
        &source_info,
        encryption,
        &cancellation,
        &ignore_progress,
        args,
    );
    tokio::pin!(copy);
    let result = tokio::select! {
        biased;
        _ = transfer_cancellation.cancelled() => {
            // Do not drop an in-flight request. Multipart copy observes its own token and performs
            // cleanup; CopyObject is allowed to settle before the command reports interruption.
            let _ = copy.await;
            Err(Error::Interrupted("Copy interrupted".to_string()))
        }
        result = &mut copy => {
            if transfer_cancellation.is_cancelled() {
                Err(Error::Interrupted("Copy interrupted".to_string()))
            } else {
                result
            }
        }
    };
    signal_task.abort();
    let _ = signal_task.await;

    match result {
        Ok(result) => {
            if formatter.is_json() {
                print_copy_json(formatter, &result.object, &src_display, &dst_display);
            } else {
                let styled_src = formatter.style_file(&src_display);
                let styled_dst = formatter.style_file(&dst_display);
                let styled_size =
                    formatter.style_size(&result.object.size_human.unwrap_or_else(|| {
                        humansize::format_size(result.bytes_copied, humansize::BINARY)
                    }));
                let upload = result
                    .upload_id
                    .map(|upload_id| format!(" upload-id={upload_id}"))
                    .unwrap_or_default();
                formatter.println(&format!(
                    "{styled_src} -> {styled_dst} ({styled_size}){upload}"
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            if matches!(error, Error::NotFound(_) | Error::VersionNotFound { .. }) {
                formatter.fail_with_suggestion(
                    ExitCode::NotFound,
                    &format!("Source not found: {src_display}"),
                    "Check the source bucket and object key, then retry the copy command.",
                )
            } else {
                formatter.fail(
                    exit_code_for_core_error(&error),
                    &format!("Failed to copy: {error}"),
                )
            }
        }
    }
}

fn print_copy_json(formatter: &Formatter, info: &rc_core::ObjectInfo, source: &str, target: &str) {
    if info.version_id.is_some() || info.source_version_id.is_some() {
        formatter.json(&V3SuccessEnvelope::versioned_objects(VersionCopyData {
            operation: "copy",
            source: source.to_string(),
            target: target.to_string(),
            source_version_id: info.source_version_id.clone(),
            version_id: info.version_id.clone(),
            size_bytes: info.size_bytes,
            size_human: info.size_human.clone(),
        }));
    } else {
        formatter.json(&CpOutput {
            status: "success",
            source: source.to_string(),
            target: target.to_string(),
            size_bytes: info.size_bytes,
            size_human: info.size_human.clone(),
            version_id: None,
            source_version_id: None,
        });
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
        let relative = safe_download_relative_path(
            "reports/2026/july/data.csv",
            "reports/",
            ObjectKeyPolicy::Logical,
        )
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
            "/absolute/path",
        ] {
            assert!(
                safe_download_relative_path(key, "reports/", ObjectKeyPolicy::Logical).is_err(),
                "unsafe key should be rejected: {key}"
            );
        }
    }

    #[test]
    fn download_relative_path_accepts_colon_keys_on_logical_destinations() {
        let relative = safe_download_relative_path(
            "loki/fake/deadbeef/19f6abd9af4:19f6abe0e77:499628ff",
            "loki/",
            ObjectKeyPolicy::Logical,
        )
        .expect("colon keys are valid Unix file names");

        assert_eq!(
            relative,
            PathBuf::from("fake")
                .join("deadbeef")
                .join("19f6abd9af4:19f6abe0e77:499628ff")
        );
    }

    #[test]
    fn download_relative_path_rejects_colon_keys_when_portable_names_requested() {
        for key in [
            "reports/C:/escaped",
            "reports/safe:stream",
            "reports/CON.txt",
            "reports/trailing.",
        ] {
            assert!(
                safe_download_relative_path(key, "reports/", ObjectKeyPolicy::WindowsPortable)
                    .is_err(),
                "portable-unsafe key should be rejected: {key}"
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

    #[test]
    fn planned_client_aliases_include_both_sides_of_a_cross_alias_copy() {
        let candidate = TransferCandidate {
            payload: CpOperation::RemoteToRemote {
                source: RemotePath::new("alpha", "source", "file.txt"),
                target: RemotePath::new("beta", "target", "file.txt"),
                source_info: Box::new(ObjectInfo::file("file.txt", 4)),
                encryption: None,
            },
            source: "alpha/source/file.txt".to_string(),
            target: "beta/target/file.txt".to_string(),
            relative_path: "file.txt".to_string(),
            modified: None,
            size_bytes: Some(4),
        };

        assert_eq!(
            planned_client_aliases(&[candidate])
                .into_iter()
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn planned_client_aliases_keep_same_alias_remote_copy_on_one_alias() {
        let candidate = TransferCandidate {
            payload: CpOperation::RemoteToRemote {
                source: RemotePath::new("shared", "source", "file.txt"),
                target: RemotePath::new("shared", "target", "file.txt"),
                source_info: Box::new(ObjectInfo::file("file.txt", 4)),
                encryption: None,
            },
            source: "shared/source/file.txt".to_string(),
            target: "shared/target/file.txt".to_string(),
            relative_path: "file.txt".to_string(),
            modified: None,
            size_bytes: Some(4),
        };

        assert_eq!(
            planned_client_aliases(&[candidate])
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
                source_info: Box::new(ObjectInfo::file(
                    "large.bin",
                    (MAX_SINGLE_COPY_SIZE + 1) as i64,
                )),
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
    fn planned_remote_copy_uses_exact_five_gib_boundary() {
        assert!(!requires_multipart_copy(Some(MAX_SINGLE_COPY_SIZE)));
        assert!(requires_multipart_copy(Some(MAX_SINGLE_COPY_SIZE + 1)));
    }

    #[test]
    fn planned_progress_replaces_attempt_bytes_instead_of_double_counting() {
        let progress = PlannedCopyProgress::new(
            OutputConfig {
                no_progress: true,
                ..OutputConfig::default()
            },
            20,
        );
        let first = ("source/a".to_string(), "target/a".to_string());
        let second = ("source/b".to_string(), "target/b".to_string());

        progress.set(&first, 7);
        progress.set(&second, 5);
        progress.reset(&first);
        progress.set(&first, 15);

        let positions = progress
            .positions
            .lock()
            .expect("planned copy progress lock");
        assert_eq!(positions.get(&first), Some(&15));
        assert_eq!(positions.get(&second), Some(&5));
        assert_eq!(
            positions.values().copied().fold(0_u64, u64::saturating_add),
            20
        );
    }

    #[test]
    fn recursive_remote_mapping_preserves_source_relative_keys() {
        let source = RemotePath::new("shared", "source", "src/");
        let target = RemotePath::new("shared", "destination", "archive/");

        let (destination, relative) = recursive_remote_target(
            &source,
            &target,
            "src/nested/report.csv",
            false,
            ObjectKeyPolicy::for_remote_destination(),
        )
        .expect("map recursive object");

        assert_eq!(relative, "nested/report.csv");
        assert_eq!(destination.key, "archive/nested/report.csv");
    }

    #[test]
    fn recursive_bucket_root_mapping_keeps_the_full_object_key() {
        let source = RemotePath::new("shared", "source", "");
        let target = RemotePath::new("shared", "destination", "archive/");

        let (destination, relative) = recursive_remote_target(
            &source,
            &target,
            "nested/report.csv",
            false,
            ObjectKeyPolicy::for_remote_destination(),
        )
        .expect("map bucket object");

        assert_eq!(relative, "nested/report.csv");
        assert_eq!(destination.key, "archive/nested/report.csv");
    }

    #[test]
    fn recursive_remote_mapping_rejects_unsafe_listed_keys() {
        let source = RemotePath::new("shared", "source", "src/");
        let target = RemotePath::new("shared", "destination", "archive/");

        for object_key in [
            "/absolute.txt",
            "src/../escape.txt",
            "src\\escape.txt",
            "src/control\u{0007}.txt",
        ] {
            assert!(
                recursive_remote_target(
                    &source,
                    &target,
                    object_key,
                    false,
                    ObjectKeyPolicy::for_remote_destination(),
                )
                .is_err(),
                "unsafe listed key should be rejected: {object_key:?}"
            );
        }
    }

    #[test]
    fn remote_child_rejects_unsafe_relative_keys() {
        let target = RemotePath::new("shared", "destination", "archive/");

        for relative in [
            "../escape.txt",
            "nested\\escape.txt",
            "nested/control\u{0007}.txt",
        ] {
            assert!(
                remote_child(&target, relative, ObjectKeyPolicy::for_remote_destination(),)
                    .is_err(),
                "unsafe relative key should be rejected: {relative:?}"
            );
        }
    }

    #[test]
    fn remote_target_rejects_absolute_prefixes() {
        let target = RemotePath::new("shared", "destination", "/archive/");

        assert!(
            normalize_remote_target(&target, ObjectKeyPolicy::for_remote_destination()).is_err()
        );
    }

    #[test]
    fn recursive_remote_overlap_is_boundary_aware_and_symmetric() {
        let source = RemotePath::new("shared", "bucket", "src/");
        let child = RemotePath::new("shared", "bucket", "src/archive/");
        let sibling = RemotePath::new("shared", "bucket", "src-old/");
        let other_bucket = RemotePath::new("shared", "other", "src/archive/");

        assert!(remote_copy_scopes_overlap(&source, &child));
        assert!(remote_copy_scopes_overlap(&child, &source));
        assert!(!remote_copy_scopes_overlap(&source, &sibling));
        assert!(!remote_copy_scopes_overlap(&source, &other_bucket));
    }

    #[test]
    fn multipart_options_require_and_preserve_planned_source_identity() {
        let mut source = rc_core::ObjectInfo::file("large.bin", (MAX_SINGLE_COPY_SIZE + 1) as i64);
        source.etag = Some("planned-etag".to_string());
        source.version_id = Some("source-version".to_string());
        source.content_type = Some("application/octet-stream".to_string());
        source.metadata = Some(HashMap::from([(
            "project".to_string(),
            "archive".to_string(),
        )]));

        let options = multipart_options_from_source(&source).expect("multipart options");

        assert_eq!(options.source_size, MAX_SINGLE_COPY_SIZE + 1);
        assert_eq!(options.source_etag, "planned-etag");
        assert_eq!(options.source_version_id.as_deref(), Some("source-version"));
        assert_eq!(
            options.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            options.metadata.get("project").map(String::as_str),
            Some("archive")
        );
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
            None,
            &alias_manager,
            ObjectKeyPolicy::Logical,
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
            version_id: Some("destination-v2".to_string()),
            source_version_id: Some("source-v1".to_string()),
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"size_bytes\":1024"));
        assert!(json.contains("\"version_id\":\"destination-v2\""));
        assert!(json.contains("\"source_version_id\":\"source-v1\""));
    }

    #[test]
    fn test_cp_output_skips_none_fields() {
        let output = CpOutput {
            status: "success",
            source: "src".to_string(),
            target: "dst".to_string(),
            size_bytes: None,
            size_human: None,
            version_id: None,
            source_version_id: None,
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(!json.contains("size_bytes"));
        assert!(!json.contains("size_human"));
        assert!(!json.contains("version_id"));
    }

    #[test]
    fn versioned_copy_output_uses_v3_and_preserves_both_version_ids() {
        let envelope = V3SuccessEnvelope::versioned_objects(VersionCopyData {
            operation: "copy",
            source: "src/object.txt".to_string(),
            target: "dst/object.txt".to_string(),
            source_version_id: Some("source-v1".to_string()),
            version_id: Some("destination-v2".to_string()),
            size_bytes: Some(1024),
            size_human: Some("1 KiB".to_string()),
        });

        let json = serde_json::to_value(envelope).expect("serialize versioned copy output");
        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["type"], "versioned_objects");
        assert_eq!(json["data"]["operation"], "copy");
        assert_eq!(json["data"]["source_version_id"], "source-v1");
        assert_eq!(json["data"]["version_id"], "destination-v2");
    }

    #[tokio::test]
    async fn storage_class_validation_has_usage_and_unsupported_exit_codes() {
        for (storage_class, expected) in [
            ("not-a-class", ExitCode::UsageError),
            ("STANDARD_IA", ExitCode::UnsupportedFeature),
        ] {
            let mut args = CpArgs::single("source.txt", "local/bucket/target.txt");
            args.storage_class = Some(storage_class.to_string());

            assert_eq!(execute(args, OutputConfig::default()).await, expected);
        }
    }

    #[test]
    fn fidelity_direction_preflight_rejects_silent_or_beta10_unsupported_paths() {
        let local = ParsedPath::Local(PathBuf::from("report.json"));
        let source = ParsedPath::Remote(RemotePath::new("test", "source", "report.json"));
        let target = ParsedPath::Remote(RemotePath::new("test", "target", "report.json"));

        let mut preserve_upload = CpArgs::single("report.json", "test/target/report.json");
        preserve_upload.preserve = true;
        assert!(matches!(
            validate_fidelity_directions(&preserve_upload, std::slice::from_ref(&local), &target),
            Err(Error::InvalidPath(_))
        ));

        let mut replace = CpArgs::single("test/source/report.json", "test/target/report.json");
        replace.metadata_directive = Some(MetadataDirectiveArg::Replace);
        assert!(matches!(
            validate_fidelity_directions(&replace, std::slice::from_ref(&source), &target),
            Err(Error::UnsupportedFeature(_))
        ));

        let cross_alias_source =
            ParsedPath::Remote(RemotePath::new("source", "source", "report.json"));
        let cross_alias_target =
            ParsedPath::Remote(RemotePath::new("destination", "target", "report.json"));
        assert!(
            validate_fidelity_directions(
                &replace,
                std::slice::from_ref(&cross_alias_source),
                &cross_alias_target,
            )
            .is_ok(),
            "metadata REPLACE is implemented by the cross-alias upload path"
        );

        let mut tags = CpArgs::single("test/source/report.json", "test/target/report.json");
        tags.tagging_directive = Some(TaggingDirectiveArg::Replace);
        tags.fidelity.tags = vec!["env=prod".to_string()];
        assert!(matches!(
            validate_fidelity_directions(&tags, std::slice::from_ref(&source), &target),
            Err(Error::UnsupportedFeature(_))
        ));

        let mut checksum = CpArgs::single("test/source/report.json", "test/target/report.json");
        checksum.fidelity.checksum = Some("sha256".to_string());
        assert!(matches!(
            validate_fidelity_directions(&checksum, std::slice::from_ref(&source), &target),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn source_identity_validation_detects_same_size_etag_changes() {
        let mut planned = ObjectInfo::file("report.json", 4);
        planned.etag = Some("planned".to_string());
        let mut current = planned.clone();
        assert!(source_identity_matches(&planned, &current));

        current.etag = Some("changed".to_string());
        assert!(!source_identity_matches(&planned, &current));

        current.etag = None;
        assert!(!source_identity_matches(&planned, &current));

        planned.etag = None;
        assert!(!source_identity_matches(&planned, &current));
    }

    #[test]
    fn preserve_builds_explicit_metadata_copy_without_replacement_payload() {
        let mut args = CpArgs::single("test/source/report.json", "test/target/report.json");
        args.preserve = true;
        args.fidelity.retention_mode = Some("GOVERNANCE".to_string());
        args.fidelity.retain_until = Some("2099-01-02T03:04:05Z".to_string());
        args.fidelity.legal_hold = Some("ON".to_string());

        let options =
            transfer_copy_options(&args, Some("source-v1".to_string()), None).expect("copy policy");
        assert_eq!(options.metadata_directive, Some(MetadataDirective::Copy));
        assert!(options.destination.attributes.is_none());
        assert_eq!(options.source.version_id.as_deref(), Some("source-v1"));
        assert!(options.destination.retention.is_some());
        assert_eq!(
            options.destination.legal_hold,
            Some(rc_core::LegalHoldStatus::On)
        );
    }

    #[test]
    fn storage_class_plan_rejects_multipart_without_mutation() {
        let plan = TransferPlan::build(
            vec![TransferCandidate {
                payload: CpOperation::RemoteToRemote {
                    source: RemotePath::new("local", "source", "large.bin"),
                    target: RemotePath::new("local", "target", "large.bin"),
                    source_info: Box::new(ObjectInfo::file(
                        "large.bin",
                        (MAX_SINGLE_COPY_SIZE + 1) as i64,
                    )),
                    encryption: None,
                },
                source: "local/source/large.bin".to_string(),
                target: "local/target/large.bin".to_string(),
                relative_path: "large.bin".to_string(),
                modified: None,
                size_bytes: Some(MAX_SINGLE_COPY_SIZE + 1),
            }],
            &TransferSelection::default(),
        );

        assert!(matches!(
            validate_storage_class_plan(&plan, Some("STANDARD")),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn storage_class_plan_rejects_cross_alias_multipart_uploads() {
        let plan = TransferPlan::build(
            vec![TransferCandidate {
                payload: CpOperation::RemoteToRemote {
                    source: RemotePath::new("alpha", "source", "medium.bin"),
                    target: RemotePath::new("beta", "target", "medium.bin"),
                    source_info: Box::new(ObjectInfo::file(
                        "medium.bin",
                        (MULTIPART_THRESHOLD + 1) as i64,
                    )),
                    encryption: None,
                },
                source: "alpha/source/medium.bin".to_string(),
                target: "beta/target/medium.bin".to_string(),
                relative_path: "medium.bin".to_string(),
                modified: None,
                size_bytes: Some(MULTIPART_THRESHOLD + 1),
            }],
            &TransferSelection::default(),
        );

        assert!(matches!(
            validate_storage_class_plan(&plan, Some("STANDARD")),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn piped_copy_preserves_source_content_type_unless_replaced() {
        let mut source = ObjectInfo::file("file.txt", 4);
        source.content_type = Some("text/plain".to_string());
        let args = CpArgs::single("alpha/source/file.txt", "beta/target/file.txt");

        let copied = piped_copy_write_options(&args, &source, None).expect("copy metadata");
        assert_eq!(
            copied
                .attributes
                .as_ref()
                .and_then(|value| value.content_type.as_deref()),
            Some("text/plain")
        );

        let mut replace = args;
        replace.metadata_directive = Some(MetadataDirectiveArg::Replace);
        let replaced = piped_copy_write_options(&replace, &source, None).expect("replace metadata");
        assert!(
            replaced
                .attributes
                .as_ref()
                .is_none_or(|value| value.content_type.is_none())
        );
    }

    #[test]
    fn piped_copy_preserves_source_user_metadata_unless_replaced() {
        let mut source = ObjectInfo::file("file.txt", 4);
        source.metadata = Some(HashMap::from([(
            "owner".to_string(),
            "storage".to_string(),
        )]));
        let args = CpArgs::single("alpha/source/file.txt", "beta/target/file.txt");

        let copied = piped_copy_write_options(&args, &source, None).expect("copy metadata");
        assert_eq!(
            copied
                .attributes
                .as_ref()
                .and_then(|value| value.user_metadata.get("owner"))
                .map(String::as_str),
            Some("storage")
        );

        let mut replace = args;
        replace.metadata_directive = Some(MetadataDirectiveArg::Replace);
        let replaced = piped_copy_write_options(&replace, &source, None).expect("replace metadata");
        assert!(
            replaced
                .attributes
                .as_ref()
                .is_none_or(|value| value.user_metadata.is_empty())
        );
    }

    #[test]
    fn get_alias_accepts_only_one_remote_source_and_local_target() {
        let remote = ParsedPath::Remote(RemotePath::new("local", "reports", "report.json"));
        let other_remote = ParsedPath::Remote(RemotePath::new("local", "reports", "other.json"));
        let local = ParsedPath::Local(PathBuf::from("./report.json"));

        assert_eq!(
            validate_alias_direction(TransferAlias::Get, std::slice::from_ref(&remote), &local),
            Ok(())
        );
        assert_eq!(
            validate_alias_direction(TransferAlias::Get, &[remote.clone(), other_remote], &local),
            Err("get requires exactly one remote source and one local target")
        );
        assert_eq!(
            validate_alias_direction(TransferAlias::Get, std::slice::from_ref(&local), &remote),
            Err("get requires exactly one remote source and one local target")
        );
    }

    #[test]
    fn put_alias_accepts_only_local_sources_and_remote_target() {
        let first_local = ParsedPath::Local(PathBuf::from("./january.csv"));
        let second_local = ParsedPath::Local(PathBuf::from("./february.csv"));
        let remote = ParsedPath::Remote(RemotePath::new("local", "reports", ""));

        assert_eq!(
            validate_alias_direction(
                TransferAlias::Put,
                &[first_local.clone(), second_local],
                &remote
            ),
            Ok(())
        );
        assert_eq!(
            validate_alias_direction(
                TransferAlias::Put,
                std::slice::from_ref(&remote),
                &first_local
            ),
            Err("put requires one or more local sources and one remote target")
        );
    }
}
