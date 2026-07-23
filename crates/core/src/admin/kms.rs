//! Typed contracts for RustFS KMS administration.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use zeroize::Zeroize;

use crate::error::Result;

/// Runtime state of the RustFS KMS service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsServiceState {
    NotConfigured,
    Configured,
    Running,
    Error,
    Unknown,
}

/// Configured KMS backend family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsBackendKind {
    Local,
    VaultKv2,
    VaultTransit,
    Unknown,
}

/// Non-secret KMS cache configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsCacheSummary {
    pub enabled: bool,
    pub max_keys: Option<u64>,
    pub ttl_seconds: Option<u64>,
    pub metrics_enabled: Option<bool>,
}

/// Non-secret KMS configuration summary returned by RustFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsConfigSummary {
    pub backend: KmsBackendKind,
    pub default_key_id: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u32>,
    pub cache: KmsCacheSummary,
    pub endpoint: Option<String>,
    pub auth_method: Option<String>,
    pub credentials_configured: Option<bool>,
    pub tls_verification_disabled: Option<bool>,
}

/// KMS health and configuration state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsStatus {
    pub state: KmsServiceState,
    pub backend: Option<KmsBackendKind>,
    pub healthy: Option<bool>,
    pub error_message: Option<String>,
    pub config: Option<KmsConfigSummary>,
}

/// KMS key state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsKeyState {
    Enabled,
    Active,
    Disabled,
    PendingDeletion,
    PendingImport,
    Unavailable,
    Deleted,
    Unknown,
}

/// KMS key usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KmsKeyUsage {
    EncryptDecrypt,
    SignVerify,
    Unknown,
}

/// A normalized KMS key returned by list or describe operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsKey {
    pub key_id: String,
    pub state: KmsKeyState,
    pub usage: KmsKeyUsage,
    pub description: Option<String>,
    pub algorithm: Option<String>,
    pub version: Option<u32>,
    pub created_at: Option<String>,
    pub deletion_date: Option<String>,
    pub rotated_at: Option<String>,
    pub origin: Option<String>,
    pub manager: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// One page of KMS keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsKeyPage {
    pub keys: Vec<KmsKey>,
    pub truncated: bool,
    pub next_marker: Option<String>,
}

/// Metadata used to create a KMS key without exposing key material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsCreateKeyRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// Result returned after creating a KMS key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsCreateKeyResult {
    pub key_id: String,
    pub key: Option<KmsKey>,
}

/// Request to schedule or immediately delete a KMS key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsDeleteKeyRequest {
    pub key_id: String,
    pub pending_window_in_days: Option<u32>,
    pub force_immediate: bool,
}

/// Result returned after requesting KMS key deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsDeleteKeyResult {
    pub key_id: String,
    pub deletion_date: Option<String>,
    pub immediate: bool,
}

/// Result returned after cancelling scheduled KMS key deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KmsCancelKeyDeletionResult {
    pub key_id: String,
    pub key: Option<KmsKey>,
}

/// Strict Local backend configuration accepted by RustFS beta.10.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KmsLocalConfigureRequest {
    pub key_dir: PathBuf,
    pub master_key: Option<String>,
    pub file_permissions: Option<u32>,
    pub default_key_id: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u32>,
    pub enable_cache: Option<bool>,
    pub max_cached_keys: Option<usize>,
    pub cache_ttl_seconds: Option<u64>,
    pub allow_insecure_dev_defaults: Option<bool>,
}

/// Vault authentication configuration. This type intentionally has no Debug implementation.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum KmsVaultAuthMethod {
    Token { token: String },
    AppRole { role_id: String, secret_id: String },
}

/// Strict Vault KV2 backend configuration accepted by RustFS beta.10.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KmsVaultKv2ConfigureRequest {
    pub address: String,
    pub auth_method: KmsVaultAuthMethod,
    pub namespace: Option<String>,
    pub mount_path: Option<String>,
    pub kv_mount: Option<String>,
    pub key_path_prefix: Option<String>,
    pub skip_tls_verify: Option<bool>,
    pub default_key_id: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u32>,
    pub enable_cache: Option<bool>,
    pub max_cached_keys: Option<usize>,
    pub cache_ttl_seconds: Option<u64>,
    pub allow_insecure_dev_defaults: Option<bool>,
}

