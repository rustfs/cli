//! RustFS KMS inspection and safe key lifecycle commands.

use clap::{Args, Subcommand};
use rc_core::admin::{
    KmsApi, KmsBackendKind, KmsCancelKeyDeletionResult, KmsConfigureRequest, KmsCreateKeyRequest,
    KmsCreateKeyResult, KmsDeleteKeyRequest, KmsDeleteKeyResult, KmsDiagnosticStore, KmsKey,
    KmsKeyPage, KmsKeyState, KmsKeyUsage, KmsRoundTripReport, KmsServiceState, KmsStatus,
    run_kms_round_trip,
};
use rc_core::{Error, Result};
use rc_s3::{AdminClient, S3Client};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, stdin};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use super::{emit_observability_error, get_admin_alias, get_admin_client};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
pub enum KmsCommands {
    /// Display KMS service health and non-secret configuration summary
    Status(KmsStatusArgs),

    /// Inspect KMS keys
    #[command(subcommand)]
    Key(KmsKeyCommands),

    /// Configure KMS from a protected JSON file or standard input
    Configure(KmsConfigureArgs),

    /// Replace KMS configuration from a protected JSON file or standard input
    Reconfigure(KmsConfigureArgs),

    /// Start the configured KMS service
    Start(KmsStartArgs),

    /// Restart the KMS service after explicit confirmation
    Restart(KmsDisruptiveActionArgs),

    /// Stop the KMS service after explicit confirmation
    Stop(KmsDisruptiveActionArgs),

    /// Verify SSE-KMS write and read behavior with a temporary object
    Roundtrip(KmsRoundtripArgs),
}

#[derive(Args, Debug)]
pub struct KmsStatusArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct KmsConfigureArgs {
    /// Alias name of the server
    pub alias: String,

    /// Read the configuration from this protected regular file
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "stdin",
        required_unless_present = "stdin"
    )]
    pub config_file: Option<PathBuf>,

    /// Read the configuration from standard input
    #[arg(
        long,
        conflicts_with = "config_file",
        required_unless_present = "config_file"
    )]
    pub stdin: bool,
}

#[derive(Args, Debug)]
pub struct KmsStartArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct KmsDisruptiveActionArgs {
    /// Alias name of the server
    pub alias: String,

    /// Confirm the disruptive service operation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct KmsRoundtripArgs {
    /// Alias name of the server
    pub alias: String,

    /// Existing bucket used for the temporary diagnostic object
    pub bucket: String,

    /// KMS key identifier; when omitted, use the configured default key
    #[arg(long)]
    pub key_id: Option<String>,

    /// Confirm the temporary encrypted object mutation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum KmsKeyCommands {
    /// List KMS keys
    List(KmsKeyListArgs),

    /// Display key lifecycle status; defaults to the configured KMS key
    Status(KmsKeyStatusArgs),

    /// Create a KMS key without exporting key material
    Create(KmsKeyCreateArgs),

    /// Schedule or immediately delete a KMS key
    Delete(KmsKeyDeleteArgs),

    /// Cancel a scheduled KMS key deletion
    CancelDeletion(KmsKeyCancelDeletionArgs),
}

#[derive(Args, Debug)]
pub struct KmsKeyListArgs {
    /// Alias name of the server
    pub alias: String,

    /// Maximum number of keys to return
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,

    /// Pagination marker returned by a previous request
    #[arg(long)]
    pub marker: Option<String>,
}

#[derive(Args, Debug)]
pub struct KmsKeyStatusArgs {
    /// Alias name of the server
    pub alias: String,

    /// Key identifier; when omitted, use the configured default KMS key
    pub key_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct KmsKeyCreateArgs {
    /// Alias name of the server
    pub alias: String,

    /// Optional friendly key name stored in the reserved name tag
    #[arg(long)]
    pub name: Option<String>,

    /// Optional key description
    #[arg(long)]
    pub description: Option<String>,

    /// Key metadata tag in KEY=VALUE form; may be repeated
    #[arg(long = "tag", value_name = "KEY=VALUE")]
    pub tags: Vec<String>,
}

#[derive(Args, Debug)]
pub struct KmsKeyDeleteArgs {
    /// Alias name of the server
    pub alias: String,

    /// Key identifier
    pub key_id: String,

