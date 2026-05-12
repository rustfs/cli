//! Alias management
//!
//! Aliases are named references to S3-compatible storage endpoints,
//! including connection details and credentials.

use std::env;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::ConfigManager;
use crate::error::{Error, Result};

const RC_HOST_PREFIX: &str = "RC_HOST_";

/// Retry configuration for an alias
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// Initial backoff duration in milliseconds
    #[serde(default = "default_initial_backoff")]
    pub initial_backoff_ms: u64,

    /// Maximum backoff duration in milliseconds
    #[serde(default = "default_max_backoff")]
    pub max_backoff_ms: u64,
}

fn default_max_attempts() -> u32 {
    3
}

fn default_initial_backoff() -> u64 {
    100
}

fn default_max_backoff() -> u64 {
    10000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            initial_backoff_ms: default_initial_backoff(),
            max_backoff_ms: default_max_backoff(),
        }
    }
}

/// Timeout configuration for an alias
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Connection timeout in milliseconds
    #[serde(default = "default_connect_timeout")]
    pub connect_ms: u64,

    /// Read timeout in milliseconds
    #[serde(default = "default_read_timeout")]
    pub read_ms: u64,
}

fn default_connect_timeout() -> u64 {
    5000
}

fn default_read_timeout() -> u64 {
    30000
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_ms: default_connect_timeout(),
            read_ms: default_read_timeout(),
        }
    }
}

/// An alias represents a named S3-compatible storage endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alias {
    /// Unique name for this alias
    pub name: String,

    /// S3 endpoint URL
    pub endpoint: String,

    /// Access key ID
    pub access_key: String,

    /// Secret access key
    pub secret_key: String,

    /// AWS region
    #[serde(default = "default_region")]
    pub region: String,

    /// Signature version: "v4" or "v2"
    #[serde(default = "default_signature")]
    pub signature: String,

    /// Bucket lookup style: "auto", "path", or "dns"
    #[serde(default = "default_bucket_lookup")]
    pub bucket_lookup: String,

    /// Allow insecure TLS connections
    #[serde(default)]
    pub insecure: bool,

    /// Path to custom CA bundle
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_bundle: Option<String>,

    /// Retry configuration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,

    /// Timeout configuration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutConfig>,
}

/// Validate that an alias endpoint is a usable HTTP(S) URL.
pub fn validate_alias_endpoint(value: &str) -> Result<()> {
    if value.contains('{') || value.contains('}') {
        return Err(Error::Config(
            "Endpoint must be a single S3 service URL; RustFS volume expansion patterns are not supported".into(),
        ));
    }

    let url = Url::parse(value)
        .map_err(|e| Error::Config(format!("Endpoint must be a valid URL: {e}")))?;

    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Config(
            "Endpoint must not include credentials; pass access key and secret key as separate arguments".into(),
        ));
    }

    validate_http_endpoint_url(&url, "Endpoint")
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_signature() -> String {
    "v4".to_string()
}

fn default_bucket_lookup() -> String {
    "auto".to_string()
}

impl Alias {
    /// Create a new alias with required fields
    pub fn new(
        name: impl Into<String>,
        endpoint: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            endpoint: endpoint.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: default_region(),
            signature: default_signature(),
            bucket_lookup: default_bucket_lookup(),
            insecure: false,
            ca_bundle: None,
            retry: None,
            timeout: None,
        }
    }

    /// Get the effective retry configuration
    pub fn retry_config(&self) -> RetryConfig {
        self.retry.clone().unwrap_or_default()
    }

    /// Get the effective timeout configuration
    pub fn timeout_config(&self) -> TimeoutConfig {
        self.timeout.clone().unwrap_or_default()
    }
}

fn env_alias_var_name(name: &str) -> String {
    format!("{RC_HOST_PREFIX}{name}")
}

fn env_alias(name: &str) -> Result<Option<Alias>> {
    let var_name = env_alias_var_name(name);
    let Some(value) = env::var_os(&var_name) else {
        return Ok(None);
    };

    let value = value
        .into_string()
        .map_err(|_| Error::Config(format!("{var_name} must be valid UTF-8")))?;
    parse_env_alias(name, &value).map(Some)
}

