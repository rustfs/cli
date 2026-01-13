//! Alias management
//!
//! Aliases are named references to S3-compatible storage endpoints,
//! including connection details and credentials.

use serde::{Deserialize, Serialize};

use crate::config::ConfigManager;
use crate::error::{Error, Result};

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
        Ok(config.aliases)
    }

    /// Get an alias by name
    pub fn get(&self, name: &str) -> Result<Alias> {
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

        let alias = Alias::new("minio", "http://localhost:9000", "minioadmin", "minioadmin");
        manager.set(alias).unwrap();

        let retrieved = manager.get("minio").unwrap();
        assert_eq!(retrieved.name, "minio");
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
}
