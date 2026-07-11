//! Golden tests for verifying JSON output format stability
//!
//! These tests ensure that the JSON output format remains stable
//! and matches the schema defined in `schemas/output_v1.json`.
//!
//! Run with: `cargo test --features golden`

#![cfg(feature = "golden")]

use std::process::Command;

fn rc_binary() -> &'static str {
    env!("CARGO_BIN_EXE_rc")
}

mod alias_tests {
    use super::*;
    use tempfile::TempDir;

    /// Set up a temporary config directory for isolated testing
    fn setup_test_env() -> TempDir {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        temp_dir
    }

    #[test]
    fn test_alias_list_empty_json() {
        let temp_dir = setup_test_env();
        let config_dir = temp_dir.path().to_str().unwrap();

        let output = Command::new(rc_binary())
            .args(["alias", "list", "--json"])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "Command should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        // Verify structure matches schema
        insta::assert_json_snapshot!("alias_list_empty", json);
    }

    #[test]
    fn test_alias_set_json() {
        let temp_dir = setup_test_env();
        let config_dir = temp_dir.path().to_str().unwrap();

        let output = Command::new(rc_binary())
            .args([
                "alias",
                "set",
                "test-alias",
                "http://localhost:9000",
                "--anonymous",
                "--json",
            ])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "Command should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        // Verify structure matches schema
        insta::assert_json_snapshot!("alias_set_success", json);
    }

    #[test]
    fn test_alias_list_with_aliases_json() {
        let temp_dir = setup_test_env();
        let config_dir = temp_dir.path().to_str().unwrap();

        // First, set up some aliases
        Command::new(rc_binary())
            .args([
                "alias",
                "set",
                "local",
                "http://localhost:9000",
                "--anonymous",
                "--json",
            ])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to set alias");

        Command::new(rc_binary())
            .args([
                "alias",
                "set",
                "s3",
                "https://s3.amazonaws.com",
                "--anonymous",
                "--region",
                "us-west-2",
                "--json",
            ])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to set alias");

        // Now list them
        let output = Command::new(rc_binary())
            .args(["alias", "list", "--json"])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "Command should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        // Verify structure - aliases should be sorted by name for consistent snapshots
        assert!(json["aliases"].is_array());
        assert_eq!(json["aliases"].as_array().unwrap().len(), 2);

        insta::assert_json_snapshot!("alias_list_with_aliases", json);
    }

    #[test]
    fn test_alias_remove_json() {
        let temp_dir = setup_test_env();
        let config_dir = temp_dir.path().to_str().unwrap();

        // First, set up an alias
        Command::new(rc_binary())
            .args([
                "alias",
                "set",
                "to-remove",
                "http://localhost:9000",
                "--anonymous",
                "--json",
            ])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to set alias");

        // Now remove it
        let output = Command::new(rc_binary())
            .args(["alias", "remove", "to-remove", "--json"])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "Command should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        insta::assert_json_snapshot!("alias_remove_success", json);
    }

    #[test]
    fn test_alias_remove_not_found_json() {
        let temp_dir = setup_test_env();
        let config_dir = temp_dir.path().to_str().unwrap();

        let output = Command::new(rc_binary())
            .args(["alias", "remove", "nonexistent", "--json"])
            .env("RC_CONFIG_DIR", config_dir)
            .output()
            .expect("Failed to execute rc");

        // Should fail with NOT_FOUND exit code (5)
        assert!(!output.status.success(), "Command should fail");
        assert_eq!(
            output.status.code(),
            Some(5),
            "Exit code should be 5 (NOT_FOUND)"
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        let json: serde_json::Value =
            serde_json::from_str(&stderr).expect("Output should be valid JSON");

        insta::assert_json_snapshot!("alias_remove_not_found", json);
    }
}

/// Integration tests that require a running S3-compatible server (RustFS)
/// These tests use the TEST_S3_* environment variables
#[cfg(feature = "integration")]
mod s3_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn get_s3_env() -> Option<(String, String, String)> {
        let endpoint = std::env::var("TEST_S3_ENDPOINT").ok()?;
        let access_key = std::env::var("TEST_S3_ACCESS_KEY").ok()?;
        let secret_key = std::env::var("TEST_S3_SECRET_KEY").ok()?;
        Some((endpoint, access_key, secret_key))
    }

