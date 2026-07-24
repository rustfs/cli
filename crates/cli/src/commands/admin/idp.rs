//! Read-only RustFS identity-provider administration.

use clap::{Args, Subcommand};
use rc_core::admin::{
    OidcProvider, OidcProviderList, OidcReadApi, OidcValidationRequest, OidcValidationResult,
};
use serde::Serialize;

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

pub async fn execute(command: IdpCommands, formatter: &Formatter) -> ExitCode {
    match command {
        IdpCommands::Openid(command) => match command {
            OpenidCommands::List(args) => execute_list(args, formatter).await,
            OpenidCommands::Get(args) => execute_get(args, formatter).await,
            OpenidCommands::Validate(args) => execute_validate(*args, formatter).await,
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
