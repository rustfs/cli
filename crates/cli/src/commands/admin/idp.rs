//! RustFS identity-provider administration.

use clap::{Args, Subcommand};
use rc_core::admin::{
    OidcMutationApi, OidcMutationRequest, OidcProvider, OidcProviderList, OidcProviderSource,
    OidcReadApi, OidcValidationRequest, OidcValidationResult,
};
use rc_core::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use super::{emit_observability_error, get_admin_client};
use crate::exit_code::ExitCode;
use crate::output::Formatter;

#[derive(Subcommand, Debug)]
pub enum IdpCommands {
    /// Inspect and validate OpenID Connect providers
    #[command(subcommand)]
    Openid(OpenidCommands),
}

#[derive(Subcommand, Debug)]
pub enum OpenidCommands {
    /// List effective OIDC provider configurations
    List(OidcListArgs),

    /// Display one effective OIDC provider configuration
    Get(OidcGetArgs),

    /// Validate OIDC discovery without saving configuration
    Validate(Box<OidcValidateArgs>),

    /// Create or replace an OIDC provider while preserving omitted fields
    Set(Box<OidcSetArgs>),

    /// Update an existing OIDC provider while preserving omitted fields
    Update(Box<OidcSetArgs>),

    /// Enable an existing OIDC provider
    Enable(OidcToggleArgs),

    /// Disable an existing OIDC provider
    Disable(OidcToggleArgs),
}

#[derive(Args, Debug)]
pub struct OidcListArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct OidcGetArgs {
    /// Alias name of the server
    pub alias: String,

    /// Exact OIDC provider ID
    pub provider_id: String,
}

#[derive(Args, Debug)]
pub struct OidcValidateArgs {
    /// Alias name of the server
    pub alias: String,

    /// OIDC provider ID used only for validation
    pub provider_id: String,

    /// Provider issuer or discovery base URL
    #[arg(long)]
    pub config_url: String,

    /// OIDC client identifier
    #[arg(long)]
    pub client_id: String,

    /// Expected issuer URL, when it must differ from the configuration URL
    #[arg(long)]
    pub issuer: Option<String>,

    /// Requested scope; may be repeated and must include openid
    #[arg(long = "scope", default_values_t = [
        "openid".to_string(),
        "profile".to_string(),
        "email".to_string()
    ])]
    pub scopes: Vec<String>,

    /// Additional trusted token audience; may be repeated
    #[arg(long = "other-audience")]
    pub other_audiences: Vec<String>,

    /// Explicit callback URI
    #[arg(long)]
    pub redirect_uri: Option<String>,

    /// Require the explicit callback URI instead of a server-derived URI
    #[arg(long, requires = "redirect_uri")]
    pub static_redirect: bool,

    /// Policy claim name
    #[arg(long, default_value = "policy")]
    pub claim_name: String,

    /// Prefix applied to policy claims
    #[arg(long, default_value = "")]
    pub claim_prefix: String,

    /// Fixed fallback policy
    #[arg(long, default_value = "")]
    pub role_policy: String,

    /// Group claim name
    #[arg(long, default_value = "groups")]
    pub groups_claim: String,

    /// Roles claim name
    #[arg(long, default_value = "roles")]
    pub roles_claim: String,

    /// Email claim name
    #[arg(long, default_value = "email")]
    pub email_claim: String,

    /// Username claim name
    #[arg(long, default_value = "preferred_username")]
    pub username_claim: String,
}

#[derive(Args, Debug)]
pub struct OidcSetArgs {
    /// Alias name of the server
    pub alias: String,

    /// Exact OIDC provider ID
    pub provider_id: String,

    /// Provider issuer or discovery base URL
    #[arg(long)]
    pub config_url: Option<String>,

    /// OIDC client identifier
    #[arg(long)]
    pub client_id: Option<String>,

    /// Human-readable provider name
    #[arg(long)]
    pub display_name: Option<String>,

