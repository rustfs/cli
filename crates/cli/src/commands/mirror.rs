//! mirror command - Synchronize trees between local filesystems and S3-compatible storage.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use clap::{Args, ValueEnum};
use jiff::Timestamp;
use rc_core::alias::RetryConfig;
use rc_core::{
    AliasManager, Error, ListOptions, ObjectAttributes, ObjectInfo, ObjectStore as _,
    ObjectWriteOptions, ParsedPath, RemotePath, TransferCandidate, TransferControls,
    TransferExecutor, TransferOutcomeState, TransferPlan, TransferReport, TransferSelection,
    TransferSummary, parse_path,
};
use rc_s3::S3Client;
use serde::Serialize;
use tokio::io::AsyncWriteExt as _;

use super::cp::{parse_age_cutoff, parse_byte_rate};
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const SOURCE_IDENTITY_METADATA_KEY: &str = "rc-source-etag";

/// How `rc mirror` decides that a destination object already matches the source.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum CompareMode {
    /// Skip when ETags match, or when size matches and destination metadata
    /// records the source ETag from a previous `rc mirror` copy.
    #[default]
    Auto,
    /// Skip only when destination and source ETags are identical.
    Etag,
    /// Skip when object sizes match, ignoring ETag differences.
    Size,
}

const MIRROR_AFTER_HELP: &str = "\
Examples:
  rc mirror ./site/ local/web/site/ --overwrite
  rc mirror local/archive/ ./restore/ --remove --dry-run
  rc mirror stage/data/ prod/data/ --include '**/*.json' --summary";

/// Synchronize objects between two directory-like roots.
#[derive(Args, Clone, Debug)]
#[command(after_help = MIRROR_AFTER_HELP)]
pub struct MirrorArgs {
    /// Source directory or remote bucket/prefix
    pub source: String,

    /// Destination directory or remote bucket/prefix
    pub target: String,

    /// Remove destination files that are absent from the selected source tree
    #[arg(long)]
    pub remove: bool,

    /// Overwrite changed destination files
    #[arg(long)]
    pub overwrite: bool,

    /// Include source-relative paths matching this glob (repeatable)
    #[arg(long)]
    pub include: Vec<String>,

    /// Exclude source-relative paths matching this glob; exclusions always win (repeatable)
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Select files modified more recently than this age (for example 1h or 7d)
    #[arg(long)]
    pub newer_than: Option<String>,

    /// Select files modified less recently than this age (for example 1h or 7d)
    #[arg(long)]
    pub older_than: Option<String>,

    /// Continue after per-file failures
    #[arg(long, visible_alias = "skip-errors")]
    pub continue_on_error: bool,

    /// Only show the deterministic plan without writing or deleting
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Maximum number of operations in flight across the command
    #[arg(short = 'P', long, visible_alias = "parallel", default_value_t = 4)]
    pub concurrency: usize,

    /// Aggregate transfer start rate in bytes per second (for example 10MiB/s)
    #[arg(long)]
    pub rate_limit: Option<String>,

    /// Maximum attempts for transient transfer failures
    #[arg(long, default_value_t = 3)]
    pub retry_attempts: u32,

    /// Initial transient retry backoff in milliseconds
    #[arg(long, default_value_t = 100)]
    pub retry_initial_backoff_ms: u64,

    /// Maximum transient retry backoff in milliseconds
    #[arg(long, default_value_t = 10_000)]
    pub retry_max_backoff_ms: u64,

    /// Print deterministic aggregate transfer counters
    #[arg(long)]
    pub summary: bool,

    /// How existing destination objects are compared before skipping a copy
    #[arg(long, value_enum, default_value_t = CompareMode::Auto)]
    pub compare: CompareMode,

    /// Suppress non-error mirror output (legacy command-local alias)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct MirrorSnapshot {
    size_bytes: Option<u64>,
    modified: Option<Timestamp>,
    etag: Option<String>,
    // Preserved source ETag from `x-amz-meta-rc-source-etag`. ListObjects does
    // not return user metadata, so this is filled from HeadObject during Auto
    // compare and must not participate in destination race detection.
    identity_etag: Option<String>,
}

