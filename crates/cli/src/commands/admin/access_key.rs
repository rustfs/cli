//! Access key inspection commands.
//!
//! Commands for resolving an access key to its IAM identity type and metadata.

use clap::{Subcommand, ValueEnum};
use serde::Serialize;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::Error;
use rc_core::admin::{
    AccessKeyDetails, AccessKeyInfo, AccessKeyListType, AccessKeyProvider, AccessKeyRecord,
    AdminApi, BulkAccessKeyApi, BulkAccessKeyQuery, CapabilityApi, CapabilityAvailability,
    IAM_ACCESS_KEYS_BULK_CAPABILITY, MAX_IAM_ACCESS_KEY_RESULTS, MAX_IAM_ACCESS_KEY_SELECTORS,
    OpenIdAccessKeyInfo,
};

const DEFAULT_PAGE_LIMIT: usize = 1_000;
const DEFAULT_REQUEST_BATCH_SIZE: usize = 100;

/// Access key inspection subcommands.
#[derive(Subcommand, Debug)]
pub enum AccessKeyCommands {
    /// Get access key information
    Info(InfoArgs),

    /// List access keys across capability-advertised identity providers
    #[command(name = "ls", alias = "list")]
    List(ListArgs),
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,

    /// Access key to inspect
    pub access_key: String,
}

#[derive(clap::Args, Debug, Clone)]
pub struct ListArgs {
    /// Alias name of the server
    pub alias: String,

    /// Provider scope; repeat to inspect multiple providers
    #[arg(long, value_enum, value_delimiter = ',', default_value = "builtin")]
    pub provider: Vec<ProviderArg>,

    /// Parent identity to inspect; may be repeated
    #[arg(long, conflicts_with = "all")]
    pub user: Vec<String>,

    /// Inspect all parent identities visible to the caller
    #[arg(long)]
    pub all: bool,

    /// Access-key class to include
    #[arg(long, value_enum, default_value = "all")]
    pub key_type: KeyTypeArg,

    /// Zero-based offset into the deterministic aggregate result
    #[arg(long, default_value_t = 0)]
    pub offset: usize,

    /// Maximum records emitted in this output page
    #[arg(long, default_value_t = DEFAULT_PAGE_LIMIT)]
    pub limit: usize,

    /// Maximum user selectors sent in one bounded server request
    #[arg(long, default_value_t = DEFAULT_REQUEST_BATCH_SIZE)]
    pub request_batch_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    Builtin,
    Ldap,
    Openid,
}

impl From<ProviderArg> for AccessKeyProvider {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Builtin => Self::Builtin,
            ProviderArg::Ldap => Self::Ldap,
            ProviderArg::Openid => Self::Openid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeyTypeArg {
    All,
    #[value(name = "users-only")]
    UsersOnly,
    Sts,
    #[value(name = "service-account")]
    ServiceAccount,
}

impl From<KeyTypeArg> for AccessKeyListType {
    fn from(value: KeyTypeArg) -> Self {
        match value {
            KeyTypeArg::All => Self::All,
            KeyTypeArg::UsersOnly => Self::UsersOnly,
            KeyTypeArg::Sts => Self::StsOnly,
            KeyTypeArg::ServiceAccount => Self::ServiceAccountsOnly,
        }
    }
}

/// Execute an access key subcommand.
pub async fn execute(cmd: AccessKeyCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        AccessKeyCommands::Info(args) => execute_info(args, formatter).await,
        AccessKeyCommands::List(args) => execute_list(args, formatter).await,
    }
}

async fn execute_list(args: ListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    execute_list_with_apis(args, &client, &client, formatter).await
}