    /// Expected issuer URL
    #[arg(long, conflicts_with = "clear_issuer")]
    pub issuer: Option<String>,

    /// Remove an existing explicit issuer
    #[arg(long)]
    pub clear_issuer: bool,

    /// Requested scope; may be repeated and must include openid
    #[arg(long = "scope")]
    pub scopes: Vec<String>,

    /// Additional trusted token audience; may be repeated
    #[arg(long = "other-audience")]
    pub other_audiences: Vec<String>,

    /// Replace all additional trusted token audiences, including with an empty list
    #[arg(long)]
    pub replace_other_audiences: bool,

    /// Explicit callback URI
    #[arg(long, conflicts_with = "clear_redirect_uri")]
    pub redirect_uri: Option<String>,

    /// Remove an existing explicit callback URI
    #[arg(long)]
    pub clear_redirect_uri: bool,

    /// Require the explicit callback URI instead of a server-derived URI
    #[arg(long, conflicts_with = "dynamic_redirect")]
    pub static_redirect: bool,

    /// Use a server-derived callback URI
    #[arg(long)]
    pub dynamic_redirect: bool,

    /// Policy claim name
    #[arg(long)]
    pub claim_name: Option<String>,

    /// Prefix applied to policy claims
    #[arg(long)]
    pub claim_prefix: Option<String>,

    /// Fixed fallback policy
    #[arg(long)]
    pub role_policy: Option<String>,

    /// Group claim name
    #[arg(long)]
    pub groups_claim: Option<String>,

    /// Roles claim name
    #[arg(long)]
    pub roles_claim: Option<String>,

    /// Email claim name
    #[arg(long)]
    pub email_claim: Option<String>,

    /// Username claim name
    #[arg(long)]
    pub username_claim: Option<String>,

    /// Hide this provider from the login UI
    #[arg(long, conflicts_with = "show_in_ui")]
    pub hide_from_ui: bool,

    /// Show this provider in the login UI
    #[arg(long)]
    pub show_in_ui: bool,

    /// Read a replacement client secret from standard input
    #[arg(long, conflicts_with = "client_secret_file")]
    pub client_secret_stdin: bool,

    /// Read a replacement client secret from a protected regular file
    #[arg(long, value_name = "PATH")]
    pub client_secret_file: Option<PathBuf>,

    /// Acknowledge that the currently persisted client secret will be replaced
    #[arg(long)]
    pub replace_client_secret: bool,

    /// Validate and display the redacted change plan without saving
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct OidcToggleArgs {
    /// Alias name of the server
    pub alias: String,

    /// Exact OIDC provider ID
    pub provider_id: String,

    /// Validate and display the change without saving
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
struct OidcSuccessOutput<T> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: T,
}

#[derive(Debug, Serialize)]
struct OidcListData<'a> {
    operation: &'static str,
    restart_required: bool,
    providers: &'a [OidcProvider],
}

#[derive(Debug, Serialize)]
struct OidcGetData<'a> {
    operation: &'static str,
    provider: &'a OidcProvider,
}

#[derive(Debug, Serialize)]
struct OidcValidateData<'a> {
    operation: &'static str,
    provider_id: &'a str,
    #[serde(flatten)]
    result: &'a OidcValidationResult,
}

#[derive(Debug, Serialize)]
struct OidcChange {
    field: &'static str,
    before: Value,
    after: Value,
}

#[derive(Debug, Serialize)]
struct OidcMutationData<'a> {
    operation: &'static str,
    provider_id: &'a str,
    created: bool,
    dry_run: bool,
    restart_required: bool,
    changes: &'a [OidcChange],
}

#[derive(Debug, Clone, Copy)]
enum MutationMode {
    Set,
    Update,
    Enable,
    Disable,
}

impl MutationMode {
    const fn operation(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Update => "update",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }
}

#[derive(Debug)]
enum ClientSecretSource {
    Stdin,
    File(PathBuf),
}