fn env_aliases() -> Result<Vec<Alias>> {
    let mut vars = Vec::new();

    for (key, value) in env::vars_os() {
        let Ok(key) = key.into_string() else {
            continue;
        };

        if !key.starts_with(RC_HOST_PREFIX) {
            continue;
        }

        let value = value
            .into_string()
            .map_err(|_| Error::Config(format!("{key} must be valid UTF-8")))?;
        vars.push((key, value));
    }

    env_aliases_from_vars(vars)
}

fn env_aliases_from_vars<I, K, V>(vars: I) -> Result<Vec<Alias>>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut aliases = Vec::new();

    for (key, value) in vars {
        let key = key.as_ref();
        let Some(alias_name) = key.strip_prefix(RC_HOST_PREFIX) else {
            continue;
        };

        if alias_name.is_empty() {
            return Err(Error::Config("RC_HOST_ must include an alias name".into()));
        }

        aliases.push(parse_env_alias(alias_name, value.as_ref())?);
    }

    aliases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(aliases)
}

fn parse_env_alias(name: &str, value: &str) -> Result<Alias> {
    let var_name = env_alias_var_name(name);
    let mut url = Url::parse(value)
        .map_err(|e| Error::Config(format!("{var_name} must be a valid URL: {e}")))?;

    validate_http_endpoint_url(&url, &var_name)?;

    let access_key = url.username();
    let Some(secret_key) = url.password() else {
        return Err(Error::Config(format!(
            "{var_name} must include access key and secret key credentials"
        )));
    };

    if access_key.is_empty() || secret_key.is_empty() {
        return Err(Error::Config(format!(
            "{var_name} must include non-empty access key and secret key credentials"
        )));
    }

    let access_key = decode_env_alias_credential(access_key, &var_name, "access key")?;
    let secret_key = decode_env_alias_credential(secret_key, &var_name, "secret key")?;

    url.set_username("").map_err(|()| {
        Error::Config(format!("{var_name} credentials cannot be removed from URL"))
    })?;
    url.set_password(None).map_err(|()| {
        Error::Config(format!("{var_name} credentials cannot be removed from URL"))
    })?;

    let endpoint = url.as_str().trim_end_matches('/').to_string();
    Ok(Alias::new(name, endpoint, access_key, secret_key))
}

fn validate_http_endpoint_url(url: &Url, label: &str) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::Config(format!(
            "{label} must use an http or https URL"
        )));
    }

    if url.host_str().is_none() {
        return Err(Error::Config(format!("{label} must include a host")));
    }

    Ok(())
}

fn decode_env_alias_credential(value: &str, var_name: &str, field: &str) -> Result<String> {
    if has_invalid_percent_encoding(value) {
        return Err(Error::Config(format!(
            "{var_name} contains invalid percent-encoding in {field}"
        )));
    }

    urlencoding::decode(value)
        .map(|decoded| decoded.into_owned())
        .map_err(|e| {
            Error::Config(format!(
                "{var_name} contains invalid percent-encoding in {field}: {e}"
            ))
        })
}

fn has_invalid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }

        if index + 2 >= bytes.len()
            || !bytes[index + 1].is_ascii_hexdigit()
            || !bytes[index + 2].is_ascii_hexdigit()
        {
            return true;
        }

        index += 3;
    }

    false
}

fn merge_env_aliases(mut aliases: Vec<Alias>, env_aliases: Vec<Alias>) -> Vec<Alias> {
    for env_alias in env_aliases {
        aliases.retain(|alias| alias.name != env_alias.name);
        aliases.push(env_alias);
    }

    aliases
}

/// Manager for alias operations
pub struct AliasManager {
    config_manager: ConfigManager,
}

impl AliasManager {
    /// Create a new AliasManager with a specific ConfigManager
    pub fn with_config_manager(config_manager: ConfigManager) -> Self {
        Self { config_manager }
    }

    /// Create a new AliasManager using the default config location
    pub fn new() -> Result<Self> {
        let config_manager = ConfigManager::new()?;
        Ok(Self { config_manager })
    }

    /// List all configured aliases
    pub fn list(&self) -> Result<Vec<Alias>> {
        let config = self.config_manager.load()?;
        let env_aliases = env_aliases()?;
        Ok(merge_env_aliases(config.aliases, env_aliases))
    }