impl MirrorSnapshot {
    fn content_matches(&self, other: &Self) -> bool {
        self.size_bytes == other.size_bytes
            && self.modified == other.modified
            && self.etag == other.etag
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MirrorLocation {
    Local(PathBuf),
    Remote(RemotePath),
}

impl std::fmt::Display for MirrorLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(path) => write!(formatter, "{}", path.display()),
            Self::Remote(path) => write!(formatter, "{path}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MirrorEntry {
    relative_path: String,
    location: MirrorLocation,
    snapshot: MirrorSnapshot,
}

#[derive(Debug, Default)]
struct MirrorManifest {
    entries: BTreeMap<String, MirrorEntry>,
    // A skipped source symlink protects the same destination path (and descendants) from removal.
    protected_paths: Vec<String>,
    ignored: usize,
}

#[derive(Debug, Clone)]
enum MirrorEndpointSpec {
    Local(PathBuf),
    Remote(RemotePath),
}

impl MirrorEndpointSpec {
    fn location_for(&self, relative_path: &str) -> rc_core::Result<MirrorLocation> {
        let relative_path = normalize_relative_path(relative_path)?;
        match self {
            Self::Local(root) => {
                let mut target = root.clone();
                for component in relative_path.split('/') {
                    target.push(component);
                }
                Ok(MirrorLocation::Local(target))
            }
            Self::Remote(root) => {
                let prefix = normalized_remote_root_prefix(&root.key)?;
                Ok(MirrorLocation::Remote(RemotePath::new(
                    &root.alias,
                    &root.bucket,
                    format!("{prefix}{relative_path}"),
                )))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MirrorCopyOperation {
    relative_path: String,
    source: MirrorEntry,
    target: MirrorLocation,
    target_before: Option<MirrorEntry>,
}

#[derive(Debug, Clone)]
struct MirrorRemoveOperation {
    relative_path: String,
    target: MirrorEntry,
}

#[async_trait::async_trait]
trait MirrorIo: Send + Sync + 'static {
    async fn copy(&self, operation: MirrorCopyOperation) -> rc_core::Result<u64>;
    async fn remove(&self, operation: MirrorRemoveOperation) -> rc_core::Result<u64>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteWriteCondition {
    IfAbsent,
    IfMatch(String),
}

#[async_trait::async_trait]
trait MirrorRemoteTransfer: Send + Sync {
    async fn mirror_head(&self, path: &RemotePath) -> rc_core::Result<ObjectInfo>;
    async fn mirror_download(&self, path: &RemotePath, destination: &Path) -> rc_core::Result<u64>;
    async fn mirror_upload(
        &self,
        path: &RemotePath,
        source: &Path,
        content_type: Option<&str>,
        condition: RemoteWriteCondition,
        identity_etag: Option<&str>,
    ) -> rc_core::Result<ObjectInfo>;
}

#[async_trait::async_trait]
impl MirrorRemoteTransfer for S3Client {
    async fn mirror_head(&self, path: &RemotePath) -> rc_core::Result<ObjectInfo> {
        rc_core::ObjectStore::head_object(self, path).await
    }

    async fn mirror_download(&self, path: &RemotePath, destination: &Path) -> rc_core::Result<u64> {
        self.download_object_to_path(path, destination, |_, _| {})
            .await
    }

    async fn mirror_upload(
        &self,
        path: &RemotePath,
        source: &Path,
        content_type: Option<&str>,
        condition: RemoteWriteCondition,
        identity_etag: Option<&str>,
    ) -> rc_core::Result<ObjectInfo> {
        let mut user_metadata = HashMap::new();
        if let Some(identity_etag) = identity_etag {
            user_metadata.insert(
                SOURCE_IDENTITY_METADATA_KEY.to_string(),
                identity_etag.to_string(),
            );
        }
        let options = ObjectWriteOptions {
            attributes: Some(ObjectAttributes {
                content_type: content_type.map(ToString::to_string),
                user_metadata,
                ..ObjectAttributes::default()
            }),
            ..ObjectWriteOptions::default()
        };
        match condition {
            RemoteWriteCondition::IfAbsent => {
                self.put_object_from_path_if_absent_with_options(path, source, &options, |_| {})
                    .await
            }
            RemoteWriteCondition::IfMatch(etag) => {
                self.put_object_from_path_if_match_with_options(
                    path,
                    source,
                    &options,
                    &etag,
                    |_| {},
                )
                .await
            }
        }
    }
}

struct MirrorOperationReports {
    copy: TransferReport<MirrorCopyOperation>,
    remove: Option<TransferReport<MirrorRemoveOperation>>,
    blocked_removals: usize,
}

#[derive(Clone)]
enum RuntimeEndpoint {
    Local {
        root: PathBuf,
    },
    Remote {
        root: RemotePath,
        client: Arc<S3Client>,
    },
}

impl RuntimeEndpoint {
    fn spec(&self) -> MirrorEndpointSpec {
        match self {
            Self::Local { root } => MirrorEndpointSpec::Local(root.clone()),
            Self::Remote { root, .. } => MirrorEndpointSpec::Remote(root.clone()),
        }
    }

    async fn current_entry(&self, relative_path: &str) -> rc_core::Result<Option<MirrorEntry>> {
        let location = self.spec().location_for(relative_path)?;
        match (&location, self) {
            (MirrorLocation::Local(path), Self::Local { root }) => {
                inspect_local_entry(root, relative_path, path).await
            }
            (MirrorLocation::Remote(path), Self::Remote { client, .. }) => {
                match client.head_object(path).await {
                    Ok(info) => Ok(Some(MirrorEntry {
                        relative_path: relative_path.to_string(),
                        location,
                        snapshot: snapshot_from_object(&info)?,
                    })),
                    Err(Error::NotFound(_)) => Ok(None),
                    Err(error) => Err(error),
                }
            }
            _ => Err(Error::General(
                "Mirror runtime endpoint does not match its planned location".to_string(),
            )),
        }
    }
}

#[derive(Clone)]
struct LiveMirrorIo {
    source: RuntimeEndpoint,
    target: RuntimeEndpoint,
    compare: CompareMode,
}

#[async_trait::async_trait]
impl MirrorIo for LiveMirrorIo {
    async fn copy(&self, operation: MirrorCopyOperation) -> rc_core::Result<u64> {
        match (&operation.source.location, &operation.target) {
            (MirrorLocation::Local(source), MirrorLocation::Remote(target)) => {
                self.copy_local_to_remote(source, target, &operation).await
            }
            (MirrorLocation::Remote(source), MirrorLocation::Local(target)) => {
                self.copy_remote_to_local(source, target, &operation).await
            }
            (MirrorLocation::Remote(source), MirrorLocation::Remote(target)) => {
                self.copy_remote_to_remote(source, target, &operation).await
            }
            (MirrorLocation::Local(_), MirrorLocation::Local(_)) => Err(Error::UnsupportedFeature(
                "Local-to-local mirror is out of scope; use a filesystem synchronization tool"
                    .to_string(),
            )),
        }
    }

    async fn remove(&self, operation: MirrorRemoveOperation) -> rc_core::Result<u64> {
        let Some(current) = self.target.current_entry(&operation.relative_path).await? else {
            // A previous attempt may have removed the entry before its response was lost.
            return Ok(0);
        };
        if !current.snapshot.content_matches(&operation.target.snapshot) {
            return Err(Error::Conflict(format!(
                "Destination changed before removal: {}",
                operation.relative_path
            )));
        }

        match (&current.location, &self.target) {
            (MirrorLocation::Local(path), RuntimeEndpoint::Local { root }) => {
                let safe_path = secure_local_path(root, &operation.relative_path, false).await?;
                if &safe_path != path {
                    return Err(Error::InvalidPath(format!(
                        "Removal target escaped mirror root: {}",
                        path.display()
                    )));
                }
                tokio::fs::remove_file(path).await?;
                Ok(0)
            }
            (MirrorLocation::Remote(path), RuntimeEndpoint::Remote { client, .. }) => {
                let etag = current.snapshot.etag.as_deref().ok_or_else(|| {
                    Error::Conflict(format!(
                        "Refusing to remove {} because its ETag is unavailable",
                        operation.relative_path
                    ))
                })?;
                client.delete_object_if_match(path, etag).await?;
                Ok(0)
            }
            _ => Err(Error::General(
                "Mirror removal target does not match the runtime endpoint".to_string(),
            )),
        }
    }
}

impl LiveMirrorIo {
    async fn copy_local_to_remote(
        &self,
        source_path: &Path,
        target_path: &RemotePath,
        operation: &MirrorCopyOperation,
    ) -> rc_core::Result<u64> {
        validate_local_entry(&operation.source)?;
        let staged = stage_local_source(source_path).await?;
        async {
            validate_local_entry(&operation.source)?;
            let staged_size = tokio::fs::metadata(&staged).await?.len();
            if operation
                .source
                .snapshot
                .size_bytes
                .is_some_and(|expected| expected != staged_size)
            {
                return Err(Error::Conflict(format!(
                    "Source changed during mirror: {}",
                    operation.relative_path
                )));
            }
            match self.target_disposition(operation).await? {
                TargetDisposition::AlreadyComplete => {
                    return Ok(operation.source.snapshot.size_bytes.unwrap_or_default());
                }
                TargetDisposition::Ready => {}
            }

            let RuntimeEndpoint::Remote { client, .. } = &self.target else {
                return Err(Error::General(
                    "Remote mirror target client is unavailable".to_string(),
                ));
            };
            let content_type = mime_guess::from_path(source_path)
                .first()
                .map(|value| value.essence_str().to_string());
            let size = operation.source.snapshot.size_bytes.unwrap_or_default();
            let condition = remote_write_condition(operation)?;
            let info = client
                .mirror_upload(
                    target_path,
                    &staged,
                    content_type.as_deref(),
                    condition,
                    None,
                )
                .await?;
            Ok(object_size(&info).unwrap_or(size))
        }
        .await
    }

    async fn copy_remote_to_remote(
        &self,
        source_path: &RemotePath,
        target_path: &RemotePath,
        operation: &MirrorCopyOperation,
    ) -> rc_core::Result<u64> {
        let RuntimeEndpoint::Remote {
            client: source_client,
            ..
        } = &self.source
        else {
            return Err(Error::General(
                "Remote mirror source client is unavailable".to_string(),
            ));
        };
        let RuntimeEndpoint::Remote {
            client: target_client,
            ..
        } = &self.target
        else {
            return Err(Error::General(
                "Remote mirror target client is unavailable".to_string(),
            ));
        };

        match self.target_disposition(operation).await? {
            TargetDisposition::AlreadyComplete => {
                return Ok(operation.source.snapshot.size_bytes.unwrap_or_default());
            }
            TargetDisposition::Ready => {}
        }
        transfer_remote_to_remote(
            source_client.as_ref(),
            target_client.as_ref(),
            source_path,
            target_path,
            operation,
            remote_write_condition(operation)?,
        )
        .await
    }

    async fn copy_remote_to_local(
        &self,
        source_path: &RemotePath,
        target_path: &Path,
        operation: &MirrorCopyOperation,
    ) -> rc_core::Result<u64> {
        let RuntimeEndpoint::Remote {
            client: source_client,
            ..
        } = &self.source
        else {
            return Err(Error::General(
                "Remote mirror source client is unavailable".to_string(),
            ));
        };
        let RuntimeEndpoint::Local { root } = &self.target else {
            return Err(Error::General(
                "Local mirror target root is unavailable".to_string(),
            ));
        };

        let before = source_client.head_object(source_path).await?;
        ensure_snapshot_matches(&operation.source, &before)?;
        match self.target_disposition(operation).await? {
            TargetDisposition::AlreadyComplete => {
                return Ok(operation.source.snapshot.size_bytes.unwrap_or_default());
            }
            TargetDisposition::Ready => {}
        }

        let destination = secure_local_path(root, &operation.relative_path, true).await?;
        if destination != target_path {
            return Err(Error::InvalidPath(format!(
                "Download target escaped mirror root: {}",
                target_path.display()
            )));
        }
        let staging = mirror_staging_path(target_path)?;
        let downloaded = source_client
            .download_object_to_path(source_path, &staging, |_, _| {})
            .await;
        let validation = match downloaded {
            Ok(bytes) => {
                if operation
                    .source
                    .snapshot
                    .size_bytes
                    .is_some_and(|expected| expected != bytes)
                {
                    Err(Error::Conflict(format!(
                        "Source changed during mirror: {}",
                        operation.relative_path
                    )))
                } else {
                    let after = source_client.head_object(source_path).await;
                    match after {
                        Ok(info) => {
                            ensure_snapshot_matches(&operation.source, &info).map(|()| bytes)
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            Err(error) => Err(error),
        };
        if validation.is_ok() {
            match self.target_disposition(operation).await {
                Ok(TargetDisposition::Ready) => {}
                Ok(TargetDisposition::AlreadyComplete) => {
                    return Ok(operation.source.snapshot.size_bytes.unwrap_or_default());
                }
                Err(error) => {
                    return Err(error);
                }
            }
        }
        finish_staged_download(
            staging,
            target_path,
            validation,
            operation.source.snapshot.modified,
            operation.target_before.is_some(),
        )
        .await
    }

    async fn target_disposition(
        &self,
        operation: &MirrorCopyOperation,
    ) -> rc_core::Result<TargetDisposition> {
        let current = self.target.current_entry(&operation.relative_path).await?;
        if let Some(current) = &current
            && source_matches_target(&operation.source, current, self.compare)
        {
            return Ok(TargetDisposition::AlreadyComplete);
        }

        match (&operation.target_before, current) {
            (None, None) => Ok(TargetDisposition::Ready),
            (Some(expected), Some(current))
                if expected.snapshot.content_matches(&current.snapshot) =>
            {
                Ok(TargetDisposition::Ready)
            }
            _ => Err(Error::Conflict(format!(
                "Destination changed after mirror planning: {}",
                operation.relative_path
            ))),
        }
    }
}

enum TargetDisposition {
    Ready,
    AlreadyComplete,
}

fn remote_write_condition(
    operation: &MirrorCopyOperation,
) -> rc_core::Result<RemoteWriteCondition> {
    match &operation.target_before {
        None => Ok(RemoteWriteCondition::IfAbsent),
        Some(target) => target
            .snapshot
            .etag
            .as_ref()
            .map(|etag| RemoteWriteCondition::IfMatch(etag.clone()))
            .ok_or_else(|| {
                Error::Conflict(format!(
                    "Refusing to overwrite {} because its ETag is unavailable",
                    operation.relative_path
                ))
            }),
    }
}

async fn transfer_remote_to_remote<S, T>(
    source_client: &S,
    target_client: &T,
    source_path: &RemotePath,
    target_path: &RemotePath,
    operation: &MirrorCopyOperation,
    condition: RemoteWriteCondition,
) -> rc_core::Result<u64>
where
    S: MirrorRemoteTransfer,
    T: MirrorRemoteTransfer,
{
    let before = source_client.mirror_head(source_path).await?;
    ensure_snapshot_matches(&operation.source, &before)?;
    let staging = temporary_mirror_path("remote-source")?;
    async {
        let bytes = source_client.mirror_download(source_path, &staging).await?;
        if operation
            .source
            .snapshot
            .size_bytes
            .is_some_and(|expected| expected != bytes)
        {
            return Err(Error::Conflict(format!(
                "Source changed during mirror: {}",
                operation.relative_path
            )));
        }
        let after = source_client.mirror_head(source_path).await?;
        ensure_snapshot_matches(&operation.source, &after)?;
        let content_type = after
            .content_type
            .as_deref()
            .or(before.content_type.as_deref());
        let info = target_client
            .mirror_upload(
                target_path,
                &staging,
                content_type,
                condition,
                operation.source.snapshot.etag.as_deref(),
            )
            .await?;
        Ok(object_size(&info)
            .or(operation.source.snapshot.size_bytes)
            .unwrap_or(bytes))
    }
    .await
}

/// Execute the mirror command.
pub async fn execute(args: MirrorArgs, mut output_config: OutputConfig) -> ExitCode {
    output_config.quiet |= args.quiet;
    let formatter = Formatter::new(output_config);

    let selection = match build_selection(&args, Timestamp::now()) {
        Ok(selection) => selection,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };
    let controls = match build_controls(&args) {
        Ok(controls) => controls,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
            );
        }
    };
    let source = match parse_mirror_path(&args.source, &alias_manager) {
        Ok(path) => path,
        Err(error) => {
            return formatter.fail(
                ExitCode::UsageError,
                &format!("Invalid source path: {error}"),
            );
        }
    };
    let target = match parse_mirror_path(&args.target, &alias_manager) {
        Ok(path) => path,
        Err(error) => {
            return formatter.fail(
                ExitCode::UsageError,
                &format!("Invalid target path: {error}"),
            );
        }
    };
    if matches!(
        (&source, &target),
        (ParsedPath::Local(_), ParsedPath::Local(_))
    ) {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "Local-to-local mirror is out of scope; use a filesystem synchronization tool",
        );
    }
    if let Err(error) = validate_remote_overlap(&source, &target, &alias_manager) {
        return formatter.fail(exit_code_for_error(&error), &error.to_string());
    }

    let (source_runtime, source_manifest) =
        match prepare_endpoint(&source, MissingRootPolicy::Error, &alias_manager).await {
            Ok(prepared) => prepared,
            Err(error) => {
                return formatter.fail(
                    exit_code_for_error(&error),
                    &format!("Failed to enumerate mirror source: {error}"),
                );
            }
        };
    let (target_runtime, mut target_manifest) =
        match prepare_endpoint(&target, MissingRootPolicy::Empty, &alias_manager).await {
            Ok(prepared) => prepared,
            Err(error) => {
                return formatter.fail(
                    exit_code_for_error(&error),
                    &format!("Failed to enumerate mirror destination: {error}"),
                );
            }
        };
    if let Err(error) = enrich_destination_identity(
        &source_manifest,
        &mut target_manifest,
        &target_runtime,
        args.compare,
    )
    .await
    {
        return formatter.fail(
            exit_code_for_error(&error),
            &format!("Failed to inspect destination identity: {error}"),
        );
    }
    let copy_plan = match build_copy_plan(
        &source_manifest,
        &target_manifest,
        &target_runtime.spec(),
        &selection,
        args.overwrite,
        args.compare,
    ) {
        Ok(plan) => plan,
        Err(error) => return formatter.fail(exit_code_for_error(&error), &error.to_string()),
    };
    let remove_plan = if args.remove {
        match build_remove_plan(&source_manifest, &target_manifest, &selection) {
            Ok(plan) => plan,
            Err(error) => return formatter.fail(exit_code_for_error(&error), &error.to_string()),
        }
    } else {
        empty_transfer_plan()
    };

    if args.dry_run {
        output_dry_run(&formatter, &args, &copy_plan, &remove_plan);
        return ExitCode::Success;
    }

    let io = Arc::new(LiveMirrorIo {
        source: source_runtime,
        target: target_runtime,
        compare: args.compare,
    });
    let reports =
        match execute_operation_plans(io, copy_plan, remove_plan, controls, args.remove).await {
            Ok(reports) => reports,
            Err(error) => return formatter.fail(ExitCode::UsageError, &error.to_string()),
        };
    output_reports(&formatter, &args, &reports);

    reports
        .copy
        .first_failure()
        .or_else(|| {
            reports
                .remove
                .as_ref()
                .and_then(TransferReport::first_failure)
        })
        .map_or(ExitCode::Success, exit_code_for_error)
}

fn build_selection(args: &MirrorArgs, now: Timestamp) -> Result<TransferSelection, String> {
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
    TransferSelection::new(&args.include, &args.exclude, newer_than, older_than, None)
        .map_err(|error| error.to_string())
}

fn build_controls(args: &MirrorArgs) -> Result<TransferControls, String> {
    let bytes_per_second = args
        .rate_limit
        .as_deref()
        .map(parse_byte_rate)
        .transpose()?;
    let controls = TransferControls {
        concurrency: args.concurrency,
        bytes_per_second,
        retry: RetryConfig {
            max_attempts: args.retry_attempts,
            initial_backoff_ms: args.retry_initial_backoff_ms,
            max_backoff_ms: args.retry_max_backoff_ms,
        },
        continue_on_error: args.continue_on_error,
    };
    controls.validate().map_err(|error| error.to_string())?;
    Ok(controls)
}

fn parse_mirror_path(raw: &str, alias_manager: &AliasManager) -> rc_core::Result<ParsedPath> {
    let parsed = match parse_path(raw) {
        Ok(parsed) => parsed,
        Err(_) if !raw.is_empty() && Path::new(raw).exists() => {
            return Ok(ParsedPath::Local(PathBuf::from(raw)));
        }
        Err(error) => return Err(error),
    };
    let ParsedPath::Remote(remote) = &parsed else {
        return Ok(parsed);
    };
    if matches!(alias_manager.exists(&remote.alias), Ok(true)) {
        return Ok(parsed);
    }
    if Path::new(raw).exists() {
        return Ok(ParsedPath::Local(PathBuf::from(raw)));
    }
    Ok(parsed)
}

fn validate_remote_overlap(
    source: &ParsedPath,
    target: &ParsedPath,
    alias_manager: &AliasManager,
) -> rc_core::Result<()> {
    let (ParsedPath::Remote(source), ParsedPath::Remote(target)) = (source, target) else {
        return Ok(());
    };
    let source_alias = alias_manager
        .get(&source.alias)
        .map_err(|_| Error::AliasNotFound(source.alias.clone()))?;
    let target_alias = alias_manager
        .get(&target.alias)
        .map_err(|_| Error::AliasNotFound(target.alias.clone()))?;
    if mirror_locations_overlap(
        source,
        target,
        &source_alias.endpoint,
        &target_alias.endpoint,
    )? {
        return Err(Error::InvalidPath(
            "Mirror source and destination roots must not overlap".to_string(),
        ));
    }
    Ok(())
}

async fn prepare_endpoint(
    parsed: &ParsedPath,
    missing_root: MissingRootPolicy,
    alias_manager: &AliasManager,
) -> rc_core::Result<(RuntimeEndpoint, MirrorManifest)> {
    match parsed {
        ParsedPath::Local(root) => {
            let manifest = enumerate_local_manifest(root, missing_root)?;
            Ok((RuntimeEndpoint::Local { root: root.clone() }, manifest))
        }
        ParsedPath::Remote(root) => {
            let alias = alias_manager
                .get(&root.alias)
                .map_err(|_| Error::AliasNotFound(root.alias.clone()))?;
            let planning_client = S3Client::new(alias.clone()).await?;
            let manifest = enumerate_remote_manifest(&planning_client, root).await?;
            let mut execution_alias = alias;
            execution_alias.retry = Some(RetryConfig {
                max_attempts: 1,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
            });
            let execution_client = Arc::new(S3Client::new(execution_alias).await?);
            Ok((
                RuntimeEndpoint::Remote {
                    root: root.clone(),
                    client: execution_client,
                },
                manifest,
            ))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MissingRootPolicy {
    Error,
    Empty,
}

fn enumerate_local_manifest(
    root: &Path,
    missing_root: MissingRootPolicy,
) -> rc_core::Result<MirrorManifest> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(missing_root, MissingRootPolicy::Empty) =>
        {
            return Ok(MirrorManifest::default());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::NotFound(format!(
                "Local mirror root not found: {}",
                root.display()
            )));
        }
        Err(error) => return Err(Error::Io(error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(Error::InvalidPath(format!(
            "Local mirror root must not be a symbolic link: {}",
            root.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(Error::InvalidPath(format!(
            "Local mirror root must be a directory: {}",
            root.display()
        )));
    }

    let mut manifest = MirrorManifest::default();
    enumerate_local_directory(root, root, &mut manifest)?;
    manifest.protected_paths.sort();
    manifest.protected_paths.dedup();
    Ok(manifest)
}

fn enumerate_local_directory(
    directory: &Path,
    root: &Path,
    manifest: &mut MirrorManifest,
) -> rc_core::Result<()> {
    let mut entries = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = local_relative_path(root, &path)?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            manifest.protected_paths.push(relative);
            manifest.ignored += 1;
            continue;
        }
        if file_type.is_dir() {
            enumerate_local_directory(&path, root, manifest)?;
            continue;
        }
        if !file_type.is_file() {
            manifest.protected_paths.push(relative);
            manifest.ignored += 1;
            continue;
        }
        let metadata = entry.metadata()?;
        insert_manifest_entry(
            &mut manifest.entries,
            MirrorEntry {
                relative_path: relative,
                location: MirrorLocation::Local(path),
                snapshot: snapshot_from_metadata(&metadata),
            },
        )?;
    }
    Ok(())
}

async fn enumerate_remote_manifest(
    client: &S3Client,
    root: &RemotePath,
) -> rc_core::Result<MirrorManifest> {
    let prefix = normalized_remote_root_prefix(&root.key)?;
    let list_root = RemotePath::new(&root.alias, &root.bucket, &prefix);
    let mut continuation_token = None;
    let mut manifest = MirrorManifest::default();
    loop {
        let result = client
            .list_objects(
                &list_root,
                ListOptions {
                    recursive: true,
                    max_keys: Some(1_000),
                    continuation_token: continuation_token.clone(),
                    ..ListOptions::default()
                },
            )
            .await?;
        for object in result
            .items
            .into_iter()
            .filter(|object| !object.is_dir && !object.key.ends_with('/'))
        {
            let raw_relative = object.key.strip_prefix(&prefix).ok_or_else(|| {
                Error::InvalidPath(format!(
                    "Remote key '{}' is outside mirror root '{}'",
                    object.key, prefix
                ))
            })?;
            if raw_relative.is_empty() {
                continue;
            }
            let relative_path = normalize_relative_path(raw_relative)?;
            let snapshot = snapshot_from_object(&object)?;
            insert_manifest_entry(
                &mut manifest.entries,
                MirrorEntry {
                    relative_path,
                    location: MirrorLocation::Remote(RemotePath::new(
                        &root.alias,
                        &root.bucket,
                        object.key,
                    )),
                    snapshot,
                },
            )?;
        }
        if !result.truncated {
            break;
        }
        continuation_token = result.continuation_token;
    }
    Ok(manifest)
}

fn insert_manifest_entry(
    entries: &mut BTreeMap<String, MirrorEntry>,
    entry: MirrorEntry,
) -> rc_core::Result<()> {
    let relative_path = entry.relative_path.clone();
    if entries.insert(relative_path.clone(), entry).is_some() {
        return Err(Error::Conflict(format!(
            "Multiple mirror entries normalize to '{}'",
            relative_path
        )));
    }
    Ok(())
}

fn build_copy_plan(
    source: &MirrorManifest,
    target: &MirrorManifest,
    target_endpoint: &MirrorEndpointSpec,
    selection: &TransferSelection,
    overwrite: bool,
    compare: CompareMode,
) -> rc_core::Result<TransferPlan<MirrorCopyOperation>> {
    let mut candidates = Vec::with_capacity(source.entries.len());
    for entry in source.entries.values() {
        if path_is_protected(&entry.relative_path, &target.protected_paths) {
            return Err(Error::Conflict(format!(
                "Destination path crosses a symbolic link or special file: {}",
                entry.relative_path
            )));
        }
        let target_before = target.entries.get(&entry.relative_path).cloned();
        let target_location = target_before.as_ref().map_or_else(
            || target_endpoint.location_for(&entry.relative_path),
            |target| Ok(target.location.clone()),
        )?;
        candidates.push(TransferCandidate {
            payload: MirrorCopyOperation {
                relative_path: entry.relative_path.clone(),
                source: entry.clone(),
                target: target_location.clone(),
                target_before,
            },
            source: entry.location.to_string(),
            target: target_location.to_string(),
            relative_path: entry.relative_path.clone(),
            modified: entry.snapshot.modified,
            size_bytes: entry.snapshot.size_bytes,
        });
    }

    let selected = TransferPlan::build(candidates, selection);
    let mut skipped_after_selection = 0usize;
    let mut items = Vec::new();
    for candidate in selected.items {
        let should_copy = match &candidate.payload.target_before {
            None => true,
            Some(target) if source_matches_target(&candidate.payload.source, target, compare) => {
                false
            }
            Some(_) => overwrite,
        };
        if should_copy {
            items.push(candidate);
        } else {
            skipped_after_selection += 1;
        }
    }
    let mut summary = selected.summary;
    summary.planned = items.len();
    summary.skipped = summary
        .skipped
        .saturating_add(skipped_after_selection)
        .saturating_add(source.ignored);
    Ok(TransferPlan { items, summary })
}

fn build_remove_plan(
    source: &MirrorManifest,
    target: &MirrorManifest,
    selection: &TransferSelection,
) -> rc_core::Result<TransferPlan<MirrorRemoveOperation>> {
    let mut protected = 0usize;
    let candidates = target
        .entries
        .values()
        .filter_map(|entry| {
            if source.entries.contains_key(&entry.relative_path) {
                return None;
            }
            if path_is_protected(&entry.relative_path, &source.protected_paths) {
                protected += 1;
                return None;
            }
            Some(TransferCandidate {
                payload: MirrorRemoveOperation {
                    relative_path: entry.relative_path.clone(),
                    target: entry.clone(),
                },
                source: entry.location.to_string(),
                target: entry.location.to_string(),
                relative_path: entry.relative_path.clone(),
                modified: entry.snapshot.modified,
                size_bytes: Some(0),
            })
        })
        .collect();
    let mut plan = TransferPlan::build(candidates, selection);
    plan.summary.skipped = plan
        .summary
        .skipped
        .saturating_add(protected)
        .saturating_add(target.ignored);
    Ok(plan)
}

async fn execute_operation_plans<I: MirrorIo>(
    io: Arc<I>,
    copy_plan: TransferPlan<MirrorCopyOperation>,
    remove_plan: TransferPlan<MirrorRemoveOperation>,
    controls: TransferControls,
    remove_enabled: bool,
) -> rc_core::Result<MirrorOperationReports> {
    let executor = TransferExecutor::new(controls)?;
    let copy = if copy_plan.items.is_empty() {
        report_without_execution(copy_plan)
    } else {
        executor
            .execute(copy_plan, {
                let io = Arc::clone(&io);
                move |candidate| {
                    let io = Arc::clone(&io);
                    async move { io.copy(candidate.payload).await }
                }
            })
            .await
    };
    let copies_succeeded = copy.summary.failed == 0 && copy.summary.cancelled == 0;
    let blocked_removals = if remove_enabled && !copies_succeeded {
        remove_plan.items.len()
    } else {
        0
    };
    let remove = if remove_enabled && copies_succeeded {
        Some(if remove_plan.items.is_empty() {
            report_without_execution(remove_plan)
        } else {
            executor
                .execute(remove_plan, move |candidate| {
                    let io = Arc::clone(&io);
                    async move { io.remove(candidate.payload).await }
                })
                .await
        })
    } else {
        None
    };
    Ok(MirrorOperationReports {
        copy,
        remove,
        blocked_removals,
    })
}

fn report_without_execution<T>(plan: TransferPlan<T>) -> TransferReport<T> {
    TransferReport {
        summary: plan.summary,
        outcomes: Vec::new(),
        was_cancelled: false,
    }
}

fn empty_transfer_plan<T>() -> TransferPlan<T> {
    TransferPlan {
        items: Vec::new(),
        summary: TransferSummary::default(),
    }
}

fn output_dry_run(
    formatter: &Formatter,
    args: &MirrorArgs,
    copy_plan: &TransferPlan<MirrorCopyOperation>,
    remove_plan: &TransferPlan<MirrorRemoveOperation>,
) {
    if formatter.is_json() {
        formatter.json(&MirrorOutput {
            source: args.source.clone(),
            target: args.target.clone(),
            copied: copy_plan.items.len(),
            removed: remove_plan.items.len(),
            skipped: copy_plan
                .summary
                .skipped
                .saturating_add(remove_plan.summary.skipped),
            errors: 0,
            dry_run: true,
        });
        return;
    }
    for candidate in &copy_plan.items {
        formatter.println(&format!(
            "Would copy: {} -> {}",
            formatter.style_file(&candidate.source),
            formatter.style_file(&candidate.target)
        ));
    }
    for candidate in &remove_plan.items {
        formatter.println(&format!(
            "Would remove: {}",
            formatter.style_file(&candidate.target)
        ));
    }
    if args.summary {
        formatter.println(&format!(
            "Summary: {} copies, {} removals, {} skipped",
            copy_plan.items.len(),
            remove_plan.items.len(),
            copy_plan
                .summary
                .skipped
                .saturating_add(remove_plan.summary.skipped)
        ));
    }
}

fn output_reports(formatter: &Formatter, args: &MirrorArgs, reports: &MirrorOperationReports) {
    if !formatter.is_json() {
        output_outcomes(formatter, "+", &reports.copy);
        if let Some(remove) = &reports.remove {
            output_outcomes(formatter, "-", remove);
        } else if reports.blocked_removals > 0 {
            formatter.error("Skipping removals because one or more copy operations failed");
        }
    }
    let removed = reports
        .remove
        .as_ref()
        .map_or(0, |report| report.summary.successful);
    let remove_failed = reports
        .remove
        .as_ref()
        .map_or(0, |report| report.summary.failed);
    let remove_skipped = reports
        .remove
        .as_ref()
        .map_or(reports.blocked_removals, |report| report.summary.skipped);
    let output = MirrorOutput {
        source: args.source.clone(),
        target: args.target.clone(),
        copied: reports.copy.summary.successful,
        removed,
        skipped: reports.copy.summary.skipped.saturating_add(remove_skipped),
        errors: reports.copy.summary.failed.saturating_add(remove_failed),
        dry_run: false,
    };
    if formatter.is_json() {
        formatter.json(&output);
    } else if args.summary {
        let remove_cancelled = reports
            .remove
            .as_ref()
            .map_or(0, |report| report.summary.cancelled);
        let cancelled = reports
            .copy
            .summary
            .cancelled
            .saturating_add(remove_cancelled);
        formatter.println(&format_human_summary(
            &output,
            cancelled,
            reports.copy.summary.transferred_bytes,
        ));
    }
}

fn format_human_summary(output: &MirrorOutput, cancelled: usize, transferred_bytes: u64) -> String {
    format!(
        "Summary: {} copied, {} removed, {} skipped, {} errors, {} cancelled, {} transferred",
        output.copied,
        output.removed,
        output.skipped,
        output.errors,
        cancelled,
        humansize::format_size(transferred_bytes, humansize::BINARY)
    )
}

fn output_outcomes<T>(formatter: &Formatter, marker: &str, report: &TransferReport<T>) {
    for outcome in &report.outcomes {
        match &outcome.state {
            TransferOutcomeState::Success { .. } => formatter.println(&format!(
                "{marker} {}",
                formatter.sanitize_text(&outcome.item.relative_path)
            )),
            TransferOutcomeState::Failed { error } => formatter.error_with_code(
                exit_code_for_error(error),
                &format!(
                    "Mirror operation failed for '{}': {error}",
                    formatter.sanitize_text(&outcome.item.relative_path)
                ),
            ),
            TransferOutcomeState::Cancelled { error } => {
                if let Some(error) = error {
                    formatter.warning(&format!(
                        "Cancelled mirror operation: {} ({error})",
                        formatter.sanitize_text(&outcome.item.relative_path)
                    ));
                } else {
                    formatter.warning(&format!(
                        "Cancelled before mirror operation: {}",
                        formatter.sanitize_text(&outcome.item.relative_path)
                    ));
                }
            }
        }
    }
}

fn normalize_relative_path(value: &str) -> rc_core::Result<String> {
    if value.starts_with(['/', '\\']) || value.contains('\\') {
        return Err(Error::InvalidPath(format!(
            "Mirror path must be relative and use '/' separators: {value}"
        )));
    }
    let mut normalized = Vec::new();
    for component in value.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(Error::InvalidPath(
                "Mirror paths must not contain traversal components".to_string(),
            ));
        }
        validate_portable_component(component)?;
        normalized.push(component);
    }
    if normalized.is_empty() {
        return Err(Error::InvalidPath(
            "Mirror path does not contain a file name".to_string(),
        ));
    }
    Ok(normalized.join("/"))
}

fn validate_portable_component(component: &str) -> rc_core::Result<()> {
    if component.chars().any(|character| {
        character.is_control() || matches!(character, ':' | '<' | '>' | '"' | '|' | '?' | '*')
    }) || component.ends_with(['.', ' '])
    {
        return Err(Error::InvalidPath(format!(
            "Mirror path component is not portable: {component}"
        )));
    }
    let stem = component.split('.').next().unwrap_or_default();
    let stem = stem.to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err(Error::InvalidPath(format!(
            "Mirror path uses a reserved device name: {component}"
        )));
    }
    Ok(())
}

fn local_relative_path(root: &Path, path: &Path) -> rc_core::Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| Error::InvalidPath(error.to_string()))?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(Error::InvalidPath(format!(
                "Local mirror entry is not relative to its root: {}",
                path.display()
            )));
        };
        let component = component.to_str().ok_or_else(|| {
            Error::InvalidPath(format!(
                "Local mirror entry is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        components.push(component);
    }
    normalize_relative_path(&components.join("/"))
}

fn normalized_remote_root_prefix(key: &str) -> rc_core::Result<String> {
    if key.starts_with('/') {
        return Err(Error::InvalidPath(
            "Remote mirror roots must not start with '/'".to_string(),
        ));
    }
    let key = key.trim_end_matches('/');
    if key.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{}/", normalize_relative_path(key)?))
}

fn snapshot_from_metadata(metadata: &std::fs::Metadata) -> MirrorSnapshot {
    MirrorSnapshot {
        size_bytes: Some(metadata.len()),
        modified: metadata
            .modified()
            .ok()
            .and_then(|value| value.try_into().ok()),
        etag: None,
        identity_etag: None,
    }
}

fn snapshot_from_object(object: &ObjectInfo) -> rc_core::Result<MirrorSnapshot> {
    let size_bytes = object
        .size_bytes
        .map(|size| {
            u64::try_from(size).map_err(|_| {
                Error::Network(format!(
                    "S3 returned a negative object size for '{}'",
                    object.key
                ))
            })
        })
        .transpose()?;
    Ok(MirrorSnapshot {
        size_bytes,
        modified: object.last_modified,
        etag: object.etag.clone(),
        identity_etag: identity_etag_from_metadata(object.metadata.as_ref()),
    })
}

fn identity_etag_from_metadata(metadata: Option<&HashMap<String, String>>) -> Option<String> {
    metadata.and_then(|metadata| {
        metadata.iter().find_map(|(key, value)| {
            let normalized = key.to_ascii_lowercase();
            let key = normalized
                .strip_prefix("x-amz-meta-")
                .unwrap_or(normalized.as_str());
            (key == SOURCE_IDENTITY_METADATA_KEY && !value.is_empty()).then(|| value.clone())
        })
    })
}

fn destination_needs_identity_lookup(
    source: &MirrorEntry,
    target: &MirrorEntry,
    compare: CompareMode,
) -> bool {
    if !matches!(compare, CompareMode::Auto) {
        return false;
    }
    if source.snapshot.size_bytes.is_none()
        || source.snapshot.size_bytes != target.snapshot.size_bytes
    {
        return false;
    }
    let Some(source_etag) = source.snapshot.etag.as_ref() else {
        return false;
    };
    if target.snapshot.etag.as_ref() == Some(source_etag) {
        return false;
    }
    if target.snapshot.identity_etag.is_some() {
        return false;
    }
    matches!(
        (&source.location, &target.location),
        (MirrorLocation::Remote(_), MirrorLocation::Remote(_))
    )
}

fn object_size(object: &ObjectInfo) -> Option<u64> {
    object.size_bytes.and_then(|size| u64::try_from(size).ok())
}

fn source_matches_target(source: &MirrorEntry, target: &MirrorEntry, compare: CompareMode) -> bool {
    let (Some(source_size), Some(target_size)) =
        (source.snapshot.size_bytes, target.snapshot.size_bytes)
    else {
        return false;
    };
    if source_size != target_size {
        return false;
    }
    match compare {
        CompareMode::Size => true,
        CompareMode::Etag => {
            source.snapshot.etag.is_some() && source.snapshot.etag == target.snapshot.etag
        }
        CompareMode::Auto => {
            if source.snapshot.etag.is_some() && source.snapshot.etag == target.snapshot.etag {
                return true;
            }
            if source
                .snapshot
                .etag
                .as_ref()
                .zip(target.snapshot.identity_etag.as_ref())
                .is_some_and(|(source_etag, identity_etag)| source_etag == identity_etag)
            {
                return true;
            }
            match (&source.location, &target.location) {
                (MirrorLocation::Remote(_), MirrorLocation::Remote(_)) => false,
                (MirrorLocation::Local(_), MirrorLocation::Remote(_)) => {
                    match (source.snapshot.modified, target.snapshot.modified) {
                        (Some(source_modified), Some(target_modified)) => {
                            target_modified >= source_modified
                        }
                        _ => false,
                    }
                }
                _ => {
                    source.snapshot.modified.is_some()
                        && source.snapshot.modified == target.snapshot.modified
                }
            }
        }
    }
}

async fn enrich_destination_identity(
    source: &MirrorManifest,
    target: &mut MirrorManifest,
    target_runtime: &RuntimeEndpoint,
    compare: CompareMode,
) -> rc_core::Result<()> {
    let RuntimeEndpoint::Remote { client, .. } = target_runtime else {
        return Ok(());
    };
    if !matches!(compare, CompareMode::Auto) {
        return Ok(());
    }
    for (relative_path, source_entry) in &source.entries {
        let Some(target_entry) = target.entries.get(relative_path) else {
            continue;
        };
        if !destination_needs_identity_lookup(source_entry, target_entry, compare) {
            continue;
        }
        let MirrorLocation::Remote(path) = &target_entry.location else {
            continue;
        };
        let path = path.clone();
        match client.head_object(&path).await {
            Ok(info) => {
                if let Some(target_entry) = target.entries.get_mut(relative_path) {
                    target_entry.snapshot.identity_etag =
                        identity_etag_from_metadata(info.metadata.as_ref());
                }
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn path_is_protected(relative_path: &str, protected_paths: &[String]) -> bool {
    protected_paths.iter().any(|protected| {
        relative_path == protected
            || relative_path
                .strip_prefix(protected)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn validate_local_entry(entry: &MirrorEntry) -> rc_core::Result<()> {
    let MirrorLocation::Local(path) = &entry.location else {
        return Err(Error::General(
            "Expected a local mirror source entry".to_string(),
        ));
    };
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Conflict(format!(
            "Source changed during mirror: {}",
            entry.relative_path
        )));
    }
    if snapshot_from_metadata(&metadata) != entry.snapshot {
        return Err(Error::Conflict(format!(
            "Source changed during mirror: {}",
            entry.relative_path
        )));
    }
    Ok(())
}

fn ensure_snapshot_matches(entry: &MirrorEntry, object: &ObjectInfo) -> rc_core::Result<()> {
    let current = snapshot_from_object(object)?;
    if current.size_bytes != entry.snapshot.size_bytes
        || current.modified != entry.snapshot.modified
        || current.etag != entry.snapshot.etag
    {
        return Err(Error::Conflict(format!(
            "Source changed during mirror: {}",
            entry.relative_path
        )));
    }
    Ok(())
}

async fn stage_local_source(source: &Path) -> rc_core::Result<tempfile::TempPath> {
    let mut input = tokio::fs::File::open(source).await?;
    let temporary = tempfile::Builder::new()
        .prefix("rc-mirror-local-source-")
        .suffix(".part")
        .tempfile()?;
    let (output, staging) = temporary.into_parts();
    let mut output = tokio::fs::File::from_std(output);
    if let Err(error) = tokio::io::copy(&mut input, &mut output).await {
        return Err(Error::Io(error));
    }
    if let Err(error) = output.flush().await {
        return Err(Error::Io(error));
    }
    Ok(staging)
}

fn temporary_mirror_path(label: &str) -> rc_core::Result<tempfile::TempPath> {
    Ok(tempfile::Builder::new()
        .prefix(&format!("rc-mirror-{label}-"))
        .suffix(".part")
        .tempfile()?
        .into_temp_path())
}

fn mirror_staging_path(destination: &Path) -> rc_core::Result<tempfile::TempPath> {
    let parent = destination.parent().ok_or_else(|| {
        Error::InvalidPath(format!(
            "Mirror destination has no parent: {}",
            destination.display()
        ))
    })?;
    let file_name = destination.file_name().ok_or_else(|| {
        Error::InvalidPath(format!(
            "Mirror destination has no file name: {}",
            destination.display()
        ))
    })?;
    Ok(tempfile::Builder::new()
        .prefix(&format!(".{}.rc-mirror-", file_name.to_string_lossy()))
        .tempfile_in(parent)?
        .into_temp_path())
}

async fn finish_staged_download(
    staging: tempfile::TempPath,
    destination: &Path,
    validation: rc_core::Result<u64>,
    modified: Option<Timestamp>,
    replace_existing: bool,
) -> rc_core::Result<u64> {
    let bytes = match validation {
        Ok(bytes) => bytes,
        Err(error) => return Err(error),
    };
    if let Some(modified) = modified
        && let Err(error) = set_file_modified(&staging, modified)
    {
        return Err(error);
    }
    let persisted = if replace_existing {
        staging.persist(destination)
    } else {
        staging.persist_noclobber(destination)
    };
    persisted.map_err(|error| {
        if !replace_existing && error.error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Conflict(format!(
                "Destination appeared before mirror completion: {}",
                destination.display()
            ))
        } else {
            Error::General(format!(
                "Failed to atomically replace mirror destination '{}': {}",
                destination.display(),
                error.error
            ))
        }
    })?;
    Ok(bytes)
}

fn set_file_modified(path: &Path, modified: Timestamp) -> rc_core::Result<()> {
    let modified: std::time::SystemTime = modified.into();
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_times(std::fs::FileTimes::new().set_modified(modified))?;
    Ok(())
}

async fn inspect_local_entry(
    root: &Path,
    relative_path: &str,
    expected_path: &Path,
) -> rc_core::Result<Option<MirrorEntry>> {
    let path = secure_local_path(root, relative_path, false).await?;
    if path != expected_path {
        return Err(Error::InvalidPath(format!(
            "Local mirror target escaped its root: {}",
            expected_path.display()
        )));
    }
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::InvalidPath(format!(
            "Destination is a symbolic link: {}",
            path.display()
        ))),
        Ok(metadata) if metadata.is_file() => Ok(Some(MirrorEntry {
            relative_path: relative_path.to_string(),
            location: MirrorLocation::Local(path),
            snapshot: snapshot_from_metadata(&metadata),
        })),
        Ok(_) => Err(Error::InvalidPath(format!(
            "Destination is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

async fn secure_local_path(
    root: &Path,
    relative_path: &str,
    create_parents: bool,
) -> rc_core::Result<PathBuf> {
    let relative_path = normalize_relative_path(relative_path)?;
    match tokio::fs::symlink_metadata(root).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(Error::InvalidPath(format!(
                "Local mirror root is a symbolic link: {}",
                root.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(Error::InvalidPath(format!(
                "Local mirror root is not a directory: {}",
                root.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_parents => {
            create_local_directory_tree(root).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }

    let components = relative_path.split('/').collect::<Vec<_>>();
    let mut path = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        let is_final = index + 1 == components.len();
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::InvalidPath(format!(
                    "Local mirror path component is a symbolic link: {}",
                    path.display()
                )));
            }
            Ok(metadata) if !is_final && !metadata.is_dir() => {
                return Err(Error::InvalidPath(format!(
                    "Local mirror parent is not a directory: {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && create_parents && !is_final =>
            {
                match tokio::fs::create_dir(&path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(Error::Io(error)),
                }
                let metadata = tokio::fs::symlink_metadata(&path).await?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(Error::InvalidPath(format!(
                        "Unsafe local mirror parent: {}",
                        path.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Ok(path)
}

async fn create_local_directory_tree(root: &Path) -> rc_core::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = root.to_path_buf();
    loop {
        if cursor.as_os_str().is_empty() {
            cursor = PathBuf::from(".");
        }
        match tokio::fs::symlink_metadata(&cursor).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::InvalidPath(format!(
                    "Local mirror directory component is a symbolic link: {}",
                    cursor.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(Error::InvalidPath(format!(
                    "Local mirror directory component is not a directory: {}",
                    cursor.display()
                )));
            }
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor.clone());
                cursor = cursor.parent().map(Path::to_path_buf).ok_or_else(|| {
                    Error::InvalidPath(format!(
                        "Local mirror root has no existing ancestor: {}",
                        root.display()
                    ))
                })?;
            }
            Err(error) => return Err(Error::Io(error)),
        }
    }

    for directory in missing.into_iter().rev() {
        match tokio::fs::create_dir(&directory).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Error::Io(error)),
        }
        let metadata = tokio::fs::symlink_metadata(&directory).await?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::InvalidPath(format!(
                "Unsafe local mirror directory: {}",
                directory.display()
            )));
        }
    }
    Ok(())
}

fn mirror_locations_overlap(
    source: &RemotePath,
    target: &RemotePath,
    source_endpoint: &str,
    target_endpoint: &str,
) -> rc_core::Result<bool> {
    if source.bucket != target.bucket || !same_endpoint(source_endpoint, target_endpoint) {
        return Ok(false);
    }
    let source = normalized_remote_root_prefix(&source.key)?;
    let target = normalized_remote_root_prefix(&target.key)?;
    Ok(source.is_empty()
        || target.is_empty()
        || source == target
        || source.starts_with(&target)
        || target.starts_with(&source))
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

fn exit_code_for_error(error: &Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

#[cfg(test)]
mod roadmap_tests;