pub async fn execute(command: IdpCommands, formatter: &Formatter) -> ExitCode {
    match command {
        IdpCommands::Openid(command) => match command {
            OpenidCommands::List(args) => execute_list(args, formatter).await,
            OpenidCommands::Get(args) => execute_get(args, formatter).await,
            OpenidCommands::Validate(args) => execute_validate(*args, formatter).await,
            OpenidCommands::Set(args) => {
                execute_mutation(*args, MutationMode::Set, formatter).await
            }
            OpenidCommands::Update(args) => {
                execute_mutation(*args, MutationMode::Update, formatter).await
            }
            OpenidCommands::Enable(args) => {
                execute_toggle(args, MutationMode::Enable, formatter).await
            }
            OpenidCommands::Disable(args) => {
                execute_toggle(args, MutationMode::Disable, formatter).await
            }
        },
    }
}

async fn execute_list(args: OidcListArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.oidc_list_providers().await {
        Ok(list) => {
            print_list(&list, formatter);
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "oidc",
            "admin.oidc-config-read",
            "Failed to list OIDC providers",
            &error,
            formatter,
        ),
    }
}

async fn execute_get(args: OidcGetArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.oidc_get_provider(&args.provider_id).await {
        Ok(provider) => {
            print_provider(&provider, formatter);
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "oidc",
            "admin.oidc-config-read",
            "Failed to get OIDC provider",
            &error,
            formatter,
        ),
    }
}

async fn execute_validate(args: OidcValidateArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    let mut request =
        OidcValidationRequest::new(args.provider_id.clone(), args.config_url, args.client_id);
    request.issuer = args.issuer;
    request.scopes = args.scopes;
    request.other_audiences = args.other_audiences;
    request.redirect_uri = args.redirect_uri;
    request.redirect_uri_dynamic = !args.static_redirect;
    request.claim_name = args.claim_name;
    request.claim_prefix = args.claim_prefix;
    request.role_policy = args.role_policy;
    request.groups_claim = args.groups_claim;
    request.roles_claim = args.roles_claim;
    request.email_claim = args.email_claim;
    request.username_claim = args.username_claim;

    match client.oidc_validate(request).await {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&OidcSuccessOutput {
                    schema_version: 3,
                    output_type: "oidc",
                    status: "success",
                    data: OidcValidateData {
                        operation: "validate",
                        provider_id: &args.provider_id,
                        result: &result,
                    },
                });
            } else {
                formatter.println(&formatter.style_name("OIDC Validation"));
                formatter.println("");
                formatter.println(&format!(
                    "Provider:               {}",
                    safe(&args.provider_id, formatter)
                ));
                formatter.println(&format!("Valid:                  {}", result.valid));
                formatter.println(&format!(
                    "Issuer:                 {}",
                    optional(result.issuer.as_deref(), formatter)
                ));
                formatter.println(&format!(
                    "Authorization endpoint: {}",
                    optional(result.authorization_endpoint.as_deref(), formatter)
                ));
                formatter.println(&format!(
                    "Token endpoint:         {}",
                    optional(result.token_endpoint.as_deref(), formatter)
                ));
            }
            ExitCode::Success
        }
        Err(error) => emit_observability_error(
            "oidc",
            "admin.oidc-config-validate",
            "Failed to validate OIDC provider",
            &error,
            formatter,
        ),
    }
}

async fn execute_mutation(
    args: OidcSetArgs,
    mode: MutationMode,
    formatter: &Formatter,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match prepare_and_apply_mutation(&client, args, mode, formatter).await {
        Ok(()) => ExitCode::Success,
        Err(error) => emit_observability_error(
            "oidc",
            "admin.oidc-config-write",
            "Failed to update OIDC provider",
            &error,
            formatter,
        ),
    }
}

