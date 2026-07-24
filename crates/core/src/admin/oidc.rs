//! Typed, secret-free contracts for RustFS OIDC inspection and validation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Maximum encoded size accepted for one OIDC administration response.
pub const MAX_OIDC_RESPONSE_BYTES: usize = 1024 * 1024;

/// Source that owns an effective OIDC provider configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OidcProviderSource {
    Env,
    Persisted,
}

/// Secret-free view of one effective RustFS OIDC provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcProvider {
    pub provider_id: String,
    pub source: OidcProviderSource,
    pub editable: bool,
    pub enabled: bool,
    pub display_name: String,
    pub config_url: String,
    pub issuer: Option<String>,
    pub client_id: String,
    pub client_secret_configured: bool,
    pub scopes: Vec<String>,
    pub other_audiences: Vec<String>,
    pub redirect_uri: Option<String>,
    pub redirect_uri_dynamic: bool,
    pub claim_name: String,
    pub claim_prefix: String,
    pub role_policy: String,
    pub groups_claim: String,
    pub roles_claim: String,
    pub email_claim: String,
    pub username_claim: String,
    pub hide_from_ui: bool,
}

impl OidcProvider {
    /// Validate invariants that distinguish a real typed provider response from a placeholder.
    pub fn validate_response(&self) -> Result<()> {
        validate_provider_id(&self.provider_id)?;
        validate_http_url(&self.config_url, "config_url")?;
        if let Some(issuer) = self.issuer.as_deref() {
            validate_http_url(issuer, "issuer")?;
        }
        if let Some(redirect_uri) = self.redirect_uri.as_deref() {
            validate_http_url(redirect_uri, "redirect_uri")?;
        }
        if self.client_id.trim().is_empty() {
            return Err(Error::General(
                "OIDC provider response is missing client_id".to_string(),
            ));
        }
        if !self.scopes.iter().any(|scope| scope == "openid") {
            return Err(Error::General(
                "OIDC provider response scopes do not include openid".to_string(),
            ));
        }
        Ok(())
    }
}

/// Effective OIDC provider collection and restart state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcProviderList {
    pub providers: Vec<OidcProvider>,
    pub restart_required: bool,
}