async fn execute_list_with_apis(
    args: ListArgs,
    capabilities: &dyn CapabilityApi,
    api: &dyn BulkAccessKeyApi,
    formatter: &Formatter,
) -> ExitCode {
    if let Err(error) = validate_list_args(&args) {
        return emit_list_error(&error, None, None, formatter);
    }

    let mut providers = args
        .provider
        .iter()
        .copied()
        .map(AccessKeyProvider::from)
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();

    let report = match capabilities.discover_capabilities(false).await {
        Ok(report) => report,
        Err(error) => {
            let error = sanitized_access_key_error(&error);
            return emit_list_error(
                &error,
                Some("Failed to discover bulk access-key capabilities"),
                None,
                formatter,
            );
        }
    };

    let mut records = Vec::new();
    let mut failures = Vec::new();
    for provider in providers {
        let capability = provider.capability();
        match report.capability(capability) {
            Some(entry) if entry.availability == CapabilityAvailability::Available => {}
            Some(entry) if entry.availability == CapabilityAvailability::PermissionDenied => {
                failures.push(AccessKeyFailure::new(
                    provider,
                    "capability",
                    &Error::Auth("Provider capability is permission-denied".to_string()),
                ));
                continue;
            }
            _ => {
                failures.push(AccessKeyFailure::new(
                    provider,
                    "capability",
                    &Error::UnsupportedFeature(
                        "Provider bulk access-key route was not advertised as available"
                            .to_string(),
                    ),
                ));
                continue;
            }
        }

        let batches = selector_batches(&args.user, args.request_batch_size);
        for users in batches {
            let query = BulkAccessKeyQuery {
                provider,
                users: users.to_vec(),
                all: args.all,
                list_type: args.key_type.into(),
            };
            match api.list_access_keys_bulk(&query).await {
                Ok(mut batch_records) => records.append(&mut batch_records),
                Err(error) => {
                    let targets = if users.is_empty() {
                        vec![if args.all { "all" } else { "self" }]
                    } else {
                        users.iter().map(String::as_str).collect()
                    };
                    failures.extend(
                        targets
                            .into_iter()
                            .map(|target| AccessKeyFailure::new(provider, target, &error)),
                    );
                }
            }
        }
    }

    if records.len() > MAX_IAM_ACCESS_KEY_RESULTS {
        return emit_list_error(
            &Error::General(format!(
                "aggregate bulk access-key result exceeds {MAX_IAM_ACCESS_KEY_RESULTS} records"
            )),
            Some("RustFS returned too many aggregate access-key records"),
            None,
            formatter,
        );
    }

    records.sort_by(|left, right| {
        (
            left.provider,
            left.parent_user.as_str(),
            left.kind,
            left.access_key.as_str(),
        )
            .cmp(&(
                right.provider,
                right.parent_user.as_str(),
                right.kind,
                right.access_key.as_str(),
            ))
    });
    records.dedup_by(|left, right| {
        left.provider == right.provider
            && left.parent_user == right.parent_user
            && left.kind == right.kind
            && left.access_key == right.access_key
    });
    failures.sort_by(|left, right| {
        (
            left.provider,
            left.target.as_str(),
            left.error_type,
            left.message,
        )
            .cmp(&(
                right.provider,
                right.target.as_str(),
                right.error_type,
                right.message,
            ))
    });

    let total = records.len();
    let page = records
        .iter()
        .skip(args.offset)
        .take(args.limit)
        .cloned()
        .collect::<Vec<_>>();
    let consumed = args.offset.saturating_add(page.len());
    let truncated = consumed < total;
    let data = AccessKeyListData {
        operation: "list",
        keys: page,
        failures,
        pagination: AccessKeyPagination {
            offset: args.offset,
            limit: args.limit,
            total,
            truncated,
            next_offset: truncated.then_some(consumed),
        },
    };

    if data.failures.is_empty() {
        output_list_success(&data, formatter);
        return ExitCode::Success;
    }

    let code = aggregate_failure_code(&data);
    output_list_failure(code, &data, formatter);
    code
}

fn validate_list_args(args: &ListArgs) -> Result<(), Error> {
    if args.provider.is_empty() {
        return Err(Error::InvalidPath(
            "at least one access-key provider is required".to_string(),
        ));
    }
    if args.all && !args.user.is_empty() {
        return Err(Error::InvalidPath(
            "bulk access-key inspection accepts either --all or --user, not both".to_string(),
        ));
    }
    let query = BulkAccessKeyQuery {
        users: args.user.clone(),
        all: args.all,
        ..Default::default()
    };
    query.validate()?;
    if args.limit == 0 || args.limit > MAX_IAM_ACCESS_KEY_RESULTS {
        return Err(Error::InvalidPath(format!(
            "--limit must be between 1 and {MAX_IAM_ACCESS_KEY_RESULTS}"
        )));
    }
    if args.offset > MAX_IAM_ACCESS_KEY_RESULTS {
        return Err(Error::InvalidPath(format!(
            "--offset must not exceed {MAX_IAM_ACCESS_KEY_RESULTS}"
        )));
    }
    if args.request_batch_size == 0 || args.request_batch_size > MAX_IAM_ACCESS_KEY_SELECTORS {
        return Err(Error::InvalidPath(format!(
            "--request-batch-size must be between 1 and {MAX_IAM_ACCESS_KEY_SELECTORS}"
        )));
    }
    Ok(())
}