async fn execute_toggle(
    args: OidcToggleArgs,
    mode: MutationMode,
    formatter: &Formatter,
) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };
    match prepare_and_apply_toggle(&client, args, mode, formatter).await {
        Ok(()) => ExitCode::Success,
        Err(error) => emit_observability_error(
            "oidc",
            "admin.oidc-config-write",
            "Failed to update OIDC provider state",
            &error,
            formatter,
        ),
    }
}

async fn prepare_and_apply_mutation(
    client: &rc_s3::AdminClient,
    args: OidcSetArgs,
    mode: MutationMode,
    formatter: &Formatter,
) -> Result<()> {
    let secret_source = resolve_secret_source(&args)?;
    let list = client.oidc_list_providers().await?;
    let current = list
        .providers
        .iter()
        .find(|provider| provider.provider_id == args.provider_id);
    if let Some(provider) = current {
        ensure_editable(provider)?;
    } else if matches!(mode, MutationMode::Update) {
        return Err(Error::NotFound(format!(
            "OIDC provider '{}' was not found",
            args.provider_id
        )));
    }

    let mut request = match current {
        Some(provider) => OidcMutationRequest::from_provider(provider),
        None => OidcMutationRequest::new(
            args.provider_id.clone(),
            args.config_url.clone().ok_or_else(|| {
                Error::InvalidPath("--config-url is required when creating a provider".to_string())
            })?,
            args.client_id.clone().ok_or_else(|| {
                Error::InvalidPath("--client-id is required when creating a provider".to_string())
            })?,
        ),
    };
    apply_patch(&mut request, &args);
    request.validate()?;

    let changes = mutation_diff(
        current,
        &request,
        secret_source.is_some(),
        args.replace_client_secret,
    );
    client
        .oidc_validate(OidcValidationRequest::from_mutation(&request))
        .await?;
    if args.dry_run || changes.is_empty() {
        print_mutation(
            mode,
            &request.provider_id,
            current.is_none(),
            args.dry_run,
            !changes.is_empty(),
            &changes,
            formatter,
        );
        return Ok(());
    }

    if let Some(source) = secret_source {
        request.client_secret = Some(read_client_secret(source)?.to_string());
    }
    let result = client.oidc_upsert_provider(request).await?;
    print_mutation(
        mode,
        &args.provider_id,
        current.is_none(),
        false,
        result.restart_required,
        &changes,
        formatter,
    );
    Ok(())
}

async fn prepare_and_apply_toggle(
    client: &rc_s3::AdminClient,
    args: OidcToggleArgs,
    mode: MutationMode,
    formatter: &Formatter,
) -> Result<()> {
    let provider = client.oidc_get_provider(&args.provider_id).await?;
    ensure_editable(&provider)?;
    let mut request = OidcMutationRequest::from_provider(&provider);
    request.enabled = matches!(mode, MutationMode::Enable);
    request.validate()?;
    let changes = mutation_diff(Some(&provider), &request, false, false);
    client
        .oidc_validate(OidcValidationRequest::from_mutation(&request))
        .await?;
    if args.dry_run || changes.is_empty() {
        print_mutation(
            mode,
            &args.provider_id,
            false,
            args.dry_run,
            !changes.is_empty(),
            &changes,
            formatter,
        );
        return Ok(());
    }
    let result = client.oidc_upsert_provider(request).await?;
    print_mutation(
        mode,
        &args.provider_id,
        false,
        false,
        result.restart_required,
        &changes,
        formatter,
    );
    Ok(())
}

fn ensure_editable(provider: &OidcProvider) -> Result<()> {
    if provider.source == OidcProviderSource::Env || !provider.editable {
        return Err(Error::Conflict(format!(
            "OIDC provider '{}' is managed by the environment and cannot be edited",
            provider.provider_id
        )));
    }
    Ok(())
}

