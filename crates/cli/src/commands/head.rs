//! head command - Display first N lines of an object
//!
//! Outputs the first N lines (or bytes) of an object to stdout.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use std::io::{self, Write};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Display first N lines of an object
#[derive(Args, Debug)]
pub struct HeadArgs {
    /// Object path (alias/bucket/key)
    pub path: String,

    /// Number of lines to display (default: 10)
    #[arg(short = 'n', long, default_value = "10")]
    pub lines: usize,

    /// Display first N bytes instead of lines
    #[arg(short = 'c', long)]
    pub bytes: Option<usize>,

    /// Specific version ID to retrieve
    #[arg(long)]
    pub version_id: Option<String>,
}

/// Execute the head command
pub async fn execute(args: HeadArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse the path
    let (alias_name, bucket, key) = match parse_head_path(&args.path) {
        Ok(parsed) => parsed,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    // Load alias
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&alias_name) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            return ExitCode::NotFound;
        }
    };

    // Create S3 client
    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create S3 client: {e}"));
            return ExitCode::NetworkError;
        }
    };

    let path = RemotePath::new(&alias_name, &bucket, &key);

    // Get object content
    match client.get_object(&path).await {
        Ok(data) => {
            let output = if let Some(num_bytes) = args.bytes {
                // Output first N bytes
                let end = num_bytes.min(data.len());
                &data[..end]
            } else {
                // Output first N lines
                let content = String::from_utf8_lossy(&data);
                let lines: Vec<&str> = content.lines().take(args.lines).collect();
                let result = lines.join("\n");

                // Write string content and add newline
                if let Err(e) = writeln!(io::stdout(), "{result}") {
                    formatter.error(&format!("Failed to write to stdout: {e}"));
                    return ExitCode::GeneralError;
                }
                return ExitCode::Success;
            };

            // Write bytes directly to stdout
            if let Err(e) = io::stdout().write_all(output) {
                formatter.error(&format!("Failed to write to stdout: {e}"));
                return ExitCode::GeneralError;
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                formatter.error(&format!("Object not found: {}", args.path));
                ExitCode::NotFound
            } else if err_str.contains("AccessDenied") {
                formatter.error(&format!("Access denied: {}", args.path));
                ExitCode::AuthError
            } else {
                formatter.error(&format!("Failed to get object: {e}"));
                ExitCode::NetworkError
            }
        }
    }
}

/// Parse head path into (alias, bucket, key)
fn parse_head_path(path: &str) -> Result<(String, String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    if parts.len() < 3 {
        return Err(format!(
            "Invalid path format: '{path}'. Expected: alias/bucket/key"
        ));
    }

    let alias = parts[0].to_string();
    let bucket = parts[1].to_string();
    let key = parts[2].to_string();

    if bucket.is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }

    if key.is_empty() {
        return Err("Object key cannot be empty".to_string());
    }

    Ok((alias, bucket, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_head_path_valid() {
        let (alias, bucket, key) = parse_head_path("myalias/mybucket/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "file.txt");
    }

    #[test]
    fn test_parse_head_path_with_prefix() {
        let (alias, bucket, key) = parse_head_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_head_path_no_key() {
        assert!(parse_head_path("myalias/mybucket").is_err());
    }

    #[test]
    fn test_parse_head_path_empty() {
        assert!(parse_head_path("").is_err());
    }
}