/// Strict Vault Transit backend configuration accepted by RustFS beta.10.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KmsVaultTransitConfigureRequest {
    pub address: String,
    pub auth_method: KmsVaultAuthMethod,
    pub namespace: Option<String>,
    pub mount_path: Option<String>,
    pub skip_tls_verify: Option<bool>,
    pub default_key_id: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u32>,
    pub enable_cache: Option<bool>,
    pub max_cached_keys: Option<usize>,
    pub cache_ttl_seconds: Option<u64>,
    pub allow_insecure_dev_defaults: Option<bool>,
}

/// Sensitive KMS configuration request. It intentionally cannot be formatted with Debug.
#[derive(Serialize, Deserialize)]
#[serde(tag = "backend_type")]
pub enum KmsConfigureRequest {
    Local(KmsLocalConfigureRequest),
    #[serde(rename = "VaultKV2")]
    VaultKv2(KmsVaultKv2ConfigureRequest),
    VaultTransit(KmsVaultTransitConfigureRequest),
}

impl KmsConfigureRequest {
    /// Validate server request shape and security invariants without exposing values.
    pub fn validate(&self, allow_existing_credentials: bool) -> Result<()> {
        match self {
            Self::Local(request) => validate_local_configuration(request),
            Self::VaultKv2(request) => {
                validate_vault_kv2_configuration(request, allow_existing_credentials)
            }
            Self::VaultTransit(request) => {
                validate_vault_transit_configuration(request, allow_existing_credentials)
            }
        }
    }

    fn zeroize_sensitive(&mut self) {
        match self {
            Self::Local(request) => request.master_key.zeroize(),
            Self::VaultKv2(request) => request.auth_method.zeroize_sensitive(),
            Self::VaultTransit(request) => request.auth_method.zeroize_sensitive(),
        }
    }
}

impl Drop for KmsConfigureRequest {
    fn drop(&mut self) {
        self.zeroize_sensitive();
    }
}

impl KmsVaultAuthMethod {
    fn zeroize_sensitive(&mut self) {
        match self {
            Self::Token { token } => token.zeroize(),
            Self::AppRole { role_id, secret_id } => {
                role_id.zeroize();
                secret_id.zeroize();
            }
        }
    }
}

fn validate_local_configuration(request: &KmsLocalConfigureRequest) -> Result<()> {
    validate_common_configuration(
        request.timeout_seconds,
        request.retry_attempts,
        request.enable_cache,
        request.max_cached_keys,
        request.cache_ttl_seconds,
    )?;
    validate_optional_text("Default KMS key id", request.default_key_id.as_deref())?;
    if !request.key_dir.is_absolute() {
        return Err(crate::Error::InvalidPath(
            "Local KMS key directory must be an absolute path".to_string(),
        ));
    }
    let allow_insecure = request.allow_insecure_dev_defaults.unwrap_or(false);
    if !allow_insecure && request.master_key.as_deref().is_none_or(str::is_empty) {
        return Err(crate::Error::InvalidPath(
            "Local KMS requires a master key outside explicit development mode".to_string(),
        ));
    }
    if request
        .master_key
        .as_deref()
        .is_some_and(|value| value.chars().any(char::is_control))
    {
        return Err(crate::Error::InvalidPath(
            "Local KMS master key contains invalid characters".to_string(),
        ));
    }
    if request.file_permissions.is_some_and(|mode| mode > 0o777) {
        return Err(crate::Error::InvalidPath(
            "Local KMS file permissions must be an octal mode between 000 and 777".to_string(),
        ));
    }
    if !allow_insecure
        && request
            .file_permissions
            .is_some_and(|mode| mode & 0o077 != 0)
    {
        return Err(crate::Error::InvalidPath(
            "Local KMS key files cannot grant group or other permissions".to_string(),
        ));
    }
    Ok(())
}