    /// Number of days before deletion; defaults to 7
    #[arg(
        long,
        value_parser = clap::value_parser!(u32).range(7..=30),
        conflicts_with = "immediate"
    )]
    pub pending_window_days: Option<u32>,

    /// Delete immediately instead of scheduling deletion
    #[arg(long)]
    pub immediate: bool,

    /// Confirm deletion without an interactive prompt
    #[arg(long)]
    pub yes: bool,

    /// Acknowledge that immediate deletion cannot be cancelled
    #[arg(long, requires = "immediate")]
    pub confirm_immediate: bool,
}

#[derive(Args, Debug)]
pub struct KmsKeyCancelDeletionArgs {
    /// Alias name of the server
    pub alias: String,

    /// Key identifier
    pub key_id: String,
}

#[derive(Debug, Serialize)]
struct KmsSuccessOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct KmsStatusData<'a> {
    operation: &'static str,
    #[serde(flatten)]
    service: &'a KmsStatus,
}

#[derive(Debug, Serialize)]
struct KmsKeyListData<'a> {
    operation: &'static str,
    keys: &'a [KmsKey],
    pagination: KmsPagination<'a>,
}

#[derive(Debug, Serialize)]
struct KmsPagination<'a> {
    truncated: bool,
    continuation_token: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct KmsKeyStatusData<'a> {
    operation: &'static str,
    key: &'a KmsKey,
}

#[derive(Debug, Serialize)]
struct KmsKeyCreateData<'a> {
    operation: &'static str,
    key_id: &'a str,
    key: Option<&'a KmsKey>,
}

#[derive(Debug, Serialize)]
struct KmsKeyDeleteData<'a> {
    operation: &'static str,
    key_id: &'a str,
    deletion_date: Option<&'a str>,
    immediate: bool,
}

#[derive(Debug, Serialize)]
struct KmsKeyCancelDeletionData<'a> {
    operation: &'static str,
    key_id: &'a str,
    key: Option<&'a KmsKey>,
}

#[derive(Debug, Serialize)]
struct KmsLifecycleData<'a> {
    operation: &'a str,
    state: &'a KmsServiceState,
}

#[derive(Debug, Serialize)]
struct KmsRoundTripData<'a> {
    operation: &'static str,
    #[serde(flatten)]
    report: &'a KmsRoundTripReport,
}

const MAX_KMS_CONFIG_BYTES: u64 = 1024 * 1024;

pub async fn execute(command: KmsCommands, formatter: &Formatter) -> ExitCode {
    match command {
        KmsCommands::Status(args) => execute_status(args, formatter).await,
        KmsCommands::Configure(args) => execute_configure(args, false, formatter).await,
        KmsCommands::Reconfigure(args) => execute_configure(args, true, formatter).await,
        KmsCommands::Start(args) => execute_start(args, formatter).await,
        KmsCommands::Restart(args) => execute_disruptive_control(args, true, formatter).await,
        KmsCommands::Stop(args) => execute_disruptive_control(args, false, formatter).await,
        KmsCommands::Roundtrip(args) => execute_roundtrip(args, formatter).await,
        KmsCommands::Key(command) => match command {
            KmsKeyCommands::List(args) => execute_key_list(args, formatter).await,
            KmsKeyCommands::Status(args) => execute_key_status(args, formatter).await,
            KmsKeyCommands::Create(args) => execute_key_create(args, formatter).await,
            KmsKeyCommands::Delete(args) => execute_key_delete(args, formatter).await,
            KmsKeyCommands::CancelDeletion(args) => {
                execute_key_cancel_deletion(args, formatter).await
            }
        },
    }
}

async fn execute_roundtrip(args: KmsRoundtripArgs, formatter: &Formatter) -> ExitCode {
    let (bucket, explicit_key_id) = match validate_roundtrip_args(&args) {
        Ok(validated) => validated,
        Err(error) => {
            return emit_kms_error(
                "Refused to run KMS round-trip diagnostic",
                &error,
                false,
                formatter,
            );
        }
    };
    let alias = match get_admin_alias(&args.alias, formatter) {
        Ok(alias) => alias,
        Err(code) => return code,
    };
    let key_id = match explicit_key_id {
        Some(key_id) => key_id,
        None => {
            let admin = match AdminClient::new(&alias) {
                Ok(client) => client,
                Err(_) => {
                    return emit_kms_error(
                        "Failed to resolve the default KMS key",
                        &Error::General("Failed to create KMS administration client".to_string()),
                        false,
                        formatter,
                    );
                }
            };
            let status = match admin.kms_status().await {
                Ok(status) => status,
                Err(error) => {
                    return emit_kms_error(
                        "Failed to resolve the default KMS key",
                        &error,
                        false,
                        formatter,
                    );
                }
            };
            match resolve_roundtrip_key_id(None, &status) {
                Ok(key_id) => key_id,
                Err(error) => {
                    return emit_kms_error(
                        "Failed to resolve the default KMS key",
                        &error,
                        false,
                        formatter,
                    );
                }
            }
        }
    };
    let store = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(_) => {
            return emit_kms_error(
                "Failed to run KMS round-trip diagnostic",
                &Error::General("Failed to create KMS diagnostic storage client".to_string()),
                false,
                formatter,
            );
        }
    };
    execute_roundtrip_with_store(&bucket, &key_id, &store, formatter).await
}