fn selector_batches(users: &[String], batch_size: usize) -> Vec<&[String]> {
    if users.is_empty() {
        vec![&[]]
    } else {
        users.chunks(batch_size).collect()
    }
}

#[derive(Debug, Serialize)]
struct AccessKeySuccessOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: &'a AccessKeyListData,
}

#[derive(Debug, Serialize)]
struct AccessKeyErrorOutput<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: AccessKeyAggregateError,
    data: &'a AccessKeyListData,
}

#[derive(Debug, Serialize)]
struct AccessKeyAggregateError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: &'static str,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<&'static str>,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct AccessKeyListData {
    operation: &'static str,
    keys: Vec<AccessKeyRecord>,
    failures: Vec<AccessKeyFailure>,
    pagination: AccessKeyPagination,
}

#[derive(Debug, Serialize)]
struct AccessKeyPagination {
    offset: usize,
    limit: usize,
    total: usize,
    truncated: bool,
    next_offset: Option<usize>,
}

#[derive(Debug, Serialize)]
struct AccessKeyFailure {
    provider: AccessKeyProvider,
    target: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    message: &'static str,
    retryable: bool,
}

impl AccessKeyFailure {
    fn new(provider: AccessKeyProvider, target: &str, error: &Error) -> Self {
        Self {
            provider,
            target: bounded_target(target),
            error_type: access_key_error_type(error),
            message: safe_access_key_error_message(error),
            retryable: matches!(error, Error::Network(_)),
        }
    }
}

fn bounded_target(target: &str) -> String {
    target.chars().take(1_024).collect()
}

fn access_key_error_type(error: &Error) -> &'static str {
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

fn safe_access_key_error_message(error: &Error) -> &'static str {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => "The access-key request was invalid",
        Error::Network(_) => "The access-key request could not reach RustFS",
        Error::Auth(_) => "RustFS denied the access-key request",
        Error::NotFound(_) | Error::AliasNotFound(_) => {
            "The requested access-key scope was not found"
        }
        Error::Conflict(_) | Error::AliasExists(_) => {
            "The access-key request encountered a conflict"
        }
        Error::UnsupportedFeature(_) => "The access-key route is not supported",
        Error::Interrupted(_) => "The access-key request was interrupted",
        _ => "RustFS returned an invalid access-key response",
    }
}

fn sanitized_access_key_error(error: &Error) -> Error {
    match error.exit_code() {
        2 => Error::Config("Bulk access-key capability discovery was not configured".to_string()),
        3 => Error::Network("Bulk access-key capability discovery failed".to_string()),
        4 => Error::Auth("Bulk access-key capability discovery was denied".to_string()),
        5 => Error::NotFound("Bulk access-key capability endpoint was not found".to_string()),
        6 => Error::Conflict("Bulk access-key capability discovery conflicted".to_string()),
        7 => Error::UnsupportedFeature(
            "Bulk access-key capability discovery is unavailable".to_string(),
        ),
        130 => {
            Error::Interrupted("Bulk access-key capability discovery was interrupted".to_string())
        }
        _ => Error::General("Bulk access-key capability discovery failed".to_string()),
    }
}

fn aggregate_failure_code(data: &AccessKeyListData) -> ExitCode {
    if !data.keys.is_empty() {
        return ExitCode::GeneralError;
    }
    let error_types = data
        .failures
        .iter()
        .map(|failure| failure.error_type)
        .collect::<std::collections::BTreeSet<_>>();
    if error_types.len() != 1 {
        return ExitCode::GeneralError;
    }
    match error_types.first().copied() {
        Some("usage_error") => ExitCode::UsageError,
        Some("network_error") => ExitCode::NetworkError,
        Some("auth_error") => ExitCode::AuthError,
        Some("not_found") => ExitCode::NotFound,
        Some("conflict") => ExitCode::Conflict,
        Some("unsupported_feature") => ExitCode::UnsupportedFeature,
        Some("interrupted") => ExitCode::Interrupted,
        _ => ExitCode::GeneralError,
    }
}

