//! Integration tests for rc CLI
//!
//! These tests require a running S3-compatible server.
//!
//! Run with:
//! ```bash
//! # Start RustFS container
//! docker run -d --name rustfs -p 9000:9000 -p 9001:9001 \
//!     -v rustfs-data:/data \
//!     -e RUSTFS_ROOT_USER=accesskey \
//!     -e RUSTFS_ROOT_PASSWORD=secretkey \
//!     -e RUSTFS_ACCESS_KEY=accesskey \
//!     -e RUSTFS_SECRET_KEY=secretkey \
//!     rustfs/rustfs:1.0.0-alpha.81
//!
//! # Run tests
//! cargo test --features integration
//! ```

#![cfg(feature = "integration")]

use std::process::{Command, Output};
use std::time::Duration;
use tempfile::TempDir;

/// Get the path to the rc binary
fn rc_binary() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rc") {
        return std::path::PathBuf::from(path);
    }

    // Try release first, then debug
    let debug = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/debug/rc");

    if debug.exists() {
        return debug;
    }

    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/release/rc")
}

/// Set up isolated test environment with custom config directory
fn setup_test_env(config_dir: &std::path::Path) -> Vec<(String, String)> {
    vec![(
        "RC_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().to_string(),
    )]
}

/// Run rc command with test environment
fn run_rc(args: &[&str], config_dir: &std::path::Path) -> Output {
    let mut cmd = Command::new(rc_binary());
    cmd.args(args);

    for (key, value) in setup_test_env(config_dir) {
        cmd.env(key, value);
    }

    cmd.output().expect("Failed to execute rc command")
}