fn validate_vault_kv2_configuration(
    request: &KmsVaultKv2ConfigureRequest,
    allow_existing_credentials: bool,
) -> Result<()> {
    validate_vault_configuration(
        "Vault KV2",
        &request.address,
        &request.auth_method,
        request.mount_path.as_deref(),
        request.skip_tls_verify,
        request.allow_insecure_dev_defaults,
        request.timeout_seconds,
        request.retry_attempts,
        request.enable_cache,
        request.max_cached_keys,
        request.cache_ttl_seconds,
        allow_existing_credentials,
    )?;
    validate_optional_text("Vault KV mount", request.kv_mount.as_deref())?;
    validate_optional_text("Vault key path prefix", request.key_path_prefix.as_deref())?;
    validate_optional_text("Default KMS key id", request.default_key_id.as_deref())
}

fn validate_vault_transit_configuration(
    request: &KmsVaultTransitConfigureRequest,
    allow_existing_credentials: bool,
) -> Result<()> {
    validate_vault_configuration(
        "Vault Transit",
        &request.address,
        &request.auth_method,
        request.mount_path.as_deref(),
        request.skip_tls_verify,
        request.allow_insecure_dev_defaults,
        request.timeout_seconds,
        request.retry_attempts,
        request.enable_cache,
        request.max_cached_keys,
        request.cache_ttl_seconds,
        allow_existing_credentials,
    )?;
    validate_optional_text("Default KMS key id", request.default_key_id.as_deref())
}

