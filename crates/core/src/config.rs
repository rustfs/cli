//! Configuration management
//!
//! This module handles loading, saving, and migrating the rc configuration file.
//! The configuration file is stored in TOML format at ~/.config/rc/config.toml.
//!
//! PROTECTED FILE: Changes to schema_version require migration support.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::alias::Alias;
use crate::error::{Error, Result};

/// Current configuration schema version
///
/// IMPORTANT: Bumping this version requires:
/// 1. Adding a migration in migrations/
/// 2. Updating migration tests
/// 3. Marking the change as BREAKING
pub const SCHEMA_VERSION: u32 = 1;

/// Default output format
const DEFAULT_OUTPUT: &str = "human";

/// Default color setting
const DEFAULT_COLOR: &str = "auto";

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Schema version for migration support
    pub schema_version: u32,

    /// Default settings
    #[serde(default)]
    pub defaults: Defaults,

    /// Configured aliases
    #[serde(default)]
    pub aliases: Vec<Alias>,
}

/// Default settings for CLI behavior
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    /// Output format: "human" or "json"
    #[serde(default = "default_output")]
    pub output: String,

    /// Color mode: "auto", "always", or "never"
    #[serde(default = "default_color")]
    pub color: String,

    /// Show progress bars
    #[serde(default = "default_true")]
    pub progress: bool,
}

fn default_output() -> String {
    DEFAULT_OUTPUT.to_string()
}

fn default_color() -> String {
    DEFAULT_COLOR.to_string()
}

fn default_true() -> bool {
    true
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            output: default_output(),
            color: default_color(),
            progress: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            defaults: Defaults::default(),
            aliases: Vec::new(),
        }
    }
}

/// Configuration manager handles loading and saving config
#[derive(Debug)]
pub struct ConfigManager {
    config_path: PathBuf,
}

impl ConfigManager {
    /// Create a new ConfigManager with the default config path
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| Error::Config("Could not determine config directory".into()))?;
        let config_path = config_dir.join("rc").join("config.toml");
        Ok(Self { config_path })
    }

    /// Create a ConfigManager with a custom path (useful for testing)
    pub fn with_path(path: PathBuf) -> Self {
        Self { config_path: path }
    }

    /// Get the configuration file path
    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    /// Load configuration from disk
    ///
    /// If the configuration file doesn't exist, returns a default configuration.
    /// If the schema version doesn't match, attempts migration.
    pub fn load(&self) -> Result<Config> {
        if !self.config_path.exists() {
            return Ok(Config::default());
        }

        let content = std::fs::read_to_string(&self.config_path)?;
        let mut config: Config = toml::from_str(&content)?;

        // Check schema version and migrate if necessary
        if config.schema_version < SCHEMA_VERSION {
            config = self.migrate(config)?;
        } else if config.schema_version > SCHEMA_VERSION {
            return Err(Error::Config(format!(
                "Configuration file version {} is newer than supported version {}. Please upgrade rc.",
                config.schema_version, SCHEMA_VERSION
            )));
        }

        Ok(config)
    }

    /// Save configuration to disk
    ///
    /// Creates parent directories if they don't exist.
    /// Sets file permissions to 600 (owner read/write only).
    pub fn save(&self, config: &Config) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(config)?;
        std::fs::write(&self.config_path, content)?;

        // Set restrictive permissions on Unix systems
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&self.config_path, permissions)?;
        }

        Ok(())
    }

    /// Migrate configuration from older schema version
    fn migrate(&self, config: Config) -> Result<Config> {
        let mut config = config;

        // Add migration logic here when schema version is bumped
        // Example:
        // if config.schema_version == 1 {
        //     config = migrate_v1_to_v2(config)?;
        // }

        config.schema_version = SCHEMA_VERSION;
        Ok(config)
    }
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new().expect("Failed to create default ConfigManager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_config_manager() -> (ConfigManager, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        let manager = ConfigManager::with_path(config_path);
        (manager, temp_dir)
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
        assert_eq!(config.defaults.output, "human");
        assert_eq!(config.defaults.color, "auto");
        assert!(config.defaults.progress);
        assert!(config.aliases.is_empty());
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let (manager, _temp_dir) = temp_config_manager();
        let config = manager.load().unwrap();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn test_save_and_load() {
        let (manager, _temp_dir) = temp_config_manager();

        let mut config = Config::default();
        config.aliases.push(Alias {
            name: "test".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            access_key: "minioadmin".to_string(),
            secret_key: "minioadmin".to_string(),
            region: "us-east-1".to_string(),
            signature: "v4".to_string(),
            bucket_lookup: "auto".to_string(),
            insecure: false,
            ca_bundle: None,
            retry: None,
            timeout: None,
        });

        manager.save(&config).unwrap();
        let loaded = manager.load().unwrap();

        assert_eq!(loaded.aliases.len(), 1);
        assert_eq!(loaded.aliases[0].name, "test");
    }

    #[test]
    fn test_schema_version_too_new() {
        let (manager, _temp_dir) = temp_config_manager();

        let content = format!(
            r#"
            schema_version = {}
            "#,
            SCHEMA_VERSION + 1
        );
        std::fs::write(manager.config_path(), content).unwrap();

        let result = manager.load();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("newer than supported"));
    }
}
