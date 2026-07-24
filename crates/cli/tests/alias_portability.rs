//! Alias export/import portability and exit-code contracts.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rc_core::{Alias, ConfigManager};

fn rc_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return PathBuf::from(path);
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has parent directory")
        .parent()
        .expect("workspace root exists")
        .to_path_buf();
    let debug_binary = workspace_root.join("target/debug/rc");
    if debug_binary.exists() {
        return debug_binary;
    }
    workspace_root.join("target/release/rc")
}

fn run_rc(args: &[&str], config_dir: &Path) -> Output {
    let mut command = Command::new(rc_binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RC_HOST_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("RC_CONFIG_DIR", config_dir)
        .output()
        .expect("execute rc")
}

fn seed_alias(config_dir: &Path, alias: Alias) {
    let manager = ConfigManager::with_path(config_dir.join("config.toml"));
    let mut config = manager.load().unwrap();
    config.aliases.push(alias);
    manager.save(&config).unwrap();
}

#[test]
fn export_succeeds_with_redacted_deterministic_json() {
    let config_dir = tempfile::tempdir().unwrap();
    seed_alias(
        config_dir.path(),
        Alias::new("zeta", "http://zeta:9000", "zeta-access", "zeta-secret"),
    );
    seed_alias(
        config_dir.path(),
        Alias::new("alpha", "http://alpha:9000", "alpha-access", "alpha-secret"),
    );

    let output = run_rc(&["alias", "export"], config_dir.path());

    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["aliases"][0]["name"], "alpha");
    assert_eq!(document["aliases"][1]["name"], "zeta");
    let encoded = String::from_utf8(output.stdout).unwrap();
    assert!(!encoded.contains("alpha-access"));
    assert!(!encoded.contains("alpha-secret"));
    assert!(!encoded.contains("zeta-access"));
    assert!(!encoded.contains("zeta-secret"));
}

#[test]
fn export_credentials_require_acknowledgement_exit_two() {
    let config_dir = tempfile::tempdir().unwrap();

    let output = run_rc(
        &["alias", "export", "--include-credentials"],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--acknowledge-credentials"));
}

#[test]
fn import_succeeds_for_a_valid_redacted_document() {
    let config_dir = tempfile::tempdir().unwrap();
    let input = config_dir.path().join("aliases.json");
    std::fs::write(
        &input,
        r#"{
          "schema_version": 1,
          "aliases": [{
            "name": "local",
            "endpoint": "http://localhost:9000",
            "region": "us-east-1",
            "signature": "v4",
            "bucket_lookup": "auto"
          }]
        }"#,
    )
    .unwrap();

    let output = run_rc(
        &["alias", "import", input.to_str().unwrap()],
        config_dir.path(),
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let alias = rc_core::AliasManager::with_config_manager(ConfigManager::with_path(
        config_dir.path().join("config.toml"),
    ))
    .get("local")
    .unwrap();
    assert!(alias.access_key.is_empty());
    assert!(alias.secret_key.is_empty());
}

#[test]
fn malformed_import_exits_two_without_writing() {
    let config_dir = tempfile::tempdir().unwrap();
    let input = config_dir.path().join("aliases.json");
    std::fs::write(&input, r#"{"schema_version":1,"aliases":"wrong"}"#).unwrap();

    let output = run_rc(
        &["alias", "import", input.to_str().unwrap()],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(!config_dir.path().join("config.toml").exists());
}

#[test]
fn import_conflict_exits_six_without_partial_write() {
    let config_dir = tempfile::tempdir().unwrap();
    seed_alias(
        config_dir.path(),
        Alias::new("existing", "http://old:9000", "access", "secret"),
    );
    let input = config_dir.path().join("aliases.json");
    std::fs::write(
        &input,
        r#"{
          "schema_version": 1,
          "aliases": [
            {
              "name": "new",
              "endpoint": "http://new:9000",
              "region": "us-east-1",
              "signature": "v4",
              "bucket_lookup": "auto"
            },
            {
              "name": "existing",
              "endpoint": "http://replacement:9000",
              "region": "us-east-1",
              "signature": "v4",
              "bucket_lookup": "auto"
            }
          ]
        }"#,
    )
    .unwrap();

    let output = run_rc(
        &["alias", "import", input.to_str().unwrap()],
        config_dir.path(),
    );

    assert_eq!(output.status.code(), Some(6));
    let manager = rc_core::AliasManager::with_config_manager(ConfigManager::with_path(
        config_dir.path().join("config.toml"),
    ));
    assert!(matches!(
        manager.get("new"),
        Err(rc_core::Error::AliasNotFound(_))
    ));
    assert_eq!(manager.get("existing").unwrap().endpoint, "http://old:9000");
}