async fn execute_roundtrip_with_store(
    bucket: &str,
    key_id: &str,
    store: &dyn KmsDiagnosticStore,
    formatter: &Formatter,
) -> ExitCode {
    match run_kms_round_trip(store, bucket, key_id).await {
        Ok(report) => {
            if formatter.is_json() {
                formatter.json(&KmsSuccessOutput {
                    schema_version: 3,
                    output_type: "kms",
                    status: "success",
                    data: KmsRoundTripData {
                        operation: "roundtrip",
                        report: &report,
                    },
                });
            } else {
                formatter.println(&format!(
                    "KMS round-trip passed for bucket {} with key {} in {} ms; cleanup passed",
                    formatter.sanitize_text(&report.bucket),
                    formatter.sanitize_text(&report.key_id),
                    report.timings.total_ms
                ));
            }
            ExitCode::Success
        }
        Err(error) => emit_kms_error(
            "KMS round-trip diagnostic failed",
            &error.into_core_error(),
            false,
            formatter,
        ),
    }
}

fn validate_roundtrip_args(args: &KmsRoundtripArgs) -> Result<(String, Option<String>)> {
    if !args.yes {
        return Err(Error::InvalidPath(
            "KMS round-trip diagnostic requires --yes".to_string(),
        ));
    }
    let bucket = validated_nonempty("KMS diagnostic bucket", &args.bucket)?;
    if bucket.contains('/') {
        return Err(Error::InvalidPath(
            "KMS diagnostic target must be a bucket, not an object path".to_string(),
        ));
    }
    let key_id = args
        .key_id
        .as_deref()
        .map(|value| validated_nonempty("KMS key id", value).map(str::to_string))
        .transpose()?;
    Ok((bucket.to_string(), key_id))
}

fn resolve_roundtrip_key_id(explicit_key_id: Option<String>, status: &KmsStatus) -> Result<String> {
    let key_id = explicit_key_id
        .or_else(|| {
            status
                .config
                .as_ref()
                .and_then(|config| config.default_key_id.clone())
        })
        .ok_or_else(|| {
            Error::NotFound(
                "No key id was provided and KMS has no configured default key".to_string(),
            )
        })?;
    Ok(validated_nonempty("KMS key id", &key_id)?.to_string())
}

async fn execute_status(args: KmsStatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_status_with_api(&client, formatter).await
}

async fn execute_status_with_api(api: &dyn KmsApi, formatter: &Formatter) -> ExitCode {
    match api.kms_status().await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&KmsSuccessOutput {
                    schema_version: 3,
                    output_type: "kms",
                    status: "success",
                    data: KmsStatusData {
                        operation: "status",
                        service: &status,
                    },
                });
            } else {
                print_status(&status, formatter);
            }
            ExitCode::Success
        }
        Err(error) => emit_kms_error("Failed to get KMS status", &error, true, formatter),
    }
}

async fn execute_configure(
    args: KmsConfigureArgs,
    reconfigure: bool,
    formatter: &Formatter,
) -> ExitCode {
    let request = match load_configuration(&args, reconfigure) {
        Ok(request) => request,
        Err(error) => {
            return emit_kms_error(
                if reconfigure {
                    "Failed to validate KMS reconfiguration"
                } else {
                    "Failed to validate KMS configuration"
                },
                &error,
                false,
                formatter,
            );
        }
    };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    let result = if reconfigure {
        client.kms_reconfigure(&request).await
    } else {
        client.kms_configure(&request).await
    };
    match result {
        Ok(state) => {
            emit_lifecycle_result(
                if reconfigure {
                    "reconfigure"
                } else {
                    "configure"
                },
                &state,
                formatter,
            );
            ExitCode::Success
        }
        Err(error) => emit_kms_error(
            if reconfigure {
                "Failed to reconfigure KMS"
            } else {
                "Failed to configure KMS"
            },
            &error,
            false,
            formatter,
        ),
    }
}

