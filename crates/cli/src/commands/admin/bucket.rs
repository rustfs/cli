//! Per-bucket administrative features.
//!
//! `rc admin bucket migration` manages On-Demand Migration: a bucket names an
//! external S3-compatible source bucket, a GET that misses locally is served
//! from that source and stored locally, and a background backfill job pulls
//! the rest. The wire contract is pinned by the fixtures under
//! `crates/core/tests/fixtures/on_demand_migration/`.

use clap::{Args, Subcommand, ValueEnum};
use rc_core::admin::{
    BackfillJob, BackfillStartRequest, HeadPolicy, MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES,
    ON_DEMAND_MIGRATION_CAPABILITY, OnDemandMigrationApi, OnDemandMigrationConfigRequest,
    OnDemandMigrationConfigView, OnDemandMigrationSetResult, OnDemandMigrationStatus, PathStyle,
    REDACTED_SECRET, RangeGetPolicy, SkipExisting, SourceCredentialsRequest, SourceErrorPolicy,
    SourceProvider, SourceRequest, TlsRequest, validate_ca_cert_pem, validate_local_bucket,
};
use rc_core::{Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zeroize::Zeroizing;

use crate::exit_code::ExitCode;
use crate::output::Formatter;
use crate::secret_input::{SecretSource, can_prompt};

/// Environment variable read for the source secret key when `--secret-key` is absent.
pub const SECRET_KEY_ENV: &str = "RC_ODM_SECRET_KEY";

const OUTPUT_TYPE: &str = "on_demand_migration";

/// Rendered for a ratio the server did not compute. Never zero: a missing
/// ratio and a zero ratio mean different things.
const NO_VALUE: &str = "\u{2014}";

#[derive(Subcommand, Debug)]
pub enum BucketCommands {
    /// Serve misses from an external S3-compatible source bucket and migrate it in place
    #[command(subcommand)]
    Migration(MigrationCommands),
}

#[derive(Subcommand, Debug)]
pub enum MigrationCommands {
    /// Validate, probe and save the source configuration, replacing any existing one
    Set(SetArgs),
    /// Show the saved configuration with credentials redacted
    Get(TargetArgs),
    /// Remove the configuration; already-pulled objects stay in place
    Rm(TargetArgs),
    /// Show the answering node's runtime status: hit ratio, pulls, breaker, last error
    Status(StatusArgs),
    /// Control the background backfill job
    #[command(subcommand)]
    Backfill(BackfillCommands),
}

#[derive(Subcommand, Debug)]
pub enum BackfillCommands {
    /// Walk the source listing and pull every object that is missing locally
    Start(BackfillStartArgs),
    /// Ask the running job to stop at its next checkpoint
    Cancel(TargetArgs),
    /// Show the job checkpoint
    Status(StatusArgs),
}

#[derive(Args, Debug)]
pub struct TargetArgs {
    /// Local bucket as alias/bucket
    pub target: String,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Refresh until interrupted, or until a backfill job reaches a terminal state
    #[arg(long)]
    pub watch: bool,
    /// Seconds between refreshes with --watch
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub interval: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    S3,
    Aws,
    Minio,
    Rustfs,
    R2,
    Gcs,
}

impl From<ProviderArg> for SourceProvider {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::S3 => Self::S3,
            ProviderArg::Aws => Self::Aws,
            ProviderArg::Minio => Self::Minio,
            ProviderArg::Rustfs => Self::Rustfs,
            ProviderArg::R2 => Self::R2,
            ProviderArg::Gcs => Self::Gcs,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum PathStyleArg {
    #[default]
    Auto,
    Path,
    Virtual,
}

impl From<PathStyleArg> for PathStyle {
    fn from(value: PathStyleArg) -> Self {
        match value {
            PathStyleArg::Auto => Self::Auto,
            PathStyleArg::Path => Self::Path,
            PathStyleArg::Virtual => Self::Virtual,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum HeadArg {
    #[default]
    Proxy,
    LocalOnly,
}

impl From<HeadArg> for HeadPolicy {
    fn from(value: HeadArg) -> Self {
        match value {
            HeadArg::Proxy => Self::Proxy,
            HeadArg::LocalOnly => Self::LocalOnly,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum RangeGetArg {
    #[default]
    ServeAndBackfill,
    ServeOnly,
}

impl From<RangeGetArg> for RangeGetPolicy {
    fn from(value: RangeGetArg) -> Self {
        match value {
            RangeGetArg::ServeAndBackfill => Self::ServeAndBackfill,
            RangeGetArg::ServeOnly => Self::ServeOnly,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum SourceErrorArg {
    #[default]
    Propagate,
    NotFound,
}

impl From<SourceErrorArg> for SourceErrorPolicy {
    fn from(value: SourceErrorArg) -> Self {
        match value {
            SourceErrorArg::Propagate => Self::Propagate,
            SourceErrorArg::NotFound => Self::NotFound,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum SkipExistingArg {
    #[default]
    Always,
    EtagOrSize,
}

impl From<SkipExistingArg> for SkipExisting {
    fn from(value: SkipExistingArg) -> Self {
        match value {
            SkipExistingArg::Always => Self::Always,
            SkipExistingArg::EtagOrSize => Self::EtagOrSize,
        }
    }
}

/// A secret taken from the command line, kept in zeroizing storage and never
/// shown by `Debug`.
#[derive(Clone)]
pub struct SecretArg(Zeroizing<String>);

impl std::fmt::Debug for SecretArg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretArg([REDACTED])")
    }
}

fn parse_secret_arg(value: &str) -> std::result::Result<SecretArg, String> {
    Ok(SecretArg(Zeroizing::new(value.to_string())))
}

#[derive(Args, Debug)]
pub struct SetArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Source vendor family; drives addressing defaults
    #[arg(long, value_enum)]
    pub provider: ProviderArg,
    /// Source endpoint as scheme://host\[:port\]; derived from --region for aws
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,
    /// Source signing region; 'auto' is accepted for r2, minio and rustfs
    #[arg(long, value_name = "R")]
    pub region: String,
    /// Bucket on the source
    #[arg(long, value_name = "B")]
    pub source_bucket: String,
    /// Only local keys with this prefix consult the source
    #[arg(long, value_name = "P")]
    pub prefix: Option<String>,
    /// Prepended to the local key to form the source key
    #[arg(long, value_name = "SP")]
    pub source_prefix: Option<String>,
    /// Source access key
    #[arg(long, value_name = "AK", conflicts_with = "public")]
    pub access_key: Option<String>,
    /// Source secret key; prefer RC_ODM_SECRET_KEY or the hidden prompt, which stay out of shell history
    #[arg(long, value_name = "SK", requires = "access_key", conflicts_with = "public", value_parser = parse_secret_arg)]
    pub secret_key: Option<SecretArg>,
    /// Read the source anonymously
    #[arg(long)]
    pub public: bool,
    /// Bucket addressing style
    #[arg(long, value_enum, default_value_t = PathStyleArg::Auto)]
    pub path_style: PathStyleArg,
    /// Do not verify the source TLS certificate
    #[arg(long)]
    pub skip_tls_verify: bool,
    /// PEM CA bundle used to verify the source TLS certificate
    #[arg(long, value_name = "FILE")]
    pub ca_cert: Option<PathBuf>,
    /// What a HEAD that misses locally does
    #[arg(long, value_enum, default_value_t = HeadArg::Proxy)]
    pub head: HeadArg,
    /// Whether a Range GET also queues a whole-object background pull
    #[arg(long, value_enum, default_value_t = RangeGetArg::ServeAndBackfill)]
    pub range_get: RangeGetArg,
    /// How a source failure is answered to the client
    #[arg(long, value_enum, default_value_t = SourceErrorArg::Propagate)]
    pub source_error: SourceErrorArg,
    /// Do not keep the source ETag on stored objects
    #[arg(long)]
    pub no_preserve_etag: bool,
    /// Copy source object tags; costs one extra source call per inline pull
    #[arg(long)]
    pub copy_tags: bool,
    /// Do not emit ObjectCreated notifications for write-backs
    #[arg(long)]
    pub no_events: bool,
    /// Largest object teed inline on a GET miss; larger objects stream through and pull in the background
    #[arg(long, value_name = "N")]
    pub inline_max_bytes: Option<u64>,
    /// Pull concurrency shared by inline and background paths (1-256)
    #[arg(long, value_name = "N")]
    pub max_concurrent_pulls: Option<u32>,
    /// Validate the configuration and probe the source without saving
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct BackfillStartArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Only walk source keys with this prefix
    #[arg(long, value_name = "P")]
    pub prefix: Option<String>,
    /// When a local object counts as already migrated
    #[arg(long, value_enum, default_value_t = SkipExistingArg::Always)]
    pub skip_existing: SkipExistingArg,
    /// List and count only; nothing is queued
    #[arg(long)]
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Preparation: everything that can fail before the network
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Set,
    Get,
    Remove,
    Status,
    BackfillStart,
    BackfillCancel,
    BackfillStatus,
}

impl Operation {
    /// Reads can be retried by a caller; a write is a second decision.
    const fn is_read(self) -> bool {
        matches!(self, Self::Get | Self::Status | Self::BackfillStatus)
    }
}

#[derive(Debug)]
enum Action {
    Set {
        request: Box<OnDemandMigrationConfigRequest>,
        dry_run: bool,
    },
    Get,
    Remove,
    Status {
        watch: Option<Duration>,
    },
    BackfillStart(BackfillStartRequest),
    BackfillCancel,
    BackfillStatus {
        watch: Option<Duration>,
    },
}

impl Action {
    const fn operation(&self) -> Operation {
        match self {
            Self::Set { .. } => Operation::Set,
            Self::Get => Operation::Get,
            Self::Remove => Operation::Remove,
            Self::Status { .. } => Operation::Status,
            Self::BackfillStart(_) => Operation::BackfillStart,
            Self::BackfillCancel => Operation::BackfillCancel,
            Self::BackfillStatus { .. } => Operation::BackfillStatus,
        }
    }
}

#[derive(Debug)]
struct Prepared {
    alias: String,
    bucket: String,
    action: Action,
}

/// `alias/bucket`, nothing more: a key or a trailing slash is a usage error.
fn parse_target(target: &str) -> Result<(String, String)> {
    let (alias, bucket) = target
        .split_once('/')
        .ok_or_else(|| Error::InvalidPath("Expected alias/bucket".into()))?;
    if alias.is_empty() {
        return Err(Error::InvalidPath("Expected alias/bucket".into()));
    }
    validate_local_bucket(bucket)?;
    Ok((alias.to_string(), bucket.to_string()))
}

fn watch_interval(args: &StatusArgs) -> Option<Duration> {
    args.watch.then(|| Duration::from_secs(args.interval))
}

/// Where the secret comes from, in order: `--secret-key`, `RC_ODM_SECRET_KEY`,
/// then a hidden prompt when there is a terminal and output is human-readable.
///
/// The server replaces a saved secret with `REDACTED` in every response and a
/// `set` replaces the configuration wholesale, so the placeholder is refused:
/// saving it would overwrite a working credential with the literal word.
fn resolve_secret(
    args: &SetArgs,
    environment_secret: Option<std::ffi::OsString>,
    formatter: &Formatter,
) -> Result<Option<Zeroizing<String>>> {
    if args.public {
        return Ok(None);
    }
    if args.access_key.is_none() {
        return Err(Error::Config(
            "Provide --access-key (with the secret from RC_ODM_SECRET_KEY, --secret-key or the prompt) or --public".into(),
        ));
    }
    let secret = if let Some(SecretArg(secret)) = &args.secret_key {
        secret.clone()
    } else if let Some(value) = environment_secret {
        let value = Zeroizing::new(value.to_string_lossy().into_owned());
        let trimmed = Zeroizing::new(value.trim_end_matches(['\r', '\n']).to_string());
        if trimmed.is_empty() {
            return Err(Error::Config(format!("{SECRET_KEY_ENV} is set but empty")));
        }
        trimmed
    } else if can_prompt(formatter.is_json()) {
        SecretSource::Prompt.load("Source secret key: ")?
    } else {
        return Err(Error::Config(format!(
            "Provide the source secret key with {SECRET_KEY_ENV} (or --secret-key) when running non-interactively or with --json"
        )));
    };
    if secret.as_str() == REDACTED_SECRET {
        return Err(Error::Config(
            "The secret key is the redaction placeholder; set replaces the configuration, so pass the real secret again".into(),
        ));
    }
    Ok(Some(secret))
}

fn read_ca_cert(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        Error::Config(format!(
            "Failed to read --ca-cert '{}': {error}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES {
        return Err(Error::Config(format!(
            "--ca-cert exceeds {MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES} bytes"
        )));
    }
    let pem = String::from_utf8(bytes)
        .map_err(|_| Error::Config("--ca-cert must be a UTF-8 PEM file".into()))?;
    validate_ca_cert_pem(&pem)?;
    Ok(pem)
}

fn build_request(
    args: &SetArgs,
    secret: Option<Zeroizing<String>>,
) -> Result<OnDemandMigrationConfigRequest> {
    let credentials = match (&args.access_key, secret) {
        (Some(access_key), Some(secret_key)) => Some(SourceCredentialsRequest {
            access_key: access_key.clone(),
            secret_key,
            session_token: None,
        }),
        _ => None,
    };
    let ca_cert_pem = args.ca_cert.as_deref().map(read_ca_cert).transpose()?;
    let mut request = OnDemandMigrationConfigRequest::new(SourceRequest {
        provider: args.provider.into(),
        endpoint: args.endpoint.clone(),
        region: args.region.clone(),
        bucket: args.source_bucket.clone(),
        path_style: args.path_style.into(),
        credentials,
        tls: TlsRequest {
            skip_verify: args.skip_tls_verify,
            ca_cert_pem,
        },
    });
    request.filter.prefix = args.prefix.clone();
    request.filter.source_prefix = args.source_prefix.clone();
    let policy = &mut request.policy;
    policy.head = args.head.into();
    policy.range_get = args.range_get.into();
    policy.source_error = args.source_error.into();
    policy.preserve_etag = !args.no_preserve_etag;
    policy.copy_tags = args.copy_tags;
    policy.emit_events = !args.no_events;
    if let Some(value) = args.inline_max_bytes {
        policy.inline_max_bytes = value;
    }
    if let Some(value) = args.max_concurrent_pulls {
        policy.max_concurrent_pulls = value;
    }
    request.validate()?;
    Ok(request)
}

fn prepare(command: MigrationCommands, formatter: &Formatter) -> Result<Prepared> {
    let (target, action) = match command {
        MigrationCommands::Set(args) => {
            let (alias, bucket) = parse_target(&args.target.target)?;
            // Validate everything that does not need the secret first, so a
            // typo never costs the operator a prompt.
            build_request(&args, None)?;
            let secret = resolve_secret(&args, std::env::var_os(SECRET_KEY_ENV), formatter)?;
            let request = build_request(&args, secret)?;
            return Ok(Prepared {
                alias,
                bucket,
                action: Action::Set {
                    request: Box::new(request),
                    dry_run: args.dry_run,
                },
            });
        }
        MigrationCommands::Get(args) => (args.target, Action::Get),
        MigrationCommands::Rm(args) => (args.target, Action::Remove),
        MigrationCommands::Status(args) => {
            let watch = watch_interval(&args);
            (args.target.target, Action::Status { watch })
        }
        MigrationCommands::Backfill(BackfillCommands::Start(args)) => {
            let request = BackfillStartRequest {
                prefix: args.prefix,
                skip_existing: Some(args.skip_existing.into()),
                dry_run: args.dry_run,
            };
            request.validate()?;
            (args.target.target, Action::BackfillStart(request))
        }
        MigrationCommands::Backfill(BackfillCommands::Cancel(args)) => {
            (args.target, Action::BackfillCancel)
        }
        MigrationCommands::Backfill(BackfillCommands::Status(args)) => {
            let watch = watch_interval(&args);
            (args.target.target, Action::BackfillStatus { watch })
        }
    };
    let (alias, bucket) = parse_target(&target)?;
    Ok(Prepared {
        alias,
        bucket,
        action,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

pub async fn execute(command: BucketCommands, formatter: &Formatter) -> ExitCode {
    match command {
        BucketCommands::Migration(command) => execute_migration(command, formatter).await,
    }
}

const fn command_operation(command: &MigrationCommands) -> Operation {
    match command {
        MigrationCommands::Set(_) => Operation::Set,
        MigrationCommands::Get(_) => Operation::Get,
        MigrationCommands::Rm(_) => Operation::Remove,
        MigrationCommands::Status(_) => Operation::Status,
        MigrationCommands::Backfill(BackfillCommands::Start(_)) => Operation::BackfillStart,
        MigrationCommands::Backfill(BackfillCommands::Cancel(_)) => Operation::BackfillCancel,
        MigrationCommands::Backfill(BackfillCommands::Status(_)) => Operation::BackfillStatus,
    }
}

async fn execute_migration(command: MigrationCommands, formatter: &Formatter) -> ExitCode {
    let operation = command_operation(&command);
    let prepared = match prepare(command, formatter) {
        Ok(prepared) => prepared,
        Err(error) => return emit_error(&error, operation, formatter),
    };
    let client = rc_core::AliasManager::new()
        .and_then(|aliases| aliases.get(&prepared.alias))
        .and_then(|alias| rc_s3::AdminClient::new(&alias));
    let client = match client {
        Ok(client) => client,
        Err(error) => return emit_error(&error, prepared.action.operation(), formatter),
    };
    execute_with_api(prepared, &client, formatter).await
}

async fn execute_with_api(
    prepared: Prepared,
    api: &dyn OnDemandMigrationApi,
    formatter: &Formatter,
) -> ExitCode {
    let operation = prepared.action.operation();
    let bucket = prepared.bucket;
    let result = match prepared.action {
        Action::Set { request, dry_run } => api
            .set_on_demand_migration(&bucket, &request, dry_run)
            .await
            .map(|result| {
                render_set(&bucket, &result, dry_run, formatter);
                serde_json::to_value(result)
            }),
        Action::Get => api.get_on_demand_migration(&bucket).await.map(|result| {
            if !formatter.is_json() {
                match &result.config {
                    Some(config) => {
                        render_config(&bucket, config, result.updated_at.as_deref(), formatter);
                    }
                    None => formatter.println(&format!(
                        "No on-demand migration configuration returned for '{}'",
                        formatter.sanitize_text(&bucket)
                    )),
                }
            }
            serde_json::to_value(result)
        }),
        Action::Remove => api.delete_on_demand_migration(&bucket).await.map(|()| {
            formatter.success(&format!(
                "On-demand migration removed from '{}'; already-pulled objects stay in place",
                formatter.sanitize_text(&bucket)
            ));
            Ok(json!({"bucket": bucket.as_str(), "removed": true}))
        }),
        Action::Status { watch: None } => {
            api.on_demand_migration_status(&bucket).await.map(|status| {
                render_status(&bucket, &status, formatter);
                serde_json::to_value(status)
            })
        }
        Action::Status {
            watch: Some(interval),
        } => watch_status(api, &bucket, interval, formatter).await,
        Action::BackfillStart(request) => api
            .start_on_demand_migration_backfill(&bucket, &request)
            .await
            .map(|result| {
                if !formatter.is_json() {
                    let verb = if request.dry_run {
                        "Backfill dry run started"
                    } else {
                        "Backfill started"
                    };
                    formatter.success(&format!(
                        "{verb} for '{}'",
                        formatter.sanitize_text(&bucket)
                    ));
                    render_backfill(&bucket, result.job.as_ref(), formatter);
                }
                serde_json::to_value(result)
            }),
        Action::BackfillCancel => {
            api.cancel_on_demand_migration_backfill(&bucket)
                .await
                .map(|result| {
                    if !formatter.is_json() {
                        formatter.success(&format!(
                            "Backfill cancellation requested for '{}'",
                            formatter.sanitize_text(&bucket)
                        ));
                        render_backfill(&bucket, result.job.as_ref(), formatter);
                    }
                    serde_json::to_value(result)
                })
        }
        Action::BackfillStatus { watch: None } => api
            .on_demand_migration_backfill_status(&bucket)
            .await
            .map(|result| {
                if !formatter.is_json() {
                    render_backfill(&bucket, result.job.as_ref(), formatter);
                }
                serde_json::to_value(result)
            }),
        Action::BackfillStatus {
            watch: Some(interval),
        } => watch_backfill(api, &bucket, interval, formatter).await,
    };
    match result {
        Ok(Ok(value)) => {
            // Watch mode already streamed one record per refresh.
            if formatter.is_json() && !value.is_null() {
                formatter.json(&success_output(operation, &bucket, value));
            }
            ExitCode::Success
        }
        Ok(Err(error)) => emit_error(&Error::Json(error), operation, formatter),
        Err(error) => emit_error(&error, operation, formatter),
    }
}

/// Refresh the runtime status until interrupted. Human output redraws the
/// full block each tick; JSON output is one record per tick.
async fn watch_status(
    api: &dyn OnDemandMigrationApi,
    bucket: &str,
    interval: Duration,
    formatter: &Formatter,
) -> Result<serde_json::Result<Value>> {
    loop {
        let status = api.on_demand_migration_status(bucket).await?;
        if formatter.is_json() {
            formatter.json_line(&success_output(
                Operation::Status,
                bucket,
                serde_json::to_value(&status)?,
            ));
        } else {
            render_status(bucket, &status, formatter);
            formatter.println("");
        }
        tokio::time::sleep(interval).await;
    }
}

/// Refresh one progress line until the job reaches a terminal state, then
/// print the final checkpoint. JSON output is one record per tick.
async fn watch_backfill(
    api: &dyn OnDemandMigrationApi,
    bucket: &str,
    interval: Duration,
    formatter: &Formatter,
) -> Result<serde_json::Result<Value>> {
    let term = console::Term::stderr();
    loop {
        let result = api.on_demand_migration_backfill_status(bucket).await?;
        let terminal = result.job.as_ref().is_none_or(BackfillJob::is_terminal);
        if formatter.is_json() {
            formatter.json_line(&success_output(
                Operation::BackfillStatus,
                bucket,
                serde_json::to_value(&result)?,
            ));
        } else if let Some(job) = &result.job {
            // The line is stderr so stdout stays clean for the final document.
            let _ = term.clear_line();
            let _ = term.write_str(&formatter.sanitize_text(&backfill_progress_line(job)));
        }
        if terminal {
            if !formatter.is_json() {
                let _ = term.write_line("");
                render_backfill(bucket, result.job.as_ref(), formatter);
            }
            // The stream already carried the final record.
            return Ok(Ok(Value::Null));
        }
        tokio::time::sleep(interval).await;
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn success_output(operation: Operation, bucket: &str, result: Value) -> Value {
    json!({
        "schema_version": 3,
        "type": OUTPUT_TYPE,
        "status": "success",
        "data": {"operation": operation, "bucket": bucket, "result": result},
    })
}

fn error_output(error: &Error, operation: Operation) -> Value {
    let kind = match error.exit_code() {
        2 => "usage_error",
        3 => "network_error",
        4 => "auth_error",
        5 => "not_found",
        6 => "conflict",
        7 => "unsupported_feature",
        130 => "interrupted",
        _ => "general_error",
    };
    let mut detail = json!({
        "type": kind,
        "message": error.to_string(),
        "retryable": matches!(error, Error::Network(_)) && operation.is_read(),
    });
    if let Some(suggestion) = suggestion(error, operation) {
        detail["suggestion"] = json!(suggestion);
    }
    if error.exit_code() == 7 {
        detail["capability"] = json!(ON_DEMAND_MIGRATION_CAPABILITY);
        detail["server"] = Value::Null;
    }
    json!({"schema_version": 3, "type": OUTPUT_TYPE, "status": "error", "error": detail})
}

fn suggestion(error: &Error, operation: Operation) -> Option<&'static str> {
    match error {
        Error::Network(_) if operation == Operation::Set => Some(
            "Check the source endpoint, region, bucket and credentials, then retry with --dry-run.",
        ),
        Error::Network(_) => Some("Verify the endpoint and network connectivity, then retry."),
        Error::Auth(_) => Some(
            "Verify admin permissions (admin:SetBucketOnDemandMigration / admin:GetBucketOnDemandMigration) and the server licence.",
        ),
        Error::Conflict(_) => {
            Some("A backfill job already holds the lease; cancel it or wait for it to finish.")
        }
        Error::UnsupportedFeature(_) => {
            Some("Upgrade RustFS to a release that ships on-demand migration.")
        }
        Error::InvalidPath(_) | Error::Config(_) => Some("Review the command arguments and retry."),
        _ => None,
    }
}

fn emit_error(error: &Error, operation: Operation, formatter: &Formatter) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    if formatter.is_json() {
        formatter.json_error(&error_output(error, operation));
    } else if let Some(suggestion) = suggestion(error, operation) {
        formatter.error_with_suggestion(code, &error.to_string(), suggestion);
    } else {
        formatter.error_with_code(code, &error.to_string());
    }
    code
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn or_dash(value: Option<&str>) -> &str {
    value.filter(|value| !value.is_empty()).unwrap_or(NO_VALUE)
}

fn bytes_text(bytes: u64) -> String {
    format!(
        "{} ({bytes})",
        humansize::format_size(bytes, humansize::BINARY)
    )
}

fn row(formatter: &Formatter, label: &str, value: &str) {
    formatter.println(&format!(
        "{:<22}{}",
        format!("{label}:"),
        formatter.sanitize_text(value)
    ));
}

fn render_set(
    bucket: &str,
    result: &OnDemandMigrationSetResult,
    dry_run: bool,
    formatter: &Formatter,
) {
    if formatter.is_json() {
        return;
    }
    let bucket_text = formatter.sanitize_text(bucket);
    if dry_run || result.dry_run {
        formatter.success(&format!(
            "Dry run: on-demand migration configuration for '{bucket_text}' is valid; nothing was saved"
        ));
    } else {
        formatter.success(&format!(
            "On-demand migration configured for '{bucket_text}'"
        ));
    }
    if let Some(probe) = &result.probe {
        row(formatter, "Source reachable", yes_no(probe.reachable));
        row(formatter, "Source listable", yes_no(probe.listable));
        row(
            formatter,
            "Sample key",
            or_dash(probe.sample_key.as_deref()),
        );
    }
    if let Some(config) = &result.config {
        formatter.println("");
        render_config(bucket, config, result.updated_at.as_deref(), formatter);
    }
}

fn render_config(
    bucket: &str,
    config: &OnDemandMigrationConfigView,
    updated_at: Option<&str>,
    formatter: &Formatter,
) {
    let source = &config.source;
    let policy = &config.policy;
    formatter.println(&formatter.style_name(&format!(
        "On-Demand Migration: {}",
        formatter.sanitize_text(bucket)
    )));
    row(formatter, "Enabled", yes_no(config.enabled));
    row(formatter, "Provider", or_dash(Some(&source.provider)));
    row(formatter, "Endpoint", or_dash(source.endpoint.as_deref()));
    row(formatter, "Region", or_dash(Some(&source.region)));
    row(formatter, "Source bucket", or_dash(Some(&source.bucket)));
    row(formatter, "Path style", source.path_style.as_str());
    match &source.credentials {
        Some(credentials) => {
            row(
                formatter,
                "Access key",
                or_dash(Some(&credentials.access_key)),
            );
            row(
                formatter,
                "Secret key",
                if credentials.has_secret_key {
                    REDACTED_SECRET
                } else {
                    NO_VALUE
                },
            );
            if credentials.has_session_token {
                row(formatter, "Session token", REDACTED_SECRET);
            }
        }
        None => row(formatter, "Credentials", "anonymous"),
    }
    row(formatter, "TLS verify", yes_no(!source.tls.skip_verify));
    row(
        formatter,
        "CA certificate",
        if source.tls.has_ca_cert {
            "custom bundle"
        } else {
            "system roots"
        },
    );
    row(
        formatter,
        "Prefix",
        or_dash(config.filter.prefix.as_deref()),
    );
    row(
        formatter,
        "Source prefix",
        or_dash(config.filter.source_prefix.as_deref()),
    );
    row(formatter, "HEAD policy", policy.head.as_str());
    row(formatter, "Range GET policy", policy.range_get.as_str());
    row(
        formatter,
        "Source error policy",
        policy.source_error.as_str(),
    );
    row(formatter, "List through", yes_no(policy.list_through));
    row(formatter, "Preserve ETag", yes_no(policy.preserve_etag));
    row(formatter, "Copy tags", yes_no(policy.copy_tags));
    row(formatter, "Emit events", yes_no(policy.emit_events));
    row(
        formatter,
        "Negative cache TTL",
        &format!("{} s", policy.negative_cache_ttl_secs),
    );
    row(
        formatter,
        "Inline max bytes",
        &bytes_text(policy.inline_max_bytes),
    );
    row(
        formatter,
        "Multipart part size",
        &bytes_text(policy.multipart_part_size_bytes),
    );
    row(
        formatter,
        "Max concurrent pulls",
        &policy.max_concurrent_pulls.to_string(),
    );
    row(
        formatter,
        "Pull queue capacity",
        &policy.pull_queue_capacity.to_string(),
    );
    row(
        formatter,
        "Source timeout",
        &format!(
            "connect {} ms, first byte {} ms, idle {} ms",
            policy.source_timeout.connect_ms,
            policy.source_timeout.first_byte_ms,
            policy.source_timeout.idle_ms
        ),
    );
    row(
        formatter,
        "Bandwidth limit",
        &policy
            .bandwidth_limit_bytes_per_sec
            .map(|limit| format!("{}/s", humansize::format_size(limit, humansize::BINARY)))
            .unwrap_or_else(|| "unlimited".to_string()),
    );
    row(formatter, "Updated at", or_dash(updated_at));
}

fn counter_line(counters: &std::collections::BTreeMap<String, u64>, nonzero_only: bool) -> String {
    let parts = counters
        .iter()
        .filter(|(_, count)| !nonzero_only || **count > 0)
        .map(|(label, count)| format!("{label} {count}"))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

fn ratio_text(ratio: Option<f64>) -> String {
    match ratio {
        Some(ratio) if ratio.is_finite() => format!("{:.1}%", ratio * 100.0),
        _ => NO_VALUE.to_string(),
    }
}

fn render_status(bucket: &str, status: &OnDemandMigrationStatus, formatter: &Formatter) {
    if formatter.is_json() {
        return;
    }
    formatter.println(&formatter.style_name(&format!(
        "On-Demand Migration Status: {}",
        formatter.sanitize_text(bucket)
    )));
    row(formatter, "Configured", yes_no(status.configured));
    row(formatter, "Enabled", yes_no(status.enabled));
    row(formatter, "Module enabled", yes_no(status.module_enabled));
    row(formatter, "Provider", or_dash(status.provider.as_deref()));
    row(
        formatter,
        "Source host",
        or_dash(status.endpoint_host.as_deref()),
    );
    match &status.breaker {
        Some(breaker) => {
            let mut text = or_dash(Some(&breaker.state)).to_string();
            if let Some(opened_at) = breaker.opened_at.as_deref() {
                text.push_str(&format!(" (opened {opened_at})"));
            }
            row(formatter, "Breaker", &text);
        }
        None => row(formatter, "Breaker", NO_VALUE),
    }
    row(
        formatter,
        "Source-hit ratio",
        &ratio_text(status.served_by_source_ratio),
    );
    match &status.counters {
        Some(counters) => {
            row(
                formatter,
                "Migrated bytes",
                &bytes_text(counters.pulled_bytes_total),
            );
            row(
                formatter,
                "Pulled objects",
                &counter_line(&counters.pulled_objects_total, false),
            );
            for (operation, outcomes) in &counters.requests_total {
                row(
                    formatter,
                    &format!("Requests ({operation})"),
                    &counter_line(outcomes, true),
                );
            }
            row(
                formatter,
                "Pull failures",
                &counter_line(&counters.pull_failures_total, true),
            );
            if let Some(latency) = &counters.source_latency {
                let text = if latency.count == 0 {
                    "no samples".to_string()
                } else {
                    format!(
                        "{} samples, mean {} ms",
                        latency.count,
                        latency.sum_ms / latency.count
                    )
                };
                row(formatter, "Source latency", &text);
            }
        }
        None => row(formatter, "Counters", NO_VALUE),
    }
    row(
        formatter,
        "In-flight pulls",
        &status.inflight_pulls.to_string(),
    );
    row(formatter, "Queued pulls", &status.queue_depth.to_string());
    match &status.last_source_error {
        Some(error) => {
            let mut text = or_dash(Some(&error.class)).to_string();
            if let Some(at) = error.at.as_deref() {
                text.push_str(&format!(" at {at}"));
            }
            row(formatter, "Last source error", &text);
        }
        None => row(formatter, "Last source error", "none"),
    }
    if let Some(backfill) = &status.backfill {
        row(
            formatter,
            "Backfill",
            &format!(
                "{} (job {}): listed {}, enqueued {}, pulled {}, skipped {}, failed {}, {}",
                or_dash(Some(&backfill.state)),
                or_dash(Some(&backfill.job_id)),
                backfill.listed,
                backfill.enqueued,
                backfill.pulled,
                backfill.skipped_existing,
                backfill.failed,
                humansize::format_size(backfill.bytes, humansize::BINARY)
            ),
        );
    }
    row(
        formatter,
        "Updated at",
        or_dash(status.updated_at.as_deref()),
    );
}

fn backfill_progress_line(job: &BackfillJob) -> String {
    format!(
        "[{}] listed {} \u{b7} enqueued {} \u{b7} pulled {} \u{b7} skipped {} \u{b7} failed {} \u{b7} {} \u{b7} updated {}",
        or_dash(Some(&job.state)),
        job.listed,
        job.enqueued,
        job.pulled,
        job.skipped_existing,
        job.failed,
        humansize::format_size(job.bytes, humansize::BINARY),
        or_dash(job.updated_at.as_deref())
    )
}

fn render_backfill(bucket: &str, job: Option<&BackfillJob>, formatter: &Formatter) {
    if formatter.is_json() {
        return;
    }
    formatter
        .println(&formatter.style_name(&format!("Backfill: {}", formatter.sanitize_text(bucket))));
    let Some(job) = job else {
        row(formatter, "Job", "none recorded");
        return;
    };
    row(formatter, "Job", or_dash(Some(&job.job_id)));
    row(formatter, "State", or_dash(Some(&job.state)));
    row(formatter, "Dry run", yes_no(job.dry_run));
    row(formatter, "Prefix", or_dash(job.prefix.as_deref()));
    row(formatter, "Skip existing", job.skip_existing.as_str());
    row(formatter, "Listed", &job.listed.to_string());
    row(formatter, "Enqueued", &job.enqueued.to_string());
    row(formatter, "Pulled", &job.pulled.to_string());
    row(
        formatter,
        "Skipped existing",
        &job.skipped_existing.to_string(),
    );
    row(formatter, "Failed", &job.failed.to_string());
    row(formatter, "Bytes", &bytes_text(job.bytes));
    row(formatter, "Last key", or_dash(job.last_key.as_deref()));
    match &job.last_error {
        Some(error) => {
            let mut text = or_dash(Some(&error.class)).to_string();
            if let Some(hash) = error.key_hash.as_deref() {
                text.push_str(&format!(" (key hash {hash})"));
            }
            if let Some(at) = error.at.as_deref() {
                text.push_str(&format!(" at {at}"));
            }
            row(formatter, "Last error", &text);
        }
        None => row(formatter, "Last error", "none"),
    }
    if !job.failed_keys.is_empty() {
        row(
            formatter,
            "Failed key hashes",
            &job.failed_keys.len().to_string(),
        );
    }
    match &job.owner {
        Some(owner) => {
            let mut text = or_dash(Some(&owner.node)).to_string();
            if let Some(lease) = owner.lease_until.as_deref() {
                text.push_str(&format!(" (lease until {lease})"));
            }
            row(formatter, "Owner", &text);
        }
        None => row(formatter, "Owner", "none"),
    }
    row(formatter, "Started at", or_dash(job.started_at.as_deref()));
    row(formatter, "Updated at", or_dash(job.updated_at.as_deref()));
    row(
        formatter,
        "Config updated at",
        or_dash(job.config_updated_at.as_deref()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputConfig;
    use async_trait::async_trait;
    use clap::Parser;
    use rc_core::admin::{BackfillJobResult, OnDemandMigrationConfigResult};
    use std::sync::Mutex;

    const GET_RESPONSE: &str =
        include_str!("../../../../core/tests/fixtures/on_demand_migration/get_response.json");
    const STATUS: &str =
        include_str!("../../../../core/tests/fixtures/on_demand_migration/status.json");
    const BACKFILL_JOB: &str =
        include_str!("../../../../core/tests/fixtures/on_demand_migration/backfill_job.json");

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: BucketCommands,
    }

    fn formatter() -> Formatter {
        Formatter::new(OutputConfig {
            json: true,
            no_color: true,
            no_progress: true,
            quiet: false,
        })
    }

    fn human_formatter() -> Formatter {
        Formatter::new(OutputConfig {
            json: false,
            no_color: true,
            no_progress: true,
            quiet: false,
        })
    }

    fn set_args(extra: &[&str]) -> SetArgs {
        let mut args = vec![
            "rc",
            "migration",
            "set",
            "local/photos",
            "--provider",
            "minio",
            "--endpoint",
            "https://source.example.com:9000",
            "--region",
            "us-east-1",
            "--source-bucket",
            "legacy-photos",
        ];
        args.extend_from_slice(extra);
        match TestCli::parse_from(args).command {
            BucketCommands::Migration(MigrationCommands::Set(args)) => args,
            _ => panic!("expected set"),
        }
    }

    #[test]
    fn parses_the_full_set_surface_with_snake_case_policy_values() {
        let args = set_args(&[
            "--source-prefix",
            "photos/",
            "--prefix",
            "img/",
            "--access-key",
            "AKIASOURCE",
            "--secret-key",
            "s3cr3t",
            "--path-style",
            "virtual",
            "--skip-tls-verify",
            "--head",
            "local_only",
            "--range-get",
            "serve_only",
            "--source-error",
            "not_found",
            "--no-preserve-etag",
            "--copy-tags",
            "--no-events",
            "--inline-max-bytes",
            "1024",
            "--max-concurrent-pulls",
            "4",
            "--dry-run",
        ]);
        assert_eq!(args.provider, ProviderArg::Minio);
        assert_eq!(args.head, HeadArg::LocalOnly);
        assert_eq!(args.range_get, RangeGetArg::ServeOnly);
        assert_eq!(args.source_error, SourceErrorArg::NotFound);
        assert_eq!(args.path_style, PathStyleArg::Virtual);
        assert!(args.dry_run && args.skip_tls_verify && args.copy_tags);
        assert!(args.no_preserve_etag && args.no_events);
        assert_eq!(args.inline_max_bytes, Some(1024));
        assert_eq!(args.max_concurrent_pulls, Some(4));
        // The secret never appears in the derived Debug output.
        let debug = format!("{args:?}");
        assert!(!debug.contains("s3cr3t"));
        assert!(debug.contains("AKIASOURCE"));

        let request = build_request(&args, Some(Zeroizing::new("s3cr3t".into()))).unwrap();
        assert_eq!(request.policy.head, HeadPolicy::LocalOnly);
        assert_eq!(request.policy.range_get, RangeGetPolicy::ServeOnly);
        assert_eq!(request.policy.source_error, SourceErrorPolicy::NotFound);
        assert!(!request.policy.preserve_etag && request.policy.copy_tags);
        assert!(!request.policy.emit_events);
        assert_eq!(request.policy.inline_max_bytes, 1024);
        assert_eq!(request.policy.max_concurrent_pulls, 4);
        assert!(request.source.tls.skip_verify);
        assert_eq!(request.filter.prefix.as_deref(), Some("img/"));
    }

    #[test]
    fn credential_flags_are_mutually_exclusive_with_public() {
        for args in [
            vec!["--public", "--access-key", "AK"],
            vec!["--public", "--secret-key", "SK"],
            vec!["--secret-key", "SK"],
            vec!["--head", "local-only"],
            vec!["--skip-existing", "always"],
        ] {
            let mut full = vec![
                "rc",
                "migration",
                "set",
                "local/photos",
                "--provider",
                "s3",
                "--endpoint",
                "https://s.example.com",
                "--region",
                "r",
                "--source-bucket",
                "b",
            ];
            full.extend(args.iter());
            assert!(TestCli::try_parse_from(&full).is_err(), "{args:?}");
        }
    }

    #[test]
    fn secret_comes_from_flag_then_environment_and_refuses_the_placeholder() {
        let formatter = formatter();
        let env = |value: &str| Some(std::ffi::OsString::from(value));

        let args = set_args(&["--access-key", "AK", "--secret-key", "from-flag"]);
        assert_eq!(
            resolve_secret(&args, env("ignored"), &formatter)
                .unwrap()
                .unwrap()
                .as_str(),
            "from-flag"
        );

        let args = set_args(&["--access-key", "AK"]);
        assert_eq!(
            resolve_secret(&args, env("from-env\n"), &formatter)
                .unwrap()
                .unwrap()
                .as_str(),
            "from-env"
        );
        let error = resolve_secret(&args, env(REDACTED_SECRET), &formatter).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("placeholder"));
        assert_eq!(
            resolve_secret(&args, env(""), &formatter)
                .unwrap_err()
                .exit_code(),
            2
        );
        // JSON output never prompts.
        let error = resolve_secret(&args, None, &formatter).unwrap_err();
        assert!(error.to_string().contains(SECRET_KEY_ENV));

        let args = set_args(&["--public"]);
        assert!(
            resolve_secret(&args, env("ignored"), &formatter)
                .unwrap()
                .is_none()
        );
        let args = set_args(&[]);
        assert_eq!(
            resolve_secret(&args, None, &formatter)
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn targets_are_alias_slash_bucket_only() {
        assert_eq!(
            parse_target("local/photos").unwrap(),
            ("local".to_string(), "photos".to_string())
        );
        for target in [
            "local",
            "/photos",
            "local/",
            "local/photos/key",
            "local/a b",
        ] {
            assert_eq!(parse_target(target).unwrap_err().exit_code(), 2, "{target}");
        }
    }

    #[test]
    fn ca_cert_must_be_a_bounded_pem_file() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("ca.pem");
        std::fs::write(
            &good,
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(read_ca_cert(&good).is_ok());
        let bad = dir.path().join("bad.pem");
        std::fs::write(&bad, "not a certificate").unwrap();
        assert_eq!(read_ca_cert(&bad).unwrap_err().exit_code(), 2);
        let huge = dir.path().join("huge.pem");
        std::fs::write(&huge, "-".repeat(MAX_ON_DEMAND_MIGRATION_CA_CERT_BYTES + 1)).unwrap();
        assert_eq!(read_ca_cert(&huge).unwrap_err().exit_code(), 2);
        assert_eq!(
            read_ca_cert(&dir.path().join("missing.pem"))
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn set_validates_arguments_before_asking_for_the_secret() {
        // Provider s3 without an endpoint fails before the secret is resolved,
        // so no prompt and no environment lookup happen.
        let command = MigrationCommands::Set(set_args_without_endpoint());
        let error = prepare(command, &formatter()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--endpoint"));
    }

    fn set_args_without_endpoint() -> SetArgs {
        match TestCli::parse_from([
            "rc",
            "migration",
            "set",
            "local/photos",
            "--provider",
            "s3",
            "--region",
            "us-east-1",
            "--source-bucket",
            "b",
            "--access-key",
            "AK",
        ])
        .command
        {
            BucketCommands::Migration(MigrationCommands::Set(args)) => args,
            _ => panic!("expected set"),
        }
    }

    #[test]
    fn human_rendering_uses_an_em_dash_for_a_null_ratio_and_never_prints_secrets() {
        let status: OnDemandMigrationStatus = serde_json::from_str(STATUS).unwrap();
        assert_eq!(ratio_text(status.served_by_source_ratio), NO_VALUE);
        assert_eq!(ratio_text(Some(0.0)), "0.0%");
        assert_eq!(ratio_text(Some(0.4567)), "45.7%");
        assert_eq!(ratio_text(Some(f64::NAN)), NO_VALUE);
        let config: OnDemandMigrationConfigResult = serde_json::from_str(GET_RESPONSE).unwrap();
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains(REDACTED_SECRET));
        let job: BackfillJobResult = serde_json::from_str(BACKFILL_JOB).unwrap();
        let line = backfill_progress_line(job.job.as_ref().unwrap());
        assert!(line.starts_with("[running] listed 2000"));
        assert!(line.contains("pulled 1400"));
        assert!(line.contains("70 MiB"));
    }

    // ---- exit code tests against a fake API ----

    #[derive(Default)]
    struct FakeApi {
        status: Option<OnDemandMigrationStatus>,
        get: Option<OnDemandMigrationConfigResult>,
        backfill: Mutex<Vec<BackfillJobResult>>,
        error: Option<fn() -> Error>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeApi {
        fn fail(&self) -> Result<()> {
            match self.error {
                Some(error) => Err(error()),
                None => Ok(()),
            }
        }
        fn record(&self, call: &'static str) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[async_trait]
    impl OnDemandMigrationApi for FakeApi {
        async fn set_on_demand_migration(
            &self,
            bucket: &str,
            config: &OnDemandMigrationConfigRequest,
            dry_run: bool,
        ) -> Result<OnDemandMigrationSetResult> {
            self.record("set");
            self.fail()?;
            Ok(OnDemandMigrationSetResult {
                bucket: bucket.to_string(),
                dry_run,
                config: Some(OnDemandMigrationConfigView {
                    source: rc_core::admin::SourceView {
                        provider: config.source.provider.as_str().to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                updated_at: (!dry_run).then(|| "2026-09-02T10:00:00Z".to_string()),
                probe: None,
            })
        }
        async fn get_on_demand_migration(
            &self,
            _bucket: &str,
        ) -> Result<OnDemandMigrationConfigResult> {
            self.record("get");
            self.fail()?;
            Ok(self.get.clone().unwrap_or_default())
        }
        async fn delete_on_demand_migration(&self, _bucket: &str) -> Result<()> {
            self.record("delete");
            self.fail()
        }
        async fn on_demand_migration_status(
            &self,
            _bucket: &str,
        ) -> Result<OnDemandMigrationStatus> {
            self.record("status");
            self.fail()?;
            Ok(self.status.clone().unwrap_or_default())
        }
        async fn start_on_demand_migration_backfill(
            &self,
            _bucket: &str,
            _request: &BackfillStartRequest,
        ) -> Result<BackfillJobResult> {
            self.record("backfill_start");
            self.fail()?;
            Ok(self.backfill.lock().unwrap().remove(0))
        }
        async fn cancel_on_demand_migration_backfill(
            &self,
            _bucket: &str,
        ) -> Result<BackfillJobResult> {
            self.record("backfill_cancel");
            self.fail()?;
            Ok(self.backfill.lock().unwrap().remove(0))
        }
        async fn on_demand_migration_backfill_status(
            &self,
            _bucket: &str,
        ) -> Result<BackfillJobResult> {
            self.record("backfill_status");
            self.fail()?;
            Ok(self.backfill.lock().unwrap().remove(0))
        }
    }

    fn prepared(action: Action) -> Prepared {
        Prepared {
            alias: "local".into(),
            bucket: "photos".into(),
            action,
        }
    }

    #[tokio::test]
    async fn status_succeeds_and_json_carries_the_null_ratio() {
        let api = FakeApi {
            status: Some(serde_json::from_str(STATUS).unwrap()),
            ..Default::default()
        };
        let code =
            execute_with_api(prepared(Action::Status { watch: None }), &api, &formatter()).await;
        assert_eq!(code, ExitCode::Success);
        let human = human_formatter();
        let code = execute_with_api(prepared(Action::Status { watch: None }), &api, &human).await;
        assert_eq!(code, ExitCode::Success);
    }

    #[tokio::test]
    async fn unsupported_server_maps_to_exit_7() {
        let api = FakeApi {
            error: Some(|| {
                Error::UnsupportedFeature("server does not support on-demand migration".into())
            }),
            ..Default::default()
        };
        assert_eq!(
            execute_with_api(prepared(Action::Get), &api, &formatter()).await,
            ExitCode::UnsupportedFeature
        );
    }

    #[tokio::test]
    async fn not_found_conflict_network_and_auth_keep_their_exit_codes() {
        for (error, expected) in [
            (
                (|| Error::NotFound("NoSuchConfiguration".into())) as fn() -> Error,
                ExitCode::NotFound,
            ),
            (
                || Error::Conflict("backfill running".into()),
                ExitCode::Conflict,
            ),
            (
                || Error::Network("source unreachable".into()),
                ExitCode::NetworkError,
            ),
            (|| Error::Auth("licence".into()), ExitCode::AuthError),
            (|| Error::Config("bad".into()), ExitCode::UsageError),
        ] {
            let api = FakeApi {
                error: Some(error),
                ..Default::default()
            };
            assert_eq!(
                execute_with_api(prepared(Action::BackfillCancel), &api, &formatter()).await,
                expected
            );
        }
    }

    #[tokio::test]
    async fn remove_reports_success_without_a_body() {
        let api = FakeApi::default();
        assert_eq!(
            execute_with_api(prepared(Action::Remove), &api, &formatter()).await,
            ExitCode::Success
        );
        assert_eq!(*api.calls.lock().unwrap(), vec!["delete"]);
    }

    #[tokio::test]
    async fn backfill_watch_stops_at_a_terminal_state() {
        let running: BackfillJobResult = serde_json::from_str(BACKFILL_JOB).unwrap();
        let mut done = running.clone();
        done.job.as_mut().unwrap().state = "completed".into();
        let api = FakeApi {
            backfill: Mutex::new(vec![running, done]),
            ..Default::default()
        };
        let code = execute_with_api(
            prepared(Action::BackfillStatus {
                watch: Some(Duration::from_millis(10)),
            }),
            &api,
            &formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
        assert_eq!(
            *api.calls.lock().unwrap(),
            vec!["backfill_status", "backfill_status"]
        );
    }

    #[tokio::test]
    async fn backfill_watch_treats_a_missing_job_as_final() {
        let api = FakeApi {
            backfill: Mutex::new(vec![BackfillJobResult::default()]),
            ..Default::default()
        };
        let code = execute_with_api(
            prepared(Action::BackfillStatus {
                watch: Some(Duration::from_millis(10)),
            }),
            &api,
            &human_formatter(),
        )
        .await;
        assert_eq!(code, ExitCode::Success);
        assert_eq!(api.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn json_error_envelope_marks_reads_retryable_and_unsupported_capability() {
        let value = error_output(&Error::Network("down".into()), Operation::Get);
        assert_eq!(value["type"], OUTPUT_TYPE);
        assert_eq!(value["error"]["type"], "network_error");
        assert_eq!(value["error"]["retryable"], true);
        let value = error_output(&Error::Network("probe".into()), Operation::Set);
        assert_eq!(value["error"]["retryable"], false);
        assert!(
            value["error"]["suggestion"]
                .as_str()
                .unwrap()
                .contains("--dry-run")
        );
        let value = error_output(&Error::UnsupportedFeature("nope".into()), Operation::Status);
        assert_eq!(value["error"]["capability"], ON_DEMAND_MIGRATION_CAPABILITY);
        assert_eq!(
            success_output(Operation::BackfillStart, "photos", json!({"a": 1}))["data"]["operation"],
            "backfill_start"
        );
    }
}