impl OidcProviderList {
    pub fn validate_response(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        for provider in &self.providers {
            provider.validate_response()?;
            if !ids.insert(provider.provider_id.as_str()) {
                return Err(Error::General(
                    "OIDC provider response contains duplicate provider IDs".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Secret-free OIDC discovery validation request.
///
/// RustFS discovery validation does not authenticate to the token endpoint, so this read-only
/// contract deliberately has no client-secret field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OidcValidationRequest {
    pub provider_id: String,
    pub enabled: bool,
    pub display_name: String,
    pub config_url: String,
    pub issuer: Option<String>,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub other_audiences: Vec<String>,
    pub redirect_uri: Option<String>,
    pub redirect_uri_dynamic: bool,
    pub claim_name: String,
    pub claim_prefix: String,
    pub role_policy: String,
    pub groups_claim: String,
    pub roles_claim: String,
    pub email_claim: String,
    pub username_claim: String,
    pub hide_from_ui: bool,
}

impl OidcValidationRequest {
    pub fn new(provider_id: String, config_url: String, client_id: String) -> Self {
        Self {
            display_name: provider_id.clone(),
            provider_id,
            enabled: true,
            config_url,
            issuer: None,
            client_id,
            scopes: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
            ],
            other_audiences: Vec::new(),
            redirect_uri: None,
            redirect_uri_dynamic: true,
            claim_name: "policy".to_string(),
            claim_prefix: String::new(),
            role_policy: String::new(),
            groups_claim: "groups".to_string(),
            roles_claim: "roles".to_string(),
            email_claim: "email".to_string(),
            username_claim: "preferred_username".to_string(),
            hide_from_ui: false,
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_provider_id(&self.provider_id)?;
        validate_http_url(&self.config_url, "config_url")?;
        if let Some(issuer) = self.issuer.as_deref() {
            validate_http_url(issuer, "issuer")?;
        }
        if self.client_id.trim().is_empty() {
            return Err(Error::InvalidPath(
                "OIDC client ID cannot be empty".to_string(),
            ));
        }
        if !self.scopes.iter().any(|scope| scope == "openid") {
            return Err(Error::InvalidPath(
                "OIDC scopes must include openid".to_string(),
            ));
        }
        if !self.redirect_uri_dynamic && self.redirect_uri.is_none() {
            return Err(Error::InvalidPath(
                "OIDC redirect URI is required when dynamic redirect is disabled".to_string(),
            ));
        }
        if let Some(redirect_uri) = self.redirect_uri.as_deref() {
            validate_http_url(redirect_uri, "redirect_uri")?;
        }
        Ok(())
    }
}

/// Result of a live, non-mutating OIDC discovery validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcValidationResult {
    pub valid: bool,
    pub message: String,
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
}

impl OidcValidationResult {
    pub fn validate_response(&self) -> Result<()> {
        if !self.valid {
            return Err(Error::General(
                "OIDC validation returned an unsuccessful result".to_string(),
            ));
        }
        for (value, field) in [
            (self.issuer.as_deref(), "issuer"),
            (
                self.authorization_endpoint.as_deref(),
                "authorization_endpoint",
            ),
            (self.token_endpoint.as_deref(), "token_endpoint"),
        ] {
            if let Some(value) = value {
                validate_http_url(value, field)?;
            }
        }
        if self.issuer.is_none() || self.authorization_endpoint.is_none() {
            return Err(Error::General(
                "OIDC validation response is incomplete".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
pub trait OidcReadApi: Send + Sync {
    async fn oidc_list_providers(&self) -> Result<OidcProviderList>;
    async fn oidc_get_provider(&self, provider_id: &str) -> Result<OidcProvider>;
    async fn oidc_validate(&self, request: OidcValidationRequest) -> Result<OidcValidationResult>;
}

fn validate_provider_id(provider_id: &str) -> Result<()> {
    if provider_id.is_empty()
        || !provider_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(Error::InvalidPath(
            "OIDC provider ID must contain only ASCII letters, digits, '_' or '-'".to_string(),
        ));
    }
    Ok(())
}

fn validate_http_url(value: &str, field: &str) -> Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|_| Error::InvalidPath(format!("OIDC {field} must be an absolute HTTP URL")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::InvalidPath(format!(
            "OIDC {field} must be an absolute HTTP URL"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_request_is_secret_free_and_checks_openid_scope() {
        let mut request = OidcValidationRequest::new(
            "corp".to_string(),
            "https://idp.example".to_string(),
            "console".to_string(),
        );
        assert!(request.validate().is_ok());
        request.scopes = vec!["profile".to_string()];
        assert!(matches!(request.validate(), Err(Error::InvalidPath(_))));
    }

    #[test]
    fn provider_list_rejects_duplicates_and_incomplete_placeholders() {
        let provider = OidcProvider {
            provider_id: "corp".to_string(),
            source: OidcProviderSource::Persisted,
            editable: true,
            enabled: true,
            display_name: "Corporate".to_string(),
            config_url: "https://idp.example".to_string(),
            issuer: Some("https://idp.example".to_string()),
            client_id: "console".to_string(),
            client_secret_configured: true,
            scopes: vec!["openid".to_string()],
            other_audiences: Vec::new(),
            redirect_uri: None,
            redirect_uri_dynamic: true,
            claim_name: "policy".to_string(),
            claim_prefix: String::new(),
            role_policy: String::new(),
            groups_claim: "groups".to_string(),
            roles_claim: "roles".to_string(),
            email_claim: "email".to_string(),
            username_claim: "preferred_username".to_string(),
            hide_from_ui: false,
        };
        assert!(
            OidcProviderList {
                providers: vec![provider.clone()],
                restart_required: false,
            }
            .validate_response()
            .is_ok()
        );
        assert!(
            OidcProviderList {
                providers: vec![provider.clone(), provider],
                restart_required: false,
            }
            .validate_response()
            .is_err()
        );
    }
}