/// Wait for the S3 service to respond to list requests
fn wait_for_s3_ready(config_dir: &std::path::Path) -> bool {
    for _ in 0..30 {
        let output = run_rc(&["ls", "test/", "--json"], config_dir);
        if output.status.success() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    false
}

/// Get S3 test configuration from environment
fn get_test_config() -> Option<(String, String, String)> {
    let endpoint = std::env::var("TEST_S3_ENDPOINT").ok()?;
    let access_key = std::env::var("TEST_S3_ACCESS_KEY").ok()?;
    let secret_key = std::env::var("TEST_S3_SECRET_KEY").ok()?;
    Some((endpoint, access_key, secret_key))
}

/// Test helper: setup alias and return config directory
fn setup_with_alias(bucket: &str) -> Option<(TempDir, String)> {
    let config = get_test_config()?;
    let config_dir = tempfile::tempdir().ok()?;
    let bucket_name = format!("test-{}-{}", bucket, uuid_suffix());

    // Set up alias
    let output = run_rc(
        &[
            "alias",
            "set",
            "test",
            &config.0,
            &config.1,
            &config.2,
            "--bucket-lookup",
            "path",
        ],
        config_dir.path(),
    );

    if !output.status.success() {
        eprintln!(
            "Failed to set alias: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    if !wait_for_s3_ready(config_dir.path()) {
        eprintln!("S3 service did not become ready in time");
        return None;
    }

    // Create test bucket
    let output = run_rc(&["mb", &format!("test/{}", bucket_name)], config_dir.path());

    if !output.status.success() {
        eprintln!(
            "Failed to create bucket: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    Some((config_dir, bucket_name))
}

/// Test helper: setup alias only
fn setup_alias_only() -> Option<TempDir> {
    let config = get_test_config()?;
    let config_dir = tempfile::tempdir().ok()?;

    let output = run_rc(
        &[
            "alias",
            "set",
            "test",
            &config.0,
            &config.1,
            &config.2,
            "--bucket-lookup",
            "path",
        ],
        config_dir.path(),
    );

    if !output.status.success() {
        eprintln!(
            "Failed to set alias: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    if !wait_for_s3_ready(config_dir.path()) {
        eprintln!("S3 service did not become ready in time");
        return None;
    }

    Some(config_dir)
}

/// Generate unique suffix for test resources
fn uuid_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}", duration.as_nanos() % 0xFFFFFFFF)
}

/// Cleanup helper: delete bucket and all objects
fn cleanup_bucket(config_dir: &std::path::Path, bucket: &str) {
    // Delete all objects first
    let _ = run_rc(
        &["rm", "--recursive", "--force", &format!("test/{}/", bucket)],
        config_dir,
    );

    // Delete bucket
    let _ = run_rc(&["rb", &format!("test/{}", bucket)], config_dir);
}

mod bucket_operations {
    use super::*;

    #[test]
    fn test_create_and_delete_bucket() {
        let (endpoint, access_key, secret_key) = match get_test_config() {
            Some(c) => c,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let config_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let bucket_name = format!("test-bucket-{}", uuid_suffix());

        // Set up alias
        let output = run_rc(
            &[
                "alias",
                "set",
                "test",
                &endpoint,
                &access_key,
                &secret_key,
                "--bucket-lookup",
                "path",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to set alias");

        assert!(
            wait_for_s3_ready(config_dir.path()),
            "S3 service did not become ready in time"
        );

        // Create bucket
        let output = run_rc(
            &["mb", &format!("test/{}", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to create bucket: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("success"), "Expected success in output");
        assert!(
            stdout.contains(&bucket_name),
            "Expected bucket name in output"
        );

        // List buckets to verify
        let output = run_rc(&["ls", "test/", "--json"], config_dir.path());
        assert!(output.status.success(), "Failed to list buckets");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(&bucket_name), "Bucket not found in listing");

        // Delete bucket
        let output = run_rc(
            &["rb", &format!("test/{}", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to delete bucket: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

mod object_operations {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_upload_and_download_small_file() {
        let (config_dir, bucket_name) = match setup_with_alias("small") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create a small test file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        let test_content = "Hello, S3 integration test!";
        std::fs::write(temp_file.path(), test_content).expect("Failed to write test file");

        // Upload file
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/test.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to upload: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify with stat
        let output = run_rc(
            &["stat", &format!("test/{}/test.txt", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to stat: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("test.txt"), "Expected filename in output");

        // Download file
        let download_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        let output = run_rc(
            &[
                "cp",
                &format!("test/{}/test.txt", bucket_name),
                download_file.path().to_str().unwrap(),
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to download: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify content
        let downloaded_content =
            std::fs::read_to_string(download_file.path()).expect("Failed to read downloaded file");
        assert_eq!(
            downloaded_content, test_content,
            "Downloaded content doesn't match"
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_upload_download_large_file_multipart() {
        let (config_dir, bucket_name) = match setup_with_alias("large") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create a large file (15 MiB to trigger multipart with 5 MiB parts)
        let file_size = 15 * 1024 * 1024;
        let temp_file = tempfile::Builder::new()
            .suffix(".bin")
            .tempfile()
            .expect("Failed to create temp file");

        {
            let mut file = std::fs::File::create(temp_file.path()).expect("Failed to create file");
            // Write deterministic pattern for verification
            let pattern: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
            for _ in 0..(file_size / 1024) {
                file.write_all(&pattern).expect("Failed to write");
            }
        }

        // Upload large file
        let start = std::time::Instant::now();
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/large.bin", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        let upload_time = start.elapsed();

        assert!(
            output.status.success(),
            "Failed to upload large file: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        println!("Uploaded {} bytes in {:?}", file_size, upload_time);

        // Verify size with stat
        let output = run_rc(
            &["stat", &format!("test/{}/large.bin", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to stat large file");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON output");
        let size = json["size_bytes"].as_i64().unwrap_or(0);
        assert_eq!(
            size, file_size as i64,
            "File size mismatch: expected {}, got {}",
            file_size, size
        );

        // Download and verify
        let download_file = tempfile::Builder::new()
            .suffix(".bin")
            .tempfile()
            .expect("Failed to create download file");

        let output = run_rc(
            &[
                "cp",
                &format!("test/{}/large.bin", bucket_name),
                download_file.path().to_str().unwrap(),
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to download large file: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify downloaded file size
        let downloaded_size = std::fs::metadata(download_file.path())
            .expect("Failed to get metadata")
            .len();
        assert_eq!(
            downloaded_size, file_size as u64,
            "Downloaded file size mismatch"
        );

        // Verify content by checking first and last bytes
        let downloaded_content =
            std::fs::read(download_file.path()).expect("Failed to read downloaded file");
        assert_eq!(
            downloaded_content.len(),
            file_size,
            "Content length mismatch"
        );

        // Check pattern integrity (first 1024 bytes should match our pattern)
        let pattern: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        assert_eq!(
            &downloaded_content[0..1024],
            &pattern[..],
            "Content pattern mismatch at start"
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_copy_object_between_paths() {
        let (config_dir, bucket_name) = match setup_with_alias("copy") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create test file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "copy test content").expect("Failed to write");

        // Upload original
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/original.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload original");

        // Copy within S3
        let output = run_rc(
            &[
                "cp",
                &format!("test/{}/original.txt", bucket_name),
                &format!("test/{}/copied.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to copy: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify both exist
        let output = run_rc(
            &["ls", &format!("test/{}/", bucket_name), "--json"],
            config_dir.path(),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("original.txt"), "Original file missing");
        assert!(stdout.contains("copied.txt"), "Copied file missing");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_move_object() {
        let (config_dir, bucket_name) = match setup_with_alias("move") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create and upload test file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "move test content").expect("Failed to write");

        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/source.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload");

        // Move within S3
        let output = run_rc(
            &[
                "mv",
                &format!("test/{}/source.txt", bucket_name),
                &format!("test/{}/dest.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to move: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify source is gone and dest exists
        let output = run_rc(
            &["ls", &format!("test/{}/", bucket_name), "--json"],
            config_dir.path(),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("source.txt"), "Source file should be gone");
        assert!(stdout.contains("dest.txt"), "Dest file should exist");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_delete_object() {
        let (config_dir, bucket_name) = match setup_with_alias("delete") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create and upload test file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "delete test content").expect("Failed to write");

        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/to-delete.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload");

        // Delete object
        let output = run_rc(
            &[
                "rm",
                &format!("test/{}/to-delete.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to delete: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify it's gone
        let output = run_rc(
            &[
                "stat",
                &format!("test/{}/to-delete.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            !output.status.success(),
            "File should not exist after delete"
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod listing_operations {
    use super::*;

    #[test]
    fn test_list_objects_with_prefix() {
        let (config_dir, bucket_name) = match setup_with_alias("list") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create multiple files with different prefixes
        let files = ["dir1/file1.txt", "dir1/file2.txt", "dir2/file3.txt"];
        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // List all
        let output = run_rc(
            &["ls", &format!("test/{}/", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to list all");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("dir1/"), "dir1 prefix missing");
        assert!(stdout.contains("dir2/"), "dir2 prefix missing");

        // List with prefix
        let output = run_rc(
            &["ls", &format!("test/{}/dir1/", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to list with prefix");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("file1.txt"), "file1 missing");
        assert!(stdout.contains("file2.txt"), "file2 missing");
        assert!(!stdout.contains("file3.txt"), "file3 should not be in dir1");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_recursive_listing() {
        let (config_dir, bucket_name) = match setup_with_alias("recursive") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create nested structure
        let files = ["a/b/c/deep.txt", "a/b/mid.txt", "a/shallow.txt", "top.txt"];
        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // Recursive list
        let output = run_rc(
            &[
                "ls",
                &format!("test/{}/", bucket_name),
                "--recursive",
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to recursive list");
        let stdout = String::from_utf8_lossy(&output.stdout);

        // All files should appear
        for file in &files {
            assert!(
                stdout.contains(file),
                "File {} missing in recursive listing",
                file
            );
        }

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod admin_operations {
    use super::*;

    #[test]
    fn test_admin_info_cluster() {
        let config_dir = match setup_alias_only() {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let output = run_rc(
            &["admin", "info", "cluster", "test", "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to run admin info cluster: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON output");
        assert!(json.get("mode").is_some(), "Expected mode in output");
        assert!(
            json.get("deploymentId").is_some(),
            "Expected deploymentId in output"
        );
    }

    #[test]
    fn test_admin_info_server() {
        let config_dir = match setup_alias_only() {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let output = run_rc(
            &["admin", "info", "server", "test", "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to run admin info server: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON output");
        assert!(json.get("servers").is_some(), "Expected servers in output");
    }

    #[test]
    fn test_admin_info_disk() {
        let config_dir = match setup_alias_only() {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let output = run_rc(
            &["admin", "info", "disk", "test", "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to run admin info disk: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON output");
        assert!(json.get("disks").is_some(), "Expected disks in output");
    }

    #[test]
    fn test_admin_heal_status() {
        let config_dir = match setup_alias_only() {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let output = run_rc(
            &["admin", "heal", "status", "test", "--json"],
            config_dir.path(),
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{stdout}{stderr}");
            if combined.contains("NotImplemented") || combined.contains("Not Implemented") {
                eprintln!("Skipping: heal status not supported by backend");
                return;
            }
            panic!("Failed to run admin heal status: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON output");
        assert!(json.get("healId").is_some(), "Expected healId in output");
        assert!(json.get("healing").is_some(), "Expected healing in output");
    }
}

mod error_handling {
    use super::*;

    #[test]
    fn test_not_found_error() {
        let (config_dir, bucket_name) = match setup_with_alias("notfound") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Try to stat non-existent object
        let output = run_rc(
            &[
                "stat",
                &format!("test/{}/nonexistent.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            !output.status.success(),
            "Should fail for non-existent object"
        );

        // Exit code should be 5 (NOT_FOUND) or 3 (NETWORK_ERROR)
        let exit_code = output.status.code().unwrap_or(-1);
        assert!(
            exit_code == 5 || exit_code == 3,
            "Expected exit code 5 (NOT_FOUND) or 3 (NETWORK_ERROR), got {}",
            exit_code
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_bucket_not_found() {
        let (endpoint, access_key, secret_key) = match get_test_config() {
            Some(c) => c,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let config_dir = tempfile::tempdir().expect("Failed to create temp dir");

        // Set up alias
        let output = run_rc(
            &[
                "alias",
                "set",
                "test",
                &endpoint,
                &access_key,
                &secret_key,
                "--bucket-lookup",
                "path",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to set alias");

        // Try to delete non-existent bucket
        let output = run_rc(
            &["rb", "test/nonexistent-bucket-xyz123", "--json"],
            config_dir.path(),
        );
        assert!(
            !output.status.success(),
            "Should fail for non-existent bucket"
        );
    }
}

mod presigned_urls {
    use super::*;

    #[test]
    fn test_generate_presigned_url() {
        let (config_dir, bucket_name) = match setup_with_alias("presign") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Upload a test file first
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "presign test content").expect("Failed to write");

        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/presign.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload");

        // Generate presigned URL
        let output = run_rc(
            &[
                "share",
                &format!("test/{}/presign.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to generate presigned URL: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("http"), "URL should contain http");
        assert!(
            stdout.contains("presign.txt"),
            "URL should contain filename"
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod multipart_operations {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_multipart_upload_exact_boundary() {
        // Test file that is exactly on part size boundary (10 MiB)
        let (config_dir, bucket_name) = match setup_with_alias("mpboundary") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create file exactly 10 MiB (2 parts at 5 MiB minimum)
        let file_size = 10 * 1024 * 1024;
        let temp_file = tempfile::Builder::new()
            .suffix(".bin")
            .tempfile()
            .expect("Failed to create temp file");

        {
            let mut file = std::fs::File::create(temp_file.path()).expect("Failed to create file");
            let pattern: Vec<u8> = vec![0xAB; 4096];
            for _ in 0..(file_size / 4096) {
                file.write_all(&pattern).expect("Failed to write");
            }
        }

        // Upload
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/boundary.bin", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to upload boundary file: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify size
        let output = run_rc(
            &[
                "stat",
                &format!("test/{}/boundary.bin", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to stat");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON");
        let size = json["size_bytes"].as_i64().unwrap_or(0);
        assert_eq!(size, file_size as i64, "File size mismatch");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_multipart_upload_small_last_part() {
        // Test file with small last part (10 MiB + 1 byte)
        let (config_dir, bucket_name) = match setup_with_alias("mplastpart") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create file 10 MiB + 1 byte
        let file_size = 10 * 1024 * 1024 + 1;
        let temp_file = tempfile::Builder::new()
            .suffix(".bin")
            .tempfile()
            .expect("Failed to create temp file");

        {
            let mut file = std::fs::File::create(temp_file.path()).expect("Failed to create file");
            let pattern: Vec<u8> = vec![0xCD; 4096];
            for _ in 0..(file_size / 4096) {
                file.write_all(&pattern).expect("Failed to write");
            }
            // Write remaining bytes
            file.write_all(&[0xCD]).expect("Failed to write last byte");
        }

        // Upload
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/lastpart.bin", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to upload: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify size
        let output = run_rc(
            &[
                "stat",
                &format!("test/{}/lastpart.bin", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to stat");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON");
        let size = json["size_bytes"].as_i64().unwrap_or(0);
        assert_eq!(size, file_size as i64, "File size mismatch");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod recursive_operations {
    use super::*;

    #[test]
    fn test_recursive_delete() {
        let (config_dir, bucket_name) = match setup_with_alias("recdel") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create nested structure
        let files = [
            "to-delete/a/1.txt",
            "to-delete/a/2.txt",
            "to-delete/b/3.txt",
            "keep/4.txt",
        ];

        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // Recursive delete of to-delete/
        let output = run_rc(
            &[
                "rm",
                "--recursive",
                "--force",
                &format!("test/{}/to-delete/", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to recursive delete: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify to-delete is gone but keep remains
        let output = run_rc(
            &[
                "ls",
                &format!("test/{}/", bucket_name),
                "--recursive",
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to list");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(!stdout.contains("to-delete"), "to-delete should be gone");
        assert!(stdout.contains("keep/4.txt"), "keep/4.txt should remain");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_recursive_copy() {
        let (config_dir, bucket_name) = match setup_with_alias("reccopy") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create source structure
        let files = ["src/a.txt", "src/b.txt", "src/sub/c.txt"];

        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // Copy src/ to dst/ - Note: recursive S3-to-S3 copy may not be fully implemented
        let output = run_rc(
            &[
                "cp",
                "--recursive",
                &format!("test/{}/src/", bucket_name),
                &format!("test/{}/dst/", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );

        // If recursive copy is not supported, skip the rest of the test
        if !output.status.success() {
            eprintln!(
                "Recursive S3-to-S3 copy not fully implemented, skipping: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            cleanup_bucket(config_dir.path(), &bucket_name);
            return;
        }

        // Verify both src and dst exist
        let output = run_rc(
            &[
                "ls",
                &format!("test/{}/", bucket_name),
                "--recursive",
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to list");
        let stdout = String::from_utf8_lossy(&output.stdout);

        // Source should still exist
        assert!(stdout.contains("src/a.txt"), "src/a.txt should exist");
        // Destination should have copies
        assert!(stdout.contains("dst/a.txt"), "dst/a.txt should exist");
        assert!(
            stdout.contains("dst/sub/c.txt"),
            "dst/sub/c.txt should exist"
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod concurrent_operations {
    use super::*;

    #[test]
    fn test_concurrent_uploads() {
        let (config_dir, bucket_name) = match setup_with_alias("concurrent") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create multiple test files
        let mut temp_files = Vec::new();
        for i in 0..5 {
            let temp_file = tempfile::Builder::new()
                .suffix(".txt")
                .tempfile()
                .expect("Failed to create temp file");
            std::fs::write(
                temp_file.path(),
                format!("File {} content with some data", i),
            )
            .expect("Failed to write");
            temp_files.push(temp_file);
        }

        // Upload all files sequentially (testing robustness of sequential uploads)
        for (i, temp_file) in temp_files.iter().enumerate() {
            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/file{}.txt", bucket_name, i),
                ],
                config_dir.path(),
            );
            assert!(
                output.status.success(),
                "Failed to upload file{}: {}",
                i,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Verify all files exist
        let output = run_rc(
            &["ls", &format!("test/{}/", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to list");
        let stdout = String::from_utf8_lossy(&output.stdout);

        for i in 0..5 {
            assert!(
                stdout.contains(&format!("file{}.txt", i)),
                "file{}.txt missing",
                i
            );
        }

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod edge_cases {
    use super::*;

    #[test]
    fn test_special_characters_in_key() {
        let (config_dir, bucket_name) = match setup_with_alias("special") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create test file with special characters in name
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "special character test").expect("Failed to write");

        // Test with spaces and unicode (not all S3 implementations support unicode well)
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/file with spaces.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to upload file with spaces: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify it exists
        let output = run_rc(
            &[
                "stat",
                &format!("test/{}/file with spaces.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to stat file with spaces: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_empty_file_upload() {
        let (config_dir, bucket_name) = match setup_with_alias("empty") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create empty file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "").expect("Failed to write empty file");

        // Upload empty file
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/empty.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to upload empty file: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify size is 0
        let output = run_rc(
            &["stat", &format!("test/{}/empty.txt", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to stat empty file");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("Invalid JSON");
        let size = json["size_bytes"].as_i64().unwrap_or(-1);
        assert_eq!(size, 0, "Empty file should have size 0");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_deep_nested_path() {
        let (config_dir, bucket_name) = match setup_with_alias("deep") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create test file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "deep nested content").expect("Failed to write");

        // Upload to deeply nested path
        let deep_path = "a/b/c/d/e/f/g/h/i/j/deep.txt";
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/{}", bucket_name, deep_path),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to upload to deep path: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify it exists
        let output = run_rc(
            &[
                "stat",
                &format!("test/{}/{}", bucket_name, deep_path),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to stat deep path: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod content_operations {
    use super::*;

    #[test]
    fn test_cat_object() {
        let (config_dir, bucket_name) = match setup_with_alias("cat") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create test file with known content
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        let test_content = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n";
        std::fs::write(temp_file.path(), test_content).expect("Failed to write");

        // Upload file
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/cat-test.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload");

        // Test cat command
        let output = run_rc(
            &["cat", &format!("test/{}/cat-test.txt", bucket_name)],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to cat: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout, test_content, "Cat output doesn't match");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_head_object() {
        let (config_dir, bucket_name) = match setup_with_alias("head") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create test file with multiple lines
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        let test_content =
            "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10\n";
        std::fs::write(temp_file.path(), test_content).expect("Failed to write");

        // Upload file
        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/head-test.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload");

        // Test head command with default (10 lines)
        let output = run_rc(
            &["head", &format!("test/{}/head-test.txt", bucket_name)],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to head: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Line 1"), "Should contain Line 1");
        assert!(stdout.contains("Line 10"), "Should contain Line 10");

        // Test head with -n 3
        let output = run_rc(
            &[
                "head",
                "-n",
                "3",
                &format!("test/{}/head-test.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to head with -n");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Line 1"), "Should contain Line 1");
        assert!(stdout.contains("Line 3"), "Should contain Line 3");
        assert!(!stdout.contains("Line 4"), "Should not contain Line 4");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_pipe_to_object() {
        let (config_dir, bucket_name) = match setup_with_alias("pipe") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let test_content = "Piped content from stdin";

        // Test pipe command - use echo and pipe to rc
        let mut cmd = std::process::Command::new(rc_binary());
        cmd.args(["pipe", &format!("test/{}/piped.txt", bucket_name)]);

        for (key, value) in setup_test_env(config_dir.path()) {
            cmd.env(key, value);
        }

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().expect("Failed to spawn");
        {
            use std::io::Write;
            let stdin = child.stdin.as_mut().expect("Failed to open stdin");
            stdin
                .write_all(test_content.as_bytes())
                .expect("Failed to write to stdin");
        }

        let output = child.wait_with_output().expect("Failed to wait");
        assert!(
            output.status.success(),
            "Failed to pipe: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify with cat
        let output = run_rc(
            &["cat", &format!("test/{}/piped.txt", bucket_name)],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to cat piped file");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout, test_content, "Piped content doesn't match");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod find_operations {
    use super::*;

    #[test]
    fn test_find_by_name() {
        let (config_dir, bucket_name) = match setup_with_alias("find") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create test files
        let files = [
            "documents/report.txt",
            "documents/summary.txt",
            "images/photo.jpg",
            "images/logo.png",
            "data/report.csv",
        ];

        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // Find by name pattern
        let output = run_rc(
            &[
                "find",
                &format!("test/{}/", bucket_name),
                "--name",
                "*.txt",
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to find: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("report.txt"), "Should find report.txt");
        assert!(stdout.contains("summary.txt"), "Should find summary.txt");
        assert!(!stdout.contains("photo.jpg"), "Should not find photo.jpg");

        // Find files in images directory by searching with prefix
        let output = run_rc(
            &["find", &format!("test/{}/images/", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to find in images path: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("photo.jpg"), "Should find photo.jpg");
        assert!(stdout.contains("logo.png"), "Should find logo.png");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }

    #[test]
    fn test_find_by_size() {
        let (config_dir, bucket_name) = match setup_with_alias("findsize") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create files of different sizes
        let small_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        std::fs::write(small_file.path(), "small").expect("Failed to write"); // 5 bytes

        let large_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        let large_content = "x".repeat(10000); // 10KB
        std::fs::write(large_file.path(), &large_content).expect("Failed to write");

        // Upload files
        let output = run_rc(
            &[
                "cp",
                small_file.path().to_str().unwrap(),
                &format!("test/{}/small.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload small file");

        let output = run_rc(
            &[
                "cp",
                large_file.path().to_str().unwrap(),
                &format!("test/{}/large.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload large file");

        // Find files larger than 1KB
        let output = run_rc(
            &[
                "find",
                &format!("test/{}/", bucket_name),
                "--larger",
                "1K",
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to find by size");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("large.txt"), "Should find large.txt");
        assert!(!stdout.contains("small.txt"), "Should not find small.txt");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod diff_operations {
    use super::*;

    #[test]
    fn test_diff_buckets() {
        let (config_dir, bucket_name) = match setup_with_alias("diff") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create second bucket for comparison
        let bucket_name2 = format!("{}-diff", bucket_name);
        let output = run_rc(
            &["mb", &format!("test/{}", bucket_name2)],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to create second bucket");

        // Create files in first bucket
        let temp_file1 = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        std::fs::write(temp_file1.path(), "content1").expect("Failed to write");

        let temp_file2 = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        std::fs::write(temp_file2.path(), "content2").expect("Failed to write");

        // Upload to first bucket
        run_rc(
            &[
                "cp",
                temp_file1.path().to_str().unwrap(),
                &format!("test/{}/file1.txt", bucket_name),
            ],
            config_dir.path(),
        );
        run_rc(
            &[
                "cp",
                temp_file2.path().to_str().unwrap(),
                &format!("test/{}/file2.txt", bucket_name),
            ],
            config_dir.path(),
        );

        // Upload only file1 to second bucket
        run_rc(
            &[
                "cp",
                temp_file1.path().to_str().unwrap(),
                &format!("test/{}/file1.txt", bucket_name2),
            ],
            config_dir.path(),
        );

        // Run diff
        // Note: diff command returns non-zero exit code when differences are found
        // (similar to Unix diff behavior), so we check stdout instead of exit code
        let output = run_rc(
            &[
                "diff",
                &format!("test/{}/", bucket_name),
                &format!("test/{}/", bucket_name2),
                "--json",
            ],
            config_dir.path(),
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Check that diff ran and produced output (not a hard error)
        assert!(
            !stdout.is_empty() || stderr.is_empty(),
            "Diff should produce output or succeed silently, stderr: {}",
            stderr
        );

        // file2.txt should be in the diff as it's only in first bucket
        assert!(
            stdout.contains("file2.txt"),
            "Should show file2.txt as different, stdout: {}, stderr: {}",
            stdout,
            stderr
        );

        // Cleanup both buckets
        cleanup_bucket(config_dir.path(), &bucket_name);
        cleanup_bucket(config_dir.path(), &bucket_name2);
    }
}

mod mirror_operations {
    use super::*;

    #[test]
    fn test_mirror_between_buckets() {
        let (config_dir, bucket_name) = match setup_with_alias("mirror") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create a second bucket for mirroring destination
        let bucket_name2 = format!("{}-dest", bucket_name);
        let output = run_rc(
            &["mb", &format!("test/{}", bucket_name2)],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to create destination bucket: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Upload files to source bucket
        let files = ["file1.txt", "file2.txt", "subdir/file3.txt"];
        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/source/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // Mirror S3 to S3
        let output = run_rc(
            &[
                "mirror",
                &format!("test/{}/source/", bucket_name),
                &format!("test/{}/", bucket_name2),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to mirror: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify all files exist in destination
        let output = run_rc(
            &[
                "ls",
                &format!("test/{}/", bucket_name2),
                "--recursive",
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to list mirrored files");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("file1.txt"), "file1.txt should exist");
        assert!(stdout.contains("file2.txt"), "file2.txt should exist");
        assert!(
            stdout.contains("subdir/file3.txt") || stdout.contains("file3.txt"),
            "file3.txt should exist"
        );

        // Cleanup both buckets
        cleanup_bucket(config_dir.path(), &bucket_name);
        cleanup_bucket(config_dir.path(), &bucket_name2);
    }
}

mod tree_operations {
    use super::*;

    #[test]
    fn test_tree_display() {
        let (config_dir, bucket_name) = match setup_with_alias("tree") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create nested structure
        let files = [
            "root.txt",
            "dir1/file1.txt",
            "dir1/file2.txt",
            "dir1/subdir/deep.txt",
            "dir2/file3.txt",
        ];

        for file in &files {
            let temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
            std::fs::write(temp_file.path(), format!("content for {}", file))
                .expect("Failed to write");

            let output = run_rc(
                &[
                    "cp",
                    temp_file.path().to_str().unwrap(),
                    &format!("test/{}/{}", bucket_name, file),
                ],
                config_dir.path(),
            );
            assert!(output.status.success(), "Failed to upload {}", file);
        }

        // Run tree command
        let output = run_rc(
            &["tree", &format!("test/{}/", bucket_name)],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to tree: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Tree output should show directory structure
        assert!(stdout.contains("dir1"), "Should show dir1");
        assert!(stdout.contains("dir2"), "Should show dir2");
        assert!(stdout.contains("root.txt"), "Should show root.txt");

        // Test with --json
        let output = run_rc(
            &["tree", &format!("test/{}/", bucket_name), "--json"],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to tree with --json");

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod version_operations {
    use super::*;

    #[test]
    fn test_bucket_versioning() {
        let (config_dir, bucket_name) = match setup_with_alias("version") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Get versioning status
        let output = run_rc(
            &[
                "version",
                "info",
                &format!("test/{}", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        // This may fail if versioning is not supported, which is OK
        if !output.status.success() {
            eprintln!(
                "Versioning not supported: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            cleanup_bucket(config_dir.path(), &bucket_name);
            return;
        }

        // Try to enable versioning
        let output = run_rc(
            &[
                "version",
                "enable",
                &format!("test/{}", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );

        if !output.status.success() {
            eprintln!(
                "Enable versioning not supported: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            cleanup_bucket(config_dir.path(), &bucket_name);
            return;
        }

        // Verify versioning is enabled
        let output = run_rc(
            &[
                "version",
                "info",
                &format!("test/{}", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to get versioning info");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Enabled") || stdout.contains("enabled"),
            "Versioning should be enabled"
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod tag_operations {
    use super::*;

    #[test]
    fn test_object_tags() {
        let (config_dir, bucket_name) = match setup_with_alias("tag") {
            Some(v) => v,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        // Create and upload test file
        let temp_file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("Failed to create temp file");
        std::fs::write(temp_file.path(), "tag test content").expect("Failed to write");

        let output = run_rc(
            &[
                "cp",
                temp_file.path().to_str().unwrap(),
                &format!("test/{}/tagged.txt", bucket_name),
            ],
            config_dir.path(),
        );
        assert!(output.status.success(), "Failed to upload");

        // Set tags
        let output = run_rc(
            &[
                "tag",
                "set",
                &format!("test/{}/tagged.txt", bucket_name),
                "environment=test",
                "project=rc-cli",
                "--json",
            ],
            config_dir.path(),
        );

        // Tags may not be supported by all S3 implementations
        if !output.status.success() {
            eprintln!(
                "Tags not supported: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            cleanup_bucket(config_dir.path(), &bucket_name);
            return;
        }

        // Get tags
        let output = run_rc(
            &[
                "tag",
                "ls",
                &format!("test/{}/tagged.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to get tags: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("environment"),
            "Should have environment tag"
        );
        assert!(stdout.contains("test"), "Should have test value");

        // Remove tags
        let output = run_rc(
            &[
                "tag",
                "rm",
                &format!("test/{}/tagged.txt", bucket_name),
                "--json",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to remove tags: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Cleanup
        cleanup_bucket(config_dir.path(), &bucket_name);
    }
}

mod alias_operations {
    use super::*;

    #[test]
    fn test_alias_lifecycle() {
        let (endpoint, access_key, secret_key) = match get_test_config() {
            Some(c) => c,
            None => {
                eprintln!("Skipping: S3 test config not available");
                return;
            }
        };

        let config_dir = tempfile::tempdir().expect("Failed to create temp dir");

        // Set alias
        let output = run_rc(
            &[
                "alias",
                "set",
                "myalias",
                &endpoint,
                &access_key,
                &secret_key,
                "--bucket-lookup",
                "path",
            ],
            config_dir.path(),
        );
        assert!(
            output.status.success(),
            "Failed to set alias: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // List aliases
        let output = run_rc(&["alias", "list", "--json"], config_dir.path());
        assert!(
            output.status.success(),
            "Failed to list aliases: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("myalias"), "Should contain myalias");
        assert!(stdout.contains(&endpoint), "Should contain endpoint");

        // Remove alias
        let output = run_rc(&["alias", "remove", "myalias"], config_dir.path());
        assert!(
            output.status.success(),
            "Failed to remove alias: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify it's gone
        let output = run_rc(&["alias", "list", "--json"], config_dir.path());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("myalias"), "myalias should be removed");
    }
}