async fn execute_start(args: KmsStartArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_control_with_api("start", false, &client, formatter).await
}

async fn execute_disruptive_control(
    args: KmsDisruptiveActionArgs,
    restart: bool,
    formatter: &Formatter,
) -> ExitCode {
    let operation = if restart { "restart" } else { "stop" };
    if !args.yes {
        return emit_kms_error(
            &format!("Refused to {operation} KMS"),
            &Error::InvalidPath(format!("KMS {operation} requires --yes")),
            false,
            formatter,
        );
    }
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_control_with_api(operation, restart, &client, formatter).await
}

async fn execute_control_with_api(
    operation: &str,
    restart: bool,
    api: &dyn KmsApi,
    formatter: &Formatter,
) -> ExitCode {
    let result = if operation == "stop" {
        api.kms_stop().await
    } else {
        api.kms_start(restart).await
    };
    match result {
        Ok(state) => {
            emit_lifecycle_result(operation, &state, formatter);
            ExitCode::Success
        }
        Err(error) => emit_kms_error(
            &format!("Failed to {operation} KMS"),
            &error,
            false,
            formatter,
        ),
    }
}

fn load_configuration(args: &KmsConfigureArgs, reconfigure: bool) -> Result<KmsConfigureRequest> {
    let bytes = if args.stdin {
        read_bounded_configuration(stdin().lock())?
    } else {
        let path = args.config_file.as_deref().ok_or_else(|| {
            Error::InvalidPath("Use --config-file or --stdin for KMS configuration".to_string())
        })?;
        open_protected_config(path).and_then(read_bounded_configuration)?
    };
    if bytes.is_empty() {
        return Err(Error::InvalidPath(
            "KMS configuration input cannot be empty".to_string(),
        ));
    }
    let request: KmsConfigureRequest = serde_json::from_slice(bytes.as_slice()).map_err(|_| {
        Error::InvalidPath(
            "KMS configuration must match a strict Local, VaultKV2, or VaultTransit JSON shape"
                .to_string(),
        )
    })?;
    request.validate(reconfigure)?;
    Ok(request)
}

fn open_protected_config(path: &Path) -> Result<File> {
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Error::InvalidPath("Failed to inspect KMS configuration file".to_string()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(Error::InvalidPath(
            "KMS configuration input must be a regular file, not a symlink".to_string(),
        ));
    }
    let file = File::open(path)
        .map_err(|_| Error::InvalidPath("Failed to open KMS configuration file".to_string()))?;
    let file_metadata = file.metadata().map_err(|_| {
        Error::InvalidPath("Failed to inspect opened KMS configuration file".to_string())
    })?;
    if !file_metadata.is_file() {
        return Err(Error::InvalidPath(
            "KMS configuration input must remain a regular file while opening".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(Error::InvalidPath(
                "KMS configuration file changed while being opened".to_string(),
            ));
        }
        if file_metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(
                "KMS configuration file cannot grant group or other permissions".to_string(),
            ));
        }
    }
    Ok(file)
}

fn read_bounded_configuration(reader: impl Read) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(MAX_KMS_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidPath("Failed to read KMS configuration input".to_string()))?;
    if bytes.len() as u64 > MAX_KMS_CONFIG_BYTES {
        return Err(Error::InvalidPath(format!(
            "KMS configuration input exceeds the {} byte limit",
            MAX_KMS_CONFIG_BYTES
        )));
    }
    Ok(bytes)
}

fn emit_lifecycle_result(operation: &str, state: &KmsServiceState, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&KmsSuccessOutput {
            schema_version: 3,
            output_type: "kms",
            status: "success",
            data: KmsLifecycleData { operation, state },
        });
    } else {
        formatter.println(&format!(
            "KMS {operation} completed; state: {}",
            service_state_label(state)
        ));
    }
}

async fn execute_key_list(args: KmsKeyListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_key_list_with_api(args.limit, args.marker.as_deref(), &client, formatter).await
}