    /// Get an alias by name
    pub fn get(&self, name: &str) -> Result<Alias> {
        if let Some(alias) = env_alias(name)? {
            return Ok(alias);
        }

        let config = self.config_manager.load()?;
        config
            .aliases
            .into_iter()
            .find(|a| a.name == name)
            .ok_or_else(|| Error::AliasNotFound(name.to_string()))
    }

    /// Add or update an alias
    pub fn set(&self, alias: Alias) -> Result<()> {
        let mut config = self.config_manager.load()?;

        // Remove existing alias with same name
        config.aliases.retain(|a| a.name != alias.name);
        config.aliases.push(alias);

        self.config_manager.save(&config)
    }

    /// Remove an alias
    pub fn remove(&self, name: &str) -> Result<()> {
        let mut config = self.config_manager.load()?;
        let original_len = config.aliases.len();

        config.aliases.retain(|a| a.name != name);

        if config.aliases.len() == original_len {
            return Err(Error::AliasNotFound(name.to_string()));
        }

        self.config_manager.save(&config)
    }

    /// Check if an alias exists
    pub fn exists(&self, name: &str) -> Result<bool> {
        if env_alias(name)?.is_some() {
            return Ok(true);
        }

        let config = self.config_manager.load()?;
        Ok(config.aliases.iter().any(|a| a.name == name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_alias_manager() -> (AliasManager, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        let config_manager = ConfigManager::with_path(config_path);
        let alias_manager = AliasManager::with_config_manager(config_manager);
        (alias_manager, temp_dir)
    }

    #[test]
    fn test_alias_new() {
        let alias = Alias::new("test", "http://localhost:9000", "access", "secret");
        assert_eq!(alias.name, "test");
        assert_eq!(alias.endpoint, "http://localhost:9000");
        assert_eq!(alias.region, "us-east-1");
        assert_eq!(alias.signature, "v4");
        assert_eq!(alias.bucket_lookup, "auto");
        assert!(!alias.insecure);
    }

    #[test]
    fn test_alias_manager_set_and_get() {
        let (manager, _temp_dir) = temp_alias_manager();

        let alias = Alias::new("local", "http://localhost:9000", "accesskey", "secretkey");
        manager.set(alias).unwrap();

        let retrieved = manager.get("local").unwrap();
        assert_eq!(retrieved.name, "local");
        assert_eq!(retrieved.endpoint, "http://localhost:9000");
    }

    #[test]
    fn test_alias_manager_list() {
        let (manager, _temp_dir) = temp_alias_manager();

        manager
            .set(Alias::new("a", "http://a:9000", "a", "a"))
            .unwrap();
        manager
            .set(Alias::new("b", "http://b:9000", "b", "b"))
            .unwrap();

        let aliases = manager.list().unwrap();
        assert_eq!(aliases.len(), 2);
    }

    #[test]
    fn test_alias_manager_remove() {
        let (manager, _temp_dir) = temp_alias_manager();

        manager
            .set(Alias::new("test", "http://localhost:9000", "a", "b"))
            .unwrap();
        assert!(manager.exists("test").unwrap());

        manager.remove("test").unwrap();
        assert!(!manager.exists("test").unwrap());
    }

    #[test]
    fn test_alias_manager_remove_not_found() {
        let (manager, _temp_dir) = temp_alias_manager();

        let result = manager.remove("nonexistent");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::AliasNotFound(_)));
    }

    #[test]
    fn test_alias_manager_get_not_found() {
        let (manager, _temp_dir) = temp_alias_manager();

        let result = manager.get("nonexistent");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::AliasNotFound(_)));
    }

    #[test]
    fn test_alias_update_existing() {
        let (manager, _temp_dir) = temp_alias_manager();

        manager
            .set(Alias::new("test", "http://old:9000", "a", "b"))
            .unwrap();
        manager
            .set(Alias::new("test", "http://new:9000", "c", "d"))
            .unwrap();

        let aliases = manager.list().unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].endpoint, "http://new:9000");
    }

    #[test]
    fn test_parse_rc_host_alias() {
        let alias =
            parse_env_alias("myalias", "https://ACCESS_KEY:SECRET_KEY@rustfs.local:9000").unwrap();

        assert_eq!(alias.name, "myalias");
        assert_eq!(alias.endpoint, "https://rustfs.local:9000");
        assert_eq!(alias.access_key, "ACCESS_KEY");
        assert_eq!(alias.secret_key, "SECRET_KEY");
        assert_eq!(alias.region, "us-east-1");
        assert_eq!(alias.bucket_lookup, "auto");
    }

    #[test]
    fn test_validate_alias_endpoint_rejects_volume_expansion_endpoint() {
        let result = validate_alias_endpoint("http://rustfs-node{1...32}:9000");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("RustFS volume expansion patterns are not supported")
        );
    }

    #[test]
    fn test_validate_alias_endpoint_rejects_missing_scheme() {
        let result = validate_alias_endpoint("localhost:9000");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint must use an http or https URL")
        );
    }

    #[test]
    fn test_validate_alias_endpoint_rejects_non_http_scheme() {
        let result = validate_alias_endpoint("ftp://localhost:9000");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint must use an http or https URL")
        );
    }

    #[test]
    fn test_validate_alias_endpoint_rejects_embedded_credentials() {
        let result = validate_alias_endpoint("http://access:secret@localhost:9000");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint must not include credentials")
        );
    }

    #[test]
    fn test_validate_alias_endpoint_accepts_http_url_with_host() {
        validate_alias_endpoint("http://localhost:9000").unwrap();
        validate_alias_endpoint("https://s3.amazonaws.com").unwrap();
    }

    #[test]
    fn test_parse_rc_host_alias_decodes_credentials() {
        let alias =
            parse_env_alias("encoded", "https://ACCESS%2FKEY:SECRET%40KEY@rustfs.local").unwrap();

        assert_eq!(alias.access_key, "ACCESS/KEY");
        assert_eq!(alias.secret_key, "SECRET@KEY");
    }

    #[test]
    fn test_parse_rc_host_alias_requires_credentials() {
        let result = parse_env_alias("missing", "https://rustfs.local");

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::Config(_)));
    }

    #[test]
    fn test_parse_rc_host_alias_rejects_invalid_percent_encoding() {
        let result = parse_env_alias("invalid", "https://ACCESS_KEY:SECRET%ZZ@rustfs.local");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("invalid percent-encoding in secret key"));
        assert!(!error.contains("SECRET"));
    }

    #[test]
    fn test_parse_rc_host_alias_rejects_non_utf8_percent_encoded_secret_key() {
        let result = parse_env_alias("invalid", "https://ACCESS_KEY:SECRET%FF@rustfs.local");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("invalid percent-encoding in secret key"));
        assert!(!error.contains("ACCESS_KEY"));
        assert!(!error.contains("SECRET"));
    }

    #[test]
    fn test_parse_rc_host_alias_rejects_invalid_access_key_percent_encoding() {
        let result = parse_env_alias("invalid", "https://ACCESS%ZZKEY:SECRET_KEY@rustfs.local");

        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("invalid percent-encoding in access key"));
        assert!(!error.contains("ACCESS"));
        assert!(!error.contains("SECRET_KEY"));
    }

    #[test]
    fn test_env_aliases_from_vars_filters_rc_host_prefix() {
        let aliases = env_aliases_from_vars(vec![
            (
                "RC_HOST_second".to_string(),
                "https://key2:secret2@second.local".to_string(),
            ),
            ("UNRELATED".to_string(), "ignored".to_string()),
            (
                "RC_HOST_first".to_string(),
                "https://key1:secret1@first.local".to_string(),
            ),
        ])
        .unwrap();

        assert_eq!(aliases.len(), 2);
        assert_eq!(aliases[0].name, "first");
        assert_eq!(aliases[1].name, "second");
    }

    #[test]
    fn test_merge_env_aliases_overrides_config_alias() {
        let config_alias = Alias::new("local", "http://old:9000", "old", "old");
        let env_alias = parse_env_alias("local", "https://new:secret@new.local").unwrap();

        let aliases = merge_env_aliases(vec![config_alias], vec![env_alias]);

        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].endpoint, "https://new.local");
        assert_eq!(aliases[0].access_key, "new");
    }
}