fn output_list_success(data: &AccessKeyListData, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&AccessKeySuccessOutput {
            schema_version: 3,
            output_type: "iam_access_keys",
            status: "success",
            data,
        });
    } else {
        print_access_key_list(data, formatter);
    }
}

fn output_list_failure(code: ExitCode, data: &AccessKeyListData, formatter: &Formatter) {
    if formatter.is_json() {
        let unsupported = code == ExitCode::UnsupportedFeature;
        formatter.json_error(&AccessKeyErrorOutput {
            schema_version: 3,
            output_type: "iam_access_keys",
            status: "error",
            error: AccessKeyAggregateError {
                error_type: if unsupported {
                    "unsupported_feature"
                } else if code == ExitCode::GeneralError {
                    "general_error"
                } else {
                    data.failures
                        .first()
                        .map(|failure| failure.error_type)
                        .unwrap_or("general_error")
                },
                message: "One or more bulk access-key scopes failed",
                retryable: code == ExitCode::NetworkError,
                capability: if unsupported {
                    data.failures
                        .first()
                        .map(|failure| failure.provider.capability())
                } else {
                    None
                },
                server: None,
                suggestion: unsupported
                    .then_some("Use only provider routes advertised by RustFS capabilities."),
            },
            data,
        });
    } else {
        print_access_key_list(data, formatter);
        formatter.error_with_code(code, "One or more bulk access-key scopes failed");
    }
}

fn emit_list_error(
    error: &Error,
    context: Option<&str>,
    capability: Option<&'static str>,
    formatter: &Formatter,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let safe_message = context.unwrap_or_else(|| safe_access_key_error_message(error));
    if formatter.is_json() {
        let unsupported = code == ExitCode::UnsupportedFeature;
        formatter.json_error(&serde_json::json!({
            "schema_version": 3,
            "type": "iam_access_keys",
            "status": "error",
            "error": {
                "type": if unsupported { "unsupported_feature" } else { access_key_error_type(error) },
                "message": safe_message,
                "retryable": code == ExitCode::NetworkError,
                "capability": capability.or(unsupported.then_some(IAM_ACCESS_KEYS_BULK_CAPABILITY)),
                "server": serde_json::Value::Null,
                "suggestion": serde_json::Value::Null
            }
        }));
    } else {
        formatter.error_with_code(code, safe_message);
    }
    code
}