async fn execute_key_list_with_api(
    limit: u32,
    marker: Option<&str>,
    api: &dyn KmsApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.kms_list_keys(limit, marker).await {
        Ok(page) => {
            if formatter.is_json() {
                formatter.json(&KmsSuccessOutput {
                    schema_version: 3,
                    output_type: "kms",
                    status: "success",
                    data: KmsKeyListData {
                        operation: "key_list",
                        keys: &page.keys,
                        pagination: KmsPagination {
                            truncated: page.truncated,
                            continuation_token: page.next_marker.as_deref(),
                        },
                    },
                });
            } else {
                print_key_page(&page, formatter);
            }
            ExitCode::Success
        }
        Err(error) => emit_kms_error("Failed to list KMS keys", &error, true, formatter),
    }
}

async fn execute_key_status(args: KmsKeyStatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_key_status_with_api(args.key_id.as_deref(), &client, formatter).await
}

async fn execute_key_status_with_api(
    key_id: Option<&str>,
    api: &dyn KmsApi,
    formatter: &Formatter,
) -> ExitCode {
    match resolve_and_describe_key(key_id, api).await {
        Ok(key) => {
            if formatter.is_json() {
                formatter.json(&KmsSuccessOutput {
                    schema_version: 3,
                    output_type: "kms",
                    status: "success",
                    data: KmsKeyStatusData {
                        operation: "key_status",
                        key: &key,
                    },
                });
            } else {
                print_key(&key, formatter);
            }
            ExitCode::Success
        }
        Err(error) => emit_kms_error("Failed to get KMS key status", &error, false, formatter),
    }
}

async fn execute_key_create(args: KmsKeyCreateArgs, formatter: &Formatter) -> ExitCode {
    let request = match build_create_request(&args) {
        Ok(request) => request,
        Err(error) => {
            return emit_kms_error("Failed to create KMS key", &error, false, formatter);
        }
    };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_key_create_with_api(&request, &client, formatter).await
}

async fn execute_key_create_with_api(
    request: &KmsCreateKeyRequest,
    api: &dyn KmsApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.kms_create_key(request).await {
        Ok(result) => {
            emit_key_create_result(&result, formatter);
            ExitCode::Success
        }
        Err(error) => emit_kms_error("Failed to create KMS key", &error, false, formatter),
    }
}

async fn execute_key_delete(args: KmsKeyDeleteArgs, formatter: &Formatter) -> ExitCode {
    let request = match build_delete_request(&args) {
        Ok(request) => request,
        Err(error) => {
            return emit_kms_error("Refused to delete KMS key", &error, false, formatter);
        }
    };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_key_delete_with_api(&request, &client, formatter).await
}

async fn execute_key_delete_with_api(
    request: &KmsDeleteKeyRequest,
    api: &dyn KmsApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.kms_delete_key(request).await {
        Ok(result) => {
            emit_key_delete_result(&result, formatter);
            ExitCode::Success
        }
        Err(error) => emit_kms_error("Failed to delete KMS key", &error, false, formatter),
    }
}

async fn execute_key_cancel_deletion(
    args: KmsKeyCancelDeletionArgs,
    formatter: &Formatter,
) -> ExitCode {
    let key_id = match validated_nonempty("KMS key id", &args.key_id) {
        Ok(key_id) => key_id,
        Err(error) => {
            return emit_kms_error(
                "Failed to cancel KMS key deletion",
                &error,
                false,
                formatter,
            );
        }
    };
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_key_cancel_deletion_with_api(key_id, &client, formatter).await
}

async fn execute_key_cancel_deletion_with_api(
    key_id: &str,
    api: &dyn KmsApi,
    formatter: &Formatter,
) -> ExitCode {
    match api.kms_cancel_key_deletion(key_id).await {
        Ok(result) => {
            emit_key_cancel_deletion_result(&result, formatter);
            ExitCode::Success
        }
        Err(error) => emit_kms_error(
            "Failed to cancel KMS key deletion",
            &error,
            false,
            formatter,
        ),
    }
}

fn build_create_request(args: &KmsKeyCreateArgs) -> Result<KmsCreateKeyRequest> {
    let name = args
        .name
        .as_deref()
        .map(|name| validated_nonempty("KMS key name", name).map(str::to_string))
        .transpose()?;
    let description = args
        .description
        .as_deref()
        .map(|description| validated_text("KMS key description", description).map(str::to_string))
        .transpose()?;
    let mut tags = BTreeMap::new();
    for tag in &args.tags {
        let (key, value) = tag
            .split_once('=')
            .ok_or_else(|| Error::InvalidPath("KMS tags must use KEY=VALUE syntax".to_string()))?;
        let key = validated_nonempty("KMS tag key", key)?;
        let value = validated_text("KMS tag value", value)?;
        if key == "name" {
            return Err(Error::InvalidPath(
                "The KMS tag key 'name' is reserved; use --name".to_string(),
            ));
        }
        if tags.insert(key.to_string(), value.to_string()).is_some() {
            return Err(Error::InvalidPath(format!("Duplicate KMS tag key: {key}")));
        }
    }
    Ok(KmsCreateKeyRequest {
        name,
        description,
        tags,
    })
}