fn apply_patch(request: &mut OidcMutationRequest, args: &OidcSetArgs) {
    if let Some(value) = &args.config_url {
        request.config_url.clone_from(value);
    }
    if let Some(value) = &args.client_id {
        request.client_id.clone_from(value);
    }
    if let Some(value) = &args.display_name {
        request.display_name.clone_from(value);
    }
    if args.clear_issuer {
        request.issuer = None;
    } else if let Some(value) = &args.issuer {
        request.issuer = Some(value.clone());
    }
    if !args.scopes.is_empty() {
        request.scopes.clone_from(&args.scopes);
    }
    if args.replace_other_audiences || !args.other_audiences.is_empty() {
        request.other_audiences.clone_from(&args.other_audiences);
    }
    if args.clear_redirect_uri {
        request.redirect_uri = None;
    } else if let Some(value) = &args.redirect_uri {
        request.redirect_uri = Some(value.clone());
    }
    if args.static_redirect {
        request.redirect_uri_dynamic = false;
    } else if args.dynamic_redirect {
        request.redirect_uri_dynamic = true;
    }
    for (target, value) in [
        (&mut request.claim_name, &args.claim_name),
        (&mut request.claim_prefix, &args.claim_prefix),
        (&mut request.role_policy, &args.role_policy),
        (&mut request.groups_claim, &args.groups_claim),
        (&mut request.roles_claim, &args.roles_claim),
        (&mut request.email_claim, &args.email_claim),
        (&mut request.username_claim, &args.username_claim),
    ] {
        if let Some(value) = value {
            target.clone_from(value);
        }
    }
    if args.hide_from_ui {
        request.hide_from_ui = true;
    } else if args.show_in_ui {
        request.hide_from_ui = false;
    }
}

fn resolve_secret_source(args: &OidcSetArgs) -> Result<Option<ClientSecretSource>> {
    let source = if args.client_secret_stdin {
        Some(ClientSecretSource::Stdin)
    } else {
        args.client_secret_file
            .clone()
            .map(ClientSecretSource::File)
    };
    match (source.is_some(), args.replace_client_secret) {
        (true, false) => Err(Error::InvalidPath(
            "--replace-client-secret is required with a client secret input".to_string(),
        )),
        (false, true) => Err(Error::InvalidPath(
            "--replace-client-secret requires --client-secret-stdin or --client-secret-file"
                .to_string(),
        )),
        _ => Ok(source),
    }
}

const MAX_OIDC_SECRET_BYTES: u64 = 64 * 1024;

fn read_client_secret(source: ClientSecretSource) -> Result<Zeroizing<String>> {
    match source {
        ClientSecretSource::Stdin => {
            let stdin = std::io::stdin();
            read_secret_text(stdin.lock())
        }
        ClientSecretSource::File(path) => read_protected_secret_file(&path),
    }
}

fn read_protected_secret_file(path: &Path) -> Result<Zeroizing<String>> {
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Error::InvalidPath("Failed to inspect OIDC secret file".to_string()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(Error::InvalidPath(
            "OIDC secret input must be a regular file, not a symlink".to_string(),
        ));
    }
    let file = File::open(path)
        .map_err(|_| Error::InvalidPath("Failed to open OIDC secret file".to_string()))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| Error::InvalidPath("Failed to inspect opened OIDC secret file".to_string()))?;
    if !file_metadata.is_file() {
        return Err(Error::InvalidPath(
            "OIDC secret input must remain a regular file while opening".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(Error::InvalidPath(
                "OIDC secret file changed while being opened".to_string(),
            ));
        }
        if file_metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidPath(
                "OIDC secret file cannot grant group or other permissions".to_string(),
            ));
        }
    }
    read_secret_text(file)
}

