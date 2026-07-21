//! Read-only RustFS KMS inspection commands.

use clap::{Args, Subcommand};
use rc_core::admin::{
    KmsApi, KmsBackendKind, KmsKey, KmsKeyPage, KmsKeyState, KmsKeyUsage, KmsServiceState,
    KmsStatus,
};
use rc_core::{Error, Result};
use serde::Serialize;

use super::{emit_observability_error, get_admin_client};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
pub enum KmsCommands {
    /// Display KMS service health and non-secret configuration summary
    Status(KmsStatusArgs),

    /// Inspect KMS keys
    #[command(subcommand)]
    Key(KmsKeyCommands),
}

#[derive(Args, Debug)]
pub struct KmsStatusArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(Subcommand, Debug)]
pub enum KmsKeyCommands {
    /// List KMS keys
    List(KmsKeyListArgs),

    /// Display key lifecycle status; defaults to the configured KMS key
    Status(KmsKeyStatusArgs),
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

pub async fn execute(command: KmsCommands, formatter: &Formatter) -> ExitCode {
    match command {
        KmsCommands::Status(args) => execute_status(args, formatter).await,
        KmsCommands::Key(command) => match command {
            KmsKeyCommands::List(args) => execute_key_list(args, formatter).await,
            KmsKeyCommands::Status(args) => execute_key_status(args, formatter).await,
        },
    }
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