fn build_delete_request(args: &KmsKeyDeleteArgs) -> Result<KmsDeleteKeyRequest> {
    let key_id = validated_nonempty("KMS key id", &args.key_id)?;
    if !args.yes {
        return Err(Error::InvalidPath(
            "KMS key deletion requires --yes".to_string(),
        ));
    }
    if args.immediate && !args.confirm_immediate {
        return Err(Error::InvalidPath(
            "Immediate KMS key deletion requires --confirm-immediate".to_string(),
        ));
    }
    Ok(KmsDeleteKeyRequest {
        key_id: key_id.to_string(),
        pending_window_in_days: (!args.immediate).then_some(args.pending_window_days.unwrap_or(7)),
        force_immediate: args.immediate,
    })
}

fn validated_nonempty<'a>(label: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::InvalidPath(format!("{label} cannot be empty")));
    }
    validated_text(label, value)
}

fn validated_text<'a>(label: &str, value: &'a str) -> Result<&'a str> {
    if value.chars().any(char::is_control) {
        return Err(Error::InvalidPath(format!(
            "{label} cannot contain control characters"
        )));
    }
    Ok(value)
}

fn emit_key_create_result(result: &KmsCreateKeyResult, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&KmsSuccessOutput {
            schema_version: 3,
            output_type: "kms",
            status: "success",
            data: KmsKeyCreateData {
                operation: "key_create",
                key_id: &result.key_id,
                key: result.key.as_ref(),
            },
        });
    } else {
        formatter.println(&format!(
            "Created KMS key {}",
            formatter.sanitize_text(&result.key_id)
        ));
    }
}

fn emit_key_delete_result(result: &KmsDeleteKeyResult, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&KmsSuccessOutput {
            schema_version: 3,
            output_type: "kms",
            status: "success",
            data: KmsKeyDeleteData {
                operation: "key_delete",
                key_id: &result.key_id,
                deletion_date: result.deletion_date.as_deref(),
                immediate: result.immediate,
            },
        });
    } else if result.immediate {
        formatter.println(&format!(
            "Deleted KMS key {} immediately",
            formatter.sanitize_text(&result.key_id)
        ));
    } else {
        formatter.println(&format!(
            "Scheduled deletion for KMS key {} at {}",
            formatter.sanitize_text(&result.key_id),
            formatter.sanitize_text(result.deletion_date.as_deref().unwrap_or("server default"))
        ));
    }
}

fn emit_key_cancel_deletion_result(result: &KmsCancelKeyDeletionResult, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&KmsSuccessOutput {
            schema_version: 3,
            output_type: "kms",
            status: "success",
            data: KmsKeyCancelDeletionData {
                operation: "key_cancel_deletion",
                key_id: &result.key_id,
                key: result.key.as_ref(),
            },
        });
    } else {
        formatter.println(&format!(
            "Cancelled deletion for KMS key {}",
            formatter.sanitize_text(&result.key_id)
        ));
    }
}

async fn resolve_and_describe_key(key_id: Option<&str>, api: &dyn KmsApi) -> Result<KmsKey> {
    let resolved = match key_id.map(str::trim).filter(|key_id| !key_id.is_empty()) {
        Some(key_id) => key_id.to_string(),
        None => api
            .kms_status()
            .await?
            .config
            .and_then(|config| config.default_key_id)
            .filter(|key_id| !key_id.trim().is_empty())
            .ok_or_else(|| {
                Error::NotFound(
                    "No key id was provided and KMS has no configured default key".to_string(),
                )
            })?,
    };
    api.kms_describe_key(&resolved).await
}

fn emit_kms_error(
    context: &str,
    error: &Error,
    missing_route_is_unsupported: bool,
    formatter: &Formatter,
) -> ExitCode {
    if missing_route_is_unsupported && matches!(error, Error::NotFound(_)) {
        return emit_observability_error(
            "kms",
            "admin.kms",
            context,
            &Error::UnsupportedFeature(
                "The RustFS KMS administration route is unavailable".to_string(),
            ),
            formatter,
        );
    }
    emit_observability_error("kms", "admin.kms", context, error, formatter)
}