#[allow(clippy::too_many_arguments)]
fn validate_vault_configuration(
    backend: &str,
    address: &str,
    auth_method: &KmsVaultAuthMethod,
    mount_path: Option<&str>,
    skip_tls_verify: Option<bool>,
    allow_insecure_dev_defaults: Option<bool>,
    timeout_seconds: Option<u64>,
    retry_attempts: Option<u32>,
    enable_cache: Option<bool>,
    max_cached_keys: Option<usize>,
    cache_ttl_seconds: Option<u64>,
    allow_existing_credentials: bool,
) -> Result<()> {
    validate_common_configuration(
        timeout_seconds,
        retry_attempts,
        enable_cache,
        max_cached_keys,
        cache_ttl_seconds,
    )?;
    validate_optional_text("Vault mount path", mount_path)?;
    let parsed = url::Url::parse(address).map_err(|_| {
        crate::Error::InvalidPath(format!(
            "{backend} address must be a valid HTTP or HTTPS URL"
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(crate::Error::InvalidPath(format!(
            "{backend} address must be a valid HTTP or HTTPS URL"
        )));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(crate::Error::InvalidPath(format!(
            "{backend} address cannot contain credentials, a query, or a fragment"
        )));
    }
    let allow_insecure = allow_insecure_dev_defaults.unwrap_or(false);
    if !allow_insecure && parsed.scheme() != "https" {
        return Err(crate::Error::InvalidPath(format!(
            "{backend} requires HTTPS outside explicit development mode"
        )));
    }
    if !allow_insecure && skip_tls_verify.unwrap_or(false) {
        return Err(crate::Error::InvalidPath(format!(
            "{backend} cannot skip TLS verification outside explicit development mode"
        )));
    }
    validate_vault_auth(auth_method, allow_existing_credentials, allow_insecure)
}

fn validate_vault_auth(
    auth_method: &KmsVaultAuthMethod,
    allow_existing_credentials: bool,
    allow_insecure: bool,
) -> Result<()> {
    match auth_method {
        KmsVaultAuthMethod::Token { token } if token.is_empty() && allow_existing_credentials => {
            Ok(())
        }
        KmsVaultAuthMethod::Token { token } if token.is_empty() => Err(crate::Error::InvalidPath(
            "Vault token cannot be empty for initial configuration".to_string(),
        )),
        KmsVaultAuthMethod::Token { token } if token.chars().any(char::is_control) => Err(
            crate::Error::InvalidPath("Vault token contains invalid characters".to_string()),
        ),
        KmsVaultAuthMethod::Token { token } if token == "dev-token" && !allow_insecure => {
            Err(crate::Error::InvalidPath(
                "Vault development token requires explicit development mode".to_string(),
            ))
        }
        KmsVaultAuthMethod::Token { .. } => Ok(()),
        KmsVaultAuthMethod::AppRole { role_id, secret_id }
            if role_id.is_empty() || secret_id.is_empty() =>
        {
            Err(crate::Error::InvalidPath(
                "Vault AppRole id and secret cannot be empty".to_string(),
            ))
        }
        KmsVaultAuthMethod::AppRole { role_id, secret_id }
            if role_id.chars().any(char::is_control) || secret_id.chars().any(char::is_control) =>
        {
            Err(crate::Error::InvalidPath(
                "Vault AppRole credentials contain invalid characters".to_string(),
            ))
        }
        KmsVaultAuthMethod::AppRole { .. } => Ok(()),
    }
}

fn validate_common_configuration(
    timeout_seconds: Option<u64>,
    retry_attempts: Option<u32>,
    enable_cache: Option<bool>,
    max_cached_keys: Option<usize>,
    cache_ttl_seconds: Option<u64>,
) -> Result<()> {
    if timeout_seconds == Some(0) {
        return Err(crate::Error::InvalidPath(
            "KMS timeout must be greater than zero".to_string(),
        ));
    }
    if retry_attempts == Some(0) {
        return Err(crate::Error::InvalidPath(
            "KMS retry attempts must be greater than zero".to_string(),
        ));
    }
    if enable_cache.unwrap_or(true) && max_cached_keys == Some(0) {
        return Err(crate::Error::InvalidPath(
            "KMS cache size must be greater than zero when caching is enabled".to_string(),
        ));
    }
    if cache_ttl_seconds == Some(0) {
        return Err(crate::Error::InvalidPath(
            "KMS cache TTL must be greater than zero".to_string(),
        ));
    }
    Ok(())
}

fn validate_optional_text(label: &str, value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| value.is_empty() || value.chars().any(char::is_control)) {
        return Err(crate::Error::InvalidPath(format!(
            "{label} cannot be empty or contain control characters"
        )));
    }
    Ok(())
}

/// RustFS KMS administration operations.
#[async_trait]
pub trait KmsApi: Send + Sync {
    async fn kms_status(&self) -> Result<KmsStatus>;
    async fn kms_list_keys(&self, limit: u32, marker: Option<&str>) -> Result<KmsKeyPage>;
    async fn kms_describe_key(&self, key_id: &str) -> Result<KmsKey>;
    async fn kms_create_key(&self, request: &KmsCreateKeyRequest) -> Result<KmsCreateKeyResult>;
    async fn kms_delete_key(&self, request: &KmsDeleteKeyRequest) -> Result<KmsDeleteKeyResult>;
    async fn kms_cancel_key_deletion(&self, key_id: &str) -> Result<KmsCancelKeyDeletionResult>;
    async fn kms_configure(&self, request: &KmsConfigureRequest) -> Result<KmsServiceState>;
    async fn kms_reconfigure(&self, request: &KmsConfigureRequest) -> Result<KmsServiceState>;
    async fn kms_start(&self, force: bool) -> Result<KmsServiceState>;
    async fn kms_stop(&self) -> Result<KmsServiceState>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_readable_states_are_stable() {
        assert_eq!(
            serde_json::to_string(&KmsServiceState::NotConfigured)
                .expect("service state should serialize"),
            "\"not-configured\""
        );
        assert_eq!(
            serde_json::to_string(&KmsKeyState::PendingDeletion)
                .expect("key state should serialize"),
            "\"pending-deletion\""
        );
    }

    #[test]
    fn configure_requests_validate_all_native_backend_shapes() {
        let local: KmsConfigureRequest = serde_json::from_value(serde_json::json!({
            "backend_type": "Local",
            "key_dir": std::env::temp_dir(),
            "master_key": "local-secret",
            "file_permissions": 384
        }))
        .expect("valid Local KMS configuration shape");
        local
            .validate(false)
            .expect("secure Local KMS configuration");

        for raw in [
            r#"{"backend_type":"VaultKV2","address":"https://vault.example","auth_method":{"Token":{"token":"vault-secret"}},"mount_path":"transit","kv_mount":"secret","key_path_prefix":"rustfs/kms/keys"}"#,
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"AppRole":{"role_id":"role","secret_id":"secret"}},"mount_path":"transit"}"#,
        ] {
            let request: KmsConfigureRequest =
                serde_json::from_str(raw).expect("valid KMS configuration shape");
            request.validate(false).expect("secure KMS configuration");
        }
    }

    #[test]
    fn configure_requests_reject_unknown_insecure_and_missing_fields() {
        for raw in [
            r#"{"backend_type":"Local","key_dir":"relative","master_key":"secret"}"#,
            r#"{"backend_type":"VaultKV2","address":"http://vault.example","auth_method":{"Token":{"token":"secret"}}}"#,
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"Token":{"token":""}}}"#,
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"Token":{"token":"dev-token"}}}"#,
        ] {
            let request: KmsConfigureRequest =
                serde_json::from_str(raw).expect("request shape should deserialize");
            request
                .validate(false)
                .expect_err("request should be rejected");
        }
        let unknown = r#"{"backend_type":"Local","key_dir":"/var/lib/kms","master_key":"secret","unknown":"secret-value"}"#;
        assert!(
            serde_json::from_str::<KmsConfigureRequest>(unknown).is_err(),
            "unknown configuration fields should fail"
        );
    }

    #[test]
    fn configure_requests_reject_vault_address_credential_channels() {
        for address in [
            "https://user@vault.example",
            "https://user:password@vault.example",
            "https://vault.example?token=hidden",
            "https://vault.example#hidden",
        ] {
            let raw = serde_json::json!({
                "backend_type": "VaultTransit",
                "address": address,
                "auth_method": {"Token": {"token": "vault-secret"}}
            });
            let request: KmsConfigureRequest =
                serde_json::from_value(raw).expect("request shape should deserialize");
            let error = request
                .validate(false)
                .expect_err("address credential channel should be rejected");
            let message = error.to_string();
            assert!(!message.contains("user"));
            assert!(!message.contains("password"));
            assert!(!message.contains("hidden"));
        }
    }

    #[test]
    fn reconfigure_only_allows_an_empty_token_to_reuse_credentials() {
        let empty_token: KmsConfigureRequest = serde_json::from_str(
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"Token":{"token":""}}}"#,
        )
        .expect("empty token request should deserialize");
        empty_token
            .validate(true)
            .expect("reconfigure may reuse the stored token");

        for raw in [
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"AppRole":{"role_id":"","secret_id":"secret"}}}"#,
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"AppRole":{"role_id":"role","secret_id":""}}}"#,
        ] {
            let request: KmsConfigureRequest =
                serde_json::from_str(raw).expect("partial AppRole request should deserialize");
            request
                .validate(true)
                .expect_err("partial AppRole credentials cannot be reused");
        }
    }

    #[test]
    fn configure_request_zeroizes_owned_secret_fields() {
        let mut request: KmsConfigureRequest = serde_json::from_str(
            r#"{"backend_type":"VaultTransit","address":"https://vault.example","auth_method":{"AppRole":{"role_id":"role-id","secret_id":"secret-id"}}}"#,
        )
        .expect("request should deserialize");
        request.zeroize_sensitive();
        let serialized = serde_json::to_string(&request).expect("request should serialize");
        assert!(!serialized.contains("role-id"));
        assert!(!serialized.contains("secret-id"));
    }
}