fn read_secret_text(reader: impl Read) -> Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(MAX_OIDC_SECRET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidPath("Failed to read OIDC client secret".to_string()))?;
    if bytes.len() as u64 > MAX_OIDC_SECRET_BYTES {
        return Err(Error::InvalidPath(format!(
            "OIDC client secret exceeds the {MAX_OIDC_SECRET_BYTES} byte limit"
        )));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| Error::InvalidPath("OIDC client secret must be UTF-8".to_string()))?;
    if value.is_empty() || value.contains(['\n', '\r', '\0']) {
        return Err(Error::InvalidPath(
            "OIDC client secret must contain one non-empty text line".to_string(),
        ));
    }
    Ok(Zeroizing::new(value.to_string()))
}

fn mutation_diff(
    current: Option<&OidcProvider>,
    requested: &OidcMutationRequest,
    replaces_secret: bool,
    replacement_acknowledged: bool,
) -> Vec<OidcChange> {
    let mut changes = Vec::new();
    macro_rules! changed {
        ($field:literal, $before:expr, $after:expr) => {{
            let before = serde_json::to_value($before).unwrap_or(Value::Null);
            let after = serde_json::to_value($after).unwrap_or(Value::Null);
            if before != after {
                changes.push(OidcChange {
                    field: $field,
                    before,
                    after,
                });
            }
        }};
    }
    changed!(
        "enabled",
        current.map(|value| value.enabled),
        requested.enabled
    );
    changed!(
        "display_name",
        current.map(|value| value.display_name.as_str()),
        requested.display_name.as_str()
    );
    changed!(
        "config_url",
        current.map(|value| value.config_url.as_str()),
        requested.config_url.as_str()
    );
    changed!(
        "issuer",
        current.and_then(|value| value.issuer.as_deref()),
        requested.issuer.as_deref()
    );
    changed!(
        "client_id",
        current.map(|value| value.client_id.as_str()),
        requested.client_id.as_str()
    );
    let previous_secret = if current.is_some_and(|value| value.client_secret_configured) {
        "[configured]"
    } else {
        "[not configured]"
    };
    if replaces_secret && replacement_acknowledged {
        changes.push(OidcChange {
            field: "client_secret",
            before: Value::String(previous_secret.to_string()),
            after: Value::String("[replaced]".to_string()),
        });
    }
    changed!(
        "scopes",
        current.map(|value| value.scopes.as_slice()),
        requested.scopes.as_slice()
    );
    changed!(
        "other_audiences",
        current.map(|value| value.other_audiences.as_slice()),
        requested.other_audiences.as_slice()
    );
    changed!(
        "redirect_uri",
        current.and_then(|value| value.redirect_uri.as_deref()),
        requested.redirect_uri.as_deref()
    );
    changed!(
        "redirect_uri_dynamic",
        current.map(|value| value.redirect_uri_dynamic),
        requested.redirect_uri_dynamic
    );
    changed!(
        "claim_name",
        current.map(|value| value.claim_name.as_str()),
        requested.claim_name.as_str()
    );
    changed!(
        "claim_prefix",
        current.map(|value| value.claim_prefix.as_str()),
        requested.claim_prefix.as_str()
    );
    changed!(
        "role_policy",
        current.map(|value| value.role_policy.as_str()),
        requested.role_policy.as_str()
    );
    changed!(
        "groups_claim",
        current.map(|value| value.groups_claim.as_str()),
        requested.groups_claim.as_str()
    );
    changed!(
        "roles_claim",
        current.map(|value| value.roles_claim.as_str()),
        requested.roles_claim.as_str()
    );
    changed!(
        "email_claim",
        current.map(|value| value.email_claim.as_str()),
        requested.email_claim.as_str()
    );
    changed!(
        "username_claim",
        current.map(|value| value.username_claim.as_str()),
        requested.username_claim.as_str()
    );
    changed!(
        "hide_from_ui",
        current.map(|value| value.hide_from_ui),
        requested.hide_from_ui
    );
    changes
}