fn print_status(status: &KmsStatus, formatter: &Formatter) {
    formatter.println(&formatter.style_name("KMS Status"));
    formatter.println("");
    formatter.println(&format!(
        "State:          {}",
        service_state_label(&status.state)
    ));
    formatter.println(&format!(
        "Backend:        {}",
        status
            .backend
            .as_ref()
            .map(backend_label)
            .unwrap_or_else(|| "not configured".to_string())
    ));
    formatter.println(&format!(
        "Healthy:        {}",
        status
            .healthy
            .map(|healthy| healthy.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    if let Some(config) = &status.config {
        formatter.println(&format!(
            "Default key:    {}",
            formatter.sanitize_text(config.default_key_id.as_deref().unwrap_or("not configured"))
        ));
        formatter.println(&format!("Cache enabled:  {}", config.cache.enabled));
    }
    if let Some(message) = status.error_message.as_deref() {
        formatter.println(&format!(
            "Error:          {}",
            formatter.sanitize_text(message)
        ));
    }
}

fn print_key_page(page: &KmsKeyPage, formatter: &Formatter) {
    if page.keys.is_empty() {
        formatter.println("No KMS keys found");
        return;
    }
    formatter.println("KEY ID                           STATE              USAGE");
    for key in &page.keys {
        formatter.println(&format!(
            "{:<32} {:<18} {}",
            formatter.sanitize_text(&key.key_id),
            key_state_label(&key.state),
            key_usage_label(&key.usage)
        ));
    }
    if page.truncated {
        formatter.println(&format!(
            "Next marker: {}",
            formatter.sanitize_text(page.next_marker.as_deref().unwrap_or("unknown"))
        ));
    }
}

fn print_key(key: &KmsKey, formatter: &Formatter) {
    formatter.println(&formatter.style_name("KMS Key"));
    formatter.println("");
    formatter.println(&format!(
        "Key ID:         {}",
        formatter.sanitize_text(&key.key_id)
    ));
    formatter.println(&format!("State:          {}", key_state_label(&key.state)));
    formatter.println(&format!("Usage:          {}", key_usage_label(&key.usage)));
    formatter.println(&format!(
        "Algorithm:      {}",
        formatter.sanitize_text(key.algorithm.as_deref().unwrap_or("unknown"))
    ));
    formatter.println(&format!(
        "Created at:     {}",
        formatter.sanitize_text(key.created_at.as_deref().unwrap_or("unknown"))
    ));
}

fn service_state_label(state: &KmsServiceState) -> &'static str {
    match state {
        KmsServiceState::NotConfigured => "not-configured",
        KmsServiceState::Configured => "configured",
        KmsServiceState::Running => "running",
        KmsServiceState::Error => "error",
        KmsServiceState::Unknown => "unknown",
    }
}

fn backend_label(backend: &KmsBackendKind) -> String {
    match backend {
        KmsBackendKind::Local => "local",
        KmsBackendKind::VaultKv2 => "vault-kv2",
        KmsBackendKind::VaultTransit => "vault-transit",
        KmsBackendKind::Unknown => "unknown",
    }
    .to_string()
}

fn key_state_label(state: &KmsKeyState) -> &'static str {
    match state {
        KmsKeyState::Enabled => "enabled",
        KmsKeyState::Active => "active",
        KmsKeyState::Disabled => "disabled",
        KmsKeyState::PendingDeletion => "pending-deletion",
        KmsKeyState::PendingImport => "pending-import",
        KmsKeyState::Unavailable => "unavailable",
        KmsKeyState::Deleted => "deleted",
        KmsKeyState::Unknown => "unknown",
    }
}

fn key_usage_label(usage: &KmsKeyUsage) -> &'static str {
    match usage {
        KmsKeyUsage::EncryptDecrypt => "encrypt-decrypt",
        KmsKeyUsage::SignVerify => "sign-verify",
        KmsKeyUsage::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rc_core::RemotePath;
    use std::sync::Mutex;

    struct FakeDiagnosticStore {
        permission_denied: bool,
        uploaded: Mutex<Vec<u8>>,
        cleanup_attempts: Mutex<u32>,
    }

    #[async_trait]
    impl KmsDiagnosticStore for FakeDiagnosticStore {
        async fn put_kms_diagnostic_object(
            &self,
            _path: &RemotePath,
            content: Zeroizing<Vec<u8>>,
            _key_id: &str,
        ) -> Result<()> {
            if self.permission_denied {
                return Err(Error::Auth("SECRET_DETAIL".to_string()));
            }
            *self.uploaded.lock().expect("uploaded content lock") = content.to_vec();
            Ok(())
        }

        async fn get_kms_diagnostic_object(
            &self,
            _path: &RemotePath,
            _max_bytes: usize,
        ) -> Result<Zeroizing<Vec<u8>>> {
            Ok(Zeroizing::new(
                self.uploaded.lock().expect("uploaded content lock").clone(),
            ))
        }

        async fn delete_kms_diagnostic_object(&self, _path: &RemotePath) -> Result<()> {
            *self.cleanup_attempts.lock().expect("cleanup counter lock") += 1;
            Ok(())
        }
    }

    fn fake_store(permission_denied: bool) -> FakeDiagnosticStore {
        FakeDiagnosticStore {
            permission_denied,
            uploaded: Mutex::new(Vec::new()),
            cleanup_attempts: Mutex::new(0),
        }
    }

    #[tokio::test]
    async fn roundtrip_command_has_success_and_auth_exit_codes() {
        let formatter = Formatter::new(crate::output::OutputConfig {
            quiet: true,
            ..Default::default()
        });
        let success = fake_store(false);
        assert_eq!(
            execute_roundtrip_with_store("bucket", "key-1", &success, &formatter).await,
            ExitCode::Success
        );
        assert_eq!(
            *success
                .cleanup_attempts
                .lock()
                .expect("cleanup counter lock"),
            1
        );

        let denied = fake_store(true);
        assert_eq!(
            execute_roundtrip_with_store("bucket", "key-1", &denied, &formatter).await,
            ExitCode::AuthError
        );
        assert_eq!(
            *denied
                .cleanup_attempts
                .lock()
                .expect("cleanup counter lock"),
            1
        );
    }

    #[test]
    fn roundtrip_requires_confirmation_and_resolves_default_key() {
        let args = KmsRoundtripArgs {
            alias: "local".to_string(),
            bucket: "diagnostic-bucket".to_string(),
            key_id: None,
            yes: false,
        };
        validate_roundtrip_args(&args).expect_err("missing confirmation should fail");

        let status = KmsStatus {
            state: KmsServiceState::Running,
            backend: None,
            healthy: Some(true),
            error_message: None,
            config: Some(rc_core::admin::KmsConfigSummary {
                backend: KmsBackendKind::Local,
                default_key_id: Some("default-key".to_string()),
                timeout_seconds: None,
                retry_attempts: None,
                cache: rc_core::admin::KmsCacheSummary {
                    enabled: false,
                    max_keys: None,
                    ttl_seconds: None,
                    metrics_enabled: None,
                },
                endpoint: None,
                auth_method: None,
                credentials_configured: None,
                tls_verification_disabled: None,
            }),
        };
        assert_eq!(
            resolve_roundtrip_key_id(None, &status).expect("default key should resolve"),
            "default-key"
        );
    }

    #[test]
    fn roundtrip_json_matches_v3_and_contains_no_probe_artifacts() {
        let report = KmsRoundTripReport {
            bucket: "diagnostic-bucket".to_string(),
            key_id: "key-1".to_string(),
            passed: true,
            cleanup_passed: true,
            timings: rc_core::admin::KmsRoundTripTimings {
                write_ms: 1,
                read_ms: 2,
                cleanup_ms: 3,
                total_ms: 6,
            },
        };
        let output = KmsSuccessOutput {
            schema_version: 3,
            output_type: "kms",
            status: "success",
            data: KmsRoundTripData {
                operation: "roundtrip",
                report: &report,
            },
        };
        let value = serde_json::to_value(output).expect("roundtrip output should serialize");
        let schema_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/output_v3.json");
        let schema: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&schema_path).unwrap_or_else(|error| {
                panic!("failed to read {}: {error}", schema_path.display())
            }),
        )
        .expect("output v3 schema should parse");
        let validator = jsonschema::validator_for(&schema).expect("output v3 should compile");
        let errors = validator
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "invalid v3 output: {}",
            errors.join("\n")
        );

        let serialized = value.to_string();
        for forbidden in ["object_name", "content", "ciphertext", "digest", "data_key"] {
            assert!(!serialized.contains(forbidden));
        }
    }
}