fn print_access_key_list(data: &AccessKeyListData, formatter: &Formatter) {
    for key in &data.keys {
        formatter.println(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            formatter.sanitize_text(&key.access_key),
            key.kind,
            key.provider,
            formatter.sanitize_text(&key.parent_user),
            formatter.sanitize_text(key.account_status.as_deref().unwrap_or("-")),
            formatter.sanitize_text(key.expiration.as_deref().unwrap_or("-")),
        ));
    }
    for failure in &data.failures {
        formatter.error(&format!(
            "{} {}: {}",
            failure.provider,
            formatter.sanitize_text(&failure.target),
            failure.message
        ));
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.get_access_key_info(&args.access_key).await {
        Ok(info) => {
            if formatter.is_json() {
                formatter.json(&info);
            } else {
                print_access_key_info(&info, formatter);
            }
            ExitCode::Success
        }
        Err(rc_core::Error::NotFound(_)) => {
            formatter.error(&format!("Access key '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) if is_access_key_not_found_error(&e) => {
            formatter.error(&format!("Access key '{}' not found", args.access_key));
            ExitCode::NotFound
        }
        Err(e) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to get access key info: {e}"),
        ),
    }
}

fn is_access_key_not_found_error(error: &rc_core::Error) -> bool {
    error.to_string().contains("access key not exist")
}

fn print_access_key_info(info: &AccessKeyInfo, formatter: &Formatter) {
    let styled_key = formatter.style_name(&info.access_key);
    formatter.println(&format!("Access Key:    {styled_key}"));
    formatter.println(&format!("User Type:     {}", info.user_type));
    formatter.println(&format!("Provider:      {}", info.user_provider));

    print_common_info(&info.info, formatter);

    if let Some(username) = &info.ldap_specific_info.username {
        formatter.println(&format!("LDAP Username: {username}"));
    }

    print_openid_info(&info.open_id_specific_info, formatter);
}

fn print_common_info(info: &AccessKeyDetails, formatter: &Formatter) {
    if let Some(parent) = &info.parent_user {
        formatter.println(&format!("Parent User:   {parent}"));
    }

    if let Some(status) = &info.account_status {
        formatter.println(&format!("Status:        {status}"));
    }

    if let Some(expiration) = &info.expiration {
        formatter.println(&format!("Expiration:    {expiration}"));
    }

    if let Some(name) = &info.name {
        formatter.println(&format!("Name:          {name}"));
    }

    if let Some(description) = &info.description {
        formatter.println(&format!("Description:   {description}"));
    }

    if let Some(implied_policy) = info.implied_policy {
        formatter.println(&format!("Implied Policy: {implied_policy}"));
    }

    if let Some(policy) = &info.policy {
        formatter.println("");
        formatter.println("Policy:");
        formatter.println(policy);
    }
}

fn print_openid_info(info: &OpenIdAccessKeyInfo, formatter: &Formatter) {
    if let Some(config_name) = &info.config_name {
        formatter.println(&format!("OpenID Config: {config_name}"));
    }

    if let Some(user_id) = &info.user_id {
        formatter.println(&format!("OpenID User:   {user_id}"));
    }

    if let Some(user_id_claim) = &info.user_id_claim {
        formatter.println(&format!("User Claim:    {user_id_claim}"));
    }

    if let Some(display_name) = &info.display_name {
        formatter.println(&format!("Display Name:  {display_name}"));
    }

    if let Some(display_name_claim) = &info.display_name_claim {
        formatter.println(&format!("Display Claim: {display_name_claim}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use jsonschema::Validator;
    use rc_core::Result;
    use rc_core::admin::{
        AccessKeyDetails, AccessKeyKind, CapabilityEntry, CapabilityReport,
        ClusterSnapshotMetadata, LdapAccessKeyInfo, OpenIdAccessKeyInfo,
    };
    use serde_json::Value;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_access_key_info_serializes_server_shape() {
        let info = AccessKeyInfo {
            access_key: "svc-ldap".to_string(),
            user_type: "Service Account".to_string(),
            user_provider: "ldap".to_string(),
            info: AccessKeyDetails {
                parent_user: Some("ldap-parent".to_string()),
                account_status: Some("on".to_string()),
                implied_policy: Some(false),
                policy: Some("{\"Version\":\"2012-10-17\"}".to_string()),
                expiration: None,
                name: Some("LDAP Service".to_string()),
                description: None,
            },
            ldap_specific_info: LdapAccessKeyInfo {
                username: Some("alice".to_string()),
            },
            open_id_specific_info: OpenIdAccessKeyInfo::default(),
        };

        let value = serde_json::to_value(info).expect("serialize access key info");

        assert_eq!(value["accessKey"], "svc-ldap");
        assert_eq!(value["userType"], "Service Account");
        assert_eq!(value["userProvider"], "ldap");
        assert_eq!(value["parentUser"], "ldap-parent");
        assert_eq!(value["accountStatus"], "on");
        assert_eq!(value["ldapSpecificInfo"]["username"], "alice");
        assert!(value.get("openIDSpecificInfo").is_none());
        assert!(value.get("openIdSpecificInfo").is_none());
    }

    #[test]
    fn test_access_key_not_exist_error_maps_to_not_found() {
        let error = rc_core::Error::General("Bad request: access key not exist".to_string());

        assert!(is_access_key_not_found_error(&error));
    }

    struct StubBulkApi {
        capabilities: Vec<CapabilityEntry>,
        builtin: Result<Vec<AccessKeyRecord>>,
        ldap: Result<Vec<AccessKeyRecord>>,
        openid: Result<Vec<AccessKeyRecord>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CapabilityApi for StubBulkApi {
        async fn discover_capabilities(&self, _refresh: bool) -> Result<CapabilityReport> {
            Ok(CapabilityReport {
                server_version: Some("1.0.0-beta.10".to_string()),
                runtime_path: "/runtime".to_string(),
                extensions_path: "/extensions".to_string(),
                cluster_snapshot_path: "/snapshot".to_string(),
                capabilities: self.capabilities.clone(),
                extensions: Vec::new(),
                cluster: ClusterSnapshotMetadata {
                    summary: None,
                    runtime_capabilities_path: None,
                    extensions_catalog_path: None,
                },
            })
        }
    }

    #[async_trait]
    impl BulkAccessKeyApi for StubBulkApi {
        async fn list_access_keys_bulk(
            &self,
            query: &BulkAccessKeyQuery,
        ) -> Result<Vec<AccessKeyRecord>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match query.provider {
                AccessKeyProvider::Builtin => clone_result(&self.builtin),
                AccessKeyProvider::Ldap => clone_result(&self.ldap),
                AccessKeyProvider::Openid => clone_result(&self.openid),
            }
        }
    }

    fn clone_result(result: &Result<Vec<AccessKeyRecord>>) -> Result<Vec<AccessKeyRecord>> {
        match result {
            Ok(records) => Ok(records.clone()),
            Err(error) => Err(match error.exit_code() {
                3 => Error::Network("stub".to_string()),
                4 => Error::Auth("stub".to_string()),
                7 => Error::UnsupportedFeature("stub".to_string()),
                _ => Error::General("stub".to_string()),
            }),
        }
    }

    fn capability(provider: AccessKeyProvider) -> CapabilityEntry {
        CapabilityEntry {
            name: provider.capability().to_string(),
            availability: CapabilityAvailability::Available,
            reason: None,
        }
    }

    fn record(
        provider: AccessKeyProvider,
        parent: &str,
        key: &str,
        kind: AccessKeyKind,
    ) -> AccessKeyRecord {
        AccessKeyRecord {
            access_key: key.to_string(),
            kind,
            provider,
            parent_user: parent.to_string(),
            account_status: Some("on".to_string()),
            expiration: None,
            name: None,
            description: None,
            implied_policy: None,
        }
    }

    fn list_args(providers: Vec<ProviderArg>) -> ListArgs {
        ListArgs {
            alias: "local".to_string(),
            provider: providers,
            user: vec!["alice".to_string()],
            all: false,
            key_type: KeyTypeArg::All,
            offset: 0,
            limit: DEFAULT_PAGE_LIMIT,
            request_batch_size: DEFAULT_REQUEST_BATCH_SIZE,
        }
    }

    fn quiet_formatter() -> Formatter {
        Formatter::new(crate::output::OutputConfig {
            quiet: true,
            ..Default::default()
        })
    }

    fn output_v3_validator() -> Validator {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("schemas/output_v3.json");
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("output schema should be readable"),
        )
        .expect("output schema should parse");
        jsonschema::validator_for(&schema).expect("output schema should compile")
    }

    #[test]
    fn access_key_success_matches_v3_schema_and_golden_fixture() {
        let data = AccessKeyListData {
            operation: "list",
            keys: vec![record(
                AccessKeyProvider::Builtin,
                "alice",
                "svc-a",
                AccessKeyKind::ServiceAccount,
            )],
            failures: Vec::new(),
            pagination: AccessKeyPagination {
                offset: 0,
                limit: 100,
                total: 1,
                truncated: false,
                next_offset: None,
            },
        };
        let value = serde_json::to_value(AccessKeySuccessOutput {
            schema_version: 3,
            output_type: "iam_access_keys",
            status: "success",
            data: &data,
        })
        .expect("serialize bulk access-key output");
        let errors = output_v3_validator()
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "invalid output: {}", errors.join("\n"));

        let golden: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/output_v3/iam_access_keys/success.json"
        ))
        .expect("golden fixture should parse");
        assert_eq!(value, golden);
    }

    #[test]
    fn access_key_empty_and_partial_outputs_match_v3_schema() {
        let empty = AccessKeyListData {
            operation: "list",
            keys: Vec::new(),
            failures: Vec::new(),
            pagination: AccessKeyPagination {
                offset: 0,
                limit: 100,
                total: 0,
                truncated: false,
                next_offset: None,
            },
        };
        let empty_value = serde_json::to_value(AccessKeySuccessOutput {
            schema_version: 3,
            output_type: "iam_access_keys",
            status: "success",
            data: &empty,
        })
        .expect("serialize empty output");
        assert!(output_v3_validator().is_valid(&empty_value));

        let partial = AccessKeyListData {
            operation: "list",
            keys: vec![record(
                AccessKeyProvider::Builtin,
                "alice",
                "svc-a",
                AccessKeyKind::ServiceAccount,
            )],
            failures: vec![AccessKeyFailure::new(
                AccessKeyProvider::Ldap,
                "alice",
                &Error::Auth("secret canary".to_string()),
            )],
            pagination: AccessKeyPagination {
                offset: 0,
                limit: 100,
                total: 1,
                truncated: false,
                next_offset: None,
            },
        };
        let partial_value = serde_json::to_value(AccessKeyErrorOutput {
            schema_version: 3,
            output_type: "iam_access_keys",
            status: "error",
            error: AccessKeyAggregateError {
                error_type: "general_error",
                message: "One or more bulk access-key scopes failed",
                retryable: false,
                capability: None,
                server: None,
                suggestion: None,
            },
            data: &partial,
        })
        .expect("serialize partial output");
        assert!(output_v3_validator().is_valid(&partial_value));
        assert!(!partial_value.to_string().contains("secret canary"));
    }

    #[tokio::test]
    async fn access_key_partial_results_return_general_error_deterministically() {
        let api = StubBulkApi {
            capabilities: vec![
                capability(AccessKeyProvider::Builtin),
                capability(AccessKeyProvider::Ldap),
            ],
            builtin: Ok(vec![
                record(
                    AccessKeyProvider::Builtin,
                    "bob",
                    "sts-z",
                    AccessKeyKind::Sts,
                ),
                record(
                    AccessKeyProvider::Builtin,
                    "alice",
                    "svc-a",
                    AccessKeyKind::ServiceAccount,
                ),
            ]),
            ldap: Err(Error::Auth("secret server body".to_string())),
            openid: Ok(Vec::new()),
            calls: AtomicUsize::new(0),
        };

        let code = execute_list_with_apis(
            list_args(vec![ProviderArg::Ldap, ProviderArg::Builtin]),
            &api,
            &api,
            &quiet_formatter(),
        )
        .await;

        assert_eq!(code, ExitCode::GeneralError);
        assert_eq!(api.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn access_key_missing_capability_fails_closed_before_route_call() {
        let api = StubBulkApi {
            capabilities: Vec::new(),
            builtin: Ok(Vec::new()),
            ldap: Ok(Vec::new()),
            openid: Ok(Vec::new()),
            calls: AtomicUsize::new(0),
        };

        let code = execute_list_with_apis(
            list_args(vec![ProviderArg::Openid]),
            &api,
            &api,
            &quiet_formatter(),
        )
        .await;

        assert_eq!(code, ExitCode::UnsupportedFeature);
        assert_eq!(api.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn access_key_uniform_permission_failure_preserves_auth_exit_code() {
        let api = StubBulkApi {
            capabilities: vec![capability(AccessKeyProvider::Builtin)],
            builtin: Err(Error::Auth("secret server body".to_string())),
            ldap: Ok(Vec::new()),
            openid: Ok(Vec::new()),
            calls: AtomicUsize::new(0),
        };

        let code = execute_list_with_apis(
            list_args(vec![ProviderArg::Builtin]),
            &api,
            &api,
            &quiet_formatter(),
        )
        .await;

        assert_eq!(code, ExitCode::AuthError);
        assert_eq!(api.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn access_key_invalid_limit_is_rejected_before_discovery() {
        let api = StubBulkApi {
            capabilities: vec![capability(AccessKeyProvider::Builtin)],
            builtin: Ok(Vec::new()),
            ldap: Ok(Vec::new()),
            openid: Ok(Vec::new()),
            calls: AtomicUsize::new(0),
        };
        let mut args = list_args(vec![ProviderArg::Builtin]);
        args.limit = MAX_IAM_ACCESS_KEY_RESULTS + 1;

        let code = execute_list_with_apis(args, &api, &api, &quiet_formatter()).await;

        assert_eq!(code, ExitCode::UsageError);
        assert_eq!(api.calls.load(Ordering::Relaxed), 0);
    }
}