fn print_mutation(
    mode: MutationMode,
    provider_id: &str,
    created: bool,
    dry_run: bool,
    restart_required: bool,
    changes: &[OidcChange],
    formatter: &Formatter,
) {
    if formatter.is_json() {
        formatter.json(&OidcSuccessOutput {
            schema_version: 3,
            output_type: "oidc",
            status: "success",
            data: OidcMutationData {
                operation: mode.operation(),
                provider_id,
                created,
                dry_run,
                restart_required,
                changes,
            },
        });
        return;
    }
    formatter.println(&formatter.style_name("OIDC Provider Change"));
    formatter.println("");
    formatter.println(&format!("Operation:        {}", mode.operation()));
    formatter.println(&format!(
        "Provider:         {}",
        safe(provider_id, formatter)
    ));
    formatter.println(&format!("Created:          {created}"));
    formatter.println(&format!("Dry run:          {dry_run}"));
    formatter.println(&format!("Restart required: {restart_required}"));
    formatter.println("Changes:");
    if changes.is_empty() {
        formatter.println("  (none)");
    } else {
        for change in changes {
            formatter.println(&format!(
                "  {}: {} -> {}",
                change.field,
                safe(&change.before.to_string(), formatter),
                safe(&change.after.to_string(), formatter)
            ));
        }
    }
}

fn print_list(list: &OidcProviderList, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&OidcSuccessOutput {
            schema_version: 3,
            output_type: "oidc",
            status: "success",
            data: OidcListData {
                operation: "list",
                restart_required: list.restart_required,
                providers: &list.providers,
            },
        });
        return;
    }
    formatter.println(&formatter.style_name("OIDC Providers"));
    formatter.println("");
    formatter.println(&format!("Restart required: {}", list.restart_required));
    if list.providers.is_empty() {
        formatter.println("No OIDC providers configured.");
        return;
    }
    for provider in &list.providers {
        formatter.println(&format!(
            "{}\t{}\t{}\t{}",
            safe(&provider.provider_id, formatter),
            safe(&provider.display_name, formatter),
            if provider.enabled {
                "enabled"
            } else {
                "disabled"
            },
            match provider.source {
                rc_core::admin::OidcProviderSource::Env => "env",
                rc_core::admin::OidcProviderSource::Persisted => "persisted",
            }
        ));
    }
}

fn print_provider(provider: &OidcProvider, formatter: &Formatter) {
    if formatter.is_json() {
        formatter.json(&OidcSuccessOutput {
            schema_version: 3,
            output_type: "oidc",
            status: "success",
            data: OidcGetData {
                operation: "get",
                provider,
            },
        });
        return;
    }
    formatter.println(&formatter.style_name("OIDC Provider"));
    formatter.println("");
    formatter.println(&format!(
        "Provider ID:              {}",
        safe(&provider.provider_id, formatter)
    ));
    formatter.println(&format!(
        "Display name:             {}",
        safe(&provider.display_name, formatter)
    ));
    formatter.println(&format!("Enabled:                  {}", provider.enabled));
    formatter.println(&format!("Editable:                 {}", provider.editable));
    formatter.println(&format!(
        "Configuration URL:        {}",
        safe(&provider.config_url, formatter)
    ));
    formatter.println(&format!(
        "Issuer:                   {}",
        optional(provider.issuer.as_deref(), formatter)
    ));
    formatter.println(&format!(
        "Client ID:                {}",
        safe(&provider.client_id, formatter)
    ));
    formatter.println(&format!(
        "Client secret configured: {}",
        provider.client_secret_configured
    ));
    formatter.println(&format!(
        "Scopes:                   {}",
        safe(&provider.scopes.join(","), formatter)
    ));
    formatter.println(&format!(
        "Redirect URI:             {}",
        optional(provider.redirect_uri.as_deref(), formatter)
    ));
}

fn optional(value: Option<&str>, formatter: &Formatter) -> String {
    value.map_or_else(|| "-".to_string(), |value| safe(value, formatter))
}

fn safe(value: &str, formatter: &Formatter) -> String {
    formatter.sanitize_text(value)
}