    fn setup_test_env_with_alias() -> Option<(TempDir, String)> {
        let (endpoint, access_key, secret_key) = get_s3_env()?;
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let config_dir = temp_dir.path().to_str().unwrap().to_string();

        // Set up the test alias
        let output = Command::new(rc_binary())
            .args([
                "alias",
                "set",
                "test",
                &endpoint,
                &access_key,
                &secret_key,
                "--json",
            ])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to set alias");

        if !output.status.success() {
            eprintln!(
                "Failed to set alias: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }

        Some((temp_dir, config_dir))
    }

    fn unique_bucket_name() -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        format!("test-bucket-{}", ts)
    }

    #[test]
    fn test_mb_json() {
        let (temp_dir, config_dir) =
            setup_test_env_with_alias().expect("S3 golden test setup failed");

        let bucket = unique_bucket_name();
        let output = Command::new(rc_binary())
            .args(["mb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "mb should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        assert_eq!(json["success"], true);
        assert!(json["bucket"].as_str().unwrap().contains("test-bucket"));

        // Cleanup: remove the bucket
        Command::new(rc_binary())
            .args(["rb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .ok();

        drop(temp_dir);
    }

    #[test]
    fn test_rb_json() {
        let (temp_dir, config_dir) =
            setup_test_env_with_alias().expect("S3 golden test setup failed");

        let bucket = unique_bucket_name();

        // First create the bucket
        Command::new(rc_binary())
            .args(["mb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to create bucket");

        // Now remove it
        let output = Command::new(rc_binary())
            .args(["rb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "rb should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        assert_eq!(json["success"], true);

        drop(temp_dir);
    }

    #[test]
    fn test_ls_empty_bucket_json() {
        let (temp_dir, config_dir) =
            setup_test_env_with_alias().expect("S3 golden test setup failed");

        let bucket = unique_bucket_name();

        // Create bucket
        Command::new(rc_binary())
            .args(["mb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to create bucket");

        // List empty bucket
        let output = Command::new(rc_binary())
            .args(["ls", &format!("test/{}/", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "ls should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        assert!(json["items"].is_array());
        assert_eq!(json["truncated"], false);

        // Cleanup
        Command::new(rc_binary())
            .args(["rb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .ok();

        drop(temp_dir);
    }

    #[test]
    fn test_ls_with_objects_json() {
        let (temp_dir, config_dir) =
            setup_test_env_with_alias().expect("S3 golden test setup failed");

        let bucket = unique_bucket_name();

        // Create bucket
        Command::new(rc_binary())
            .args(["mb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to create bucket");

        // Upload a test file using pipe
        let upload = Command::new(rc_binary())
            .args(["pipe", &format!("test/{}/test-file.txt", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(ref mut stdin) = child.stdin {
                    stdin.write_all(b"Hello, World!")?;
                }
                child.wait_with_output()
            })
            .expect("pipe upload should execute");
        assert!(upload.status.success(), "pipe upload should succeed");

        // List bucket with object
        let output = Command::new(rc_binary())
            .args(["ls", &format!("test/{}/", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "ls should succeed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        assert!(json["items"].is_array());
        let items = json["items"].as_array().unwrap();
        assert!(!items.is_empty(), "Should have at least one item");

        // Cleanup: remove object and bucket
        Command::new(rc_binary())
            .args(["rm", &format!("test/{}/test-file.txt", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .ok();

        Command::new(rc_binary())
            .args(["rb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .ok();

        drop(temp_dir);
    }

    #[test]
    fn test_stat_json() {
        let (temp_dir, config_dir) =
            setup_test_env_with_alias().expect("S3 golden test setup failed");

        let bucket = unique_bucket_name();

        // Create bucket
        Command::new(rc_binary())
            .args(["mb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to create bucket");

        // Upload a test file
        let upload = Command::new(rc_binary())
            .args(["pipe", &format!("test/{}/stat-test.txt", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(ref mut stdin) = child.stdin {
                    stdin.write_all(b"Test content for stat")?;
                }
                child.wait_with_output()
            })
            .expect("pipe upload should execute");
        assert!(upload.status.success(), "pipe upload should succeed");

        // Get stat
        let output = Command::new(rc_binary())
            .args(["stat", &format!("test/{}/stat-test.txt", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .expect("Failed to execute rc");

        assert!(output.status.success(), "stat should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&stdout).expect("Output should be valid JSON");

        assert!(json["key"].is_string());
        assert!(json["size_bytes"].is_number());

        // Cleanup
        Command::new(rc_binary())
            .args(["rm", &format!("test/{}/stat-test.txt", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .ok();

        Command::new(rc_binary())
            .args(["rb", &format!("test/{}", bucket), "--json"])
            .env("RC_CONFIG_DIR", &config_dir)
            .output()
            .ok();

        drop(temp_dir);
    }
}
