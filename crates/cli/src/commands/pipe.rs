//! pipe command - Stream stdin to S3
//!
//! Reads from stdin and uploads to S3. Useful for piping output from other commands.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;
use std::io::Read;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Stream stdin to an object
#[derive(Args, Debug)]
pub struct PipeArgs {
    /// Destination path (alias/bucket/key)
    pub target: String,

    /// Content type for the uploaded object
    #[arg(long, default_value = "application/octet-stream")]
    pub content_type: String,

    /// Storage class for the object
    #[arg(long)]
    pub storage_class: Option<String>,
}

#[derive(Debug, Serialize)]
struct PipeOutput {
    status: &'static str,
    target: String,
    size_bytes: i64,
    size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
}

/// Execute the pipe command
pub async fn execute(args: PipeArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse the target path
    let (alias_name, bucket, key) = match parse_pipe_path(&args.target) {
        Ok(parsed) => parsed,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    if key.is_empty() {
        formatter.error("Object key is required for pipe command.");
        return ExitCode::UsageError;
    }

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

    // Read from stdin
    let mut buffer = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buffer) {
        formatter.error(&format!("Failed to read from stdin: {e}"));
        return ExitCode::GeneralError;
    }

    let size = buffer.len() as i64;
    let target = RemotePath::new(&alias_name, &bucket, &key);
    let target_display = format!("{alias_name}/{bucket}/{key}");

    // Upload
    match client
        .put_object(&target, buffer, Some(&args.content_type))
        .await
    {
        Ok(info) => {
            if formatter.is_json() {
                let output = PipeOutput {
                    status: "success",
                    target: target_display,
                    size_bytes: size,
                    size_human: humansize::format_size(size as u64, humansize::BINARY),
                    etag: info.etag,
                };
                formatter.json(&output);
            } else {
                formatter.success(&format!(
                    "Uploaded to {target_display} ({})",
                    humansize::format_size(size as u64, humansize::BINARY)
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to upload: {e}"));
            ExitCode::NetworkError
        }
    }
}

/// Parse pipe path into (alias, bucket, key)
fn parse_pipe_path(path: &str) -> Result<(String, String, String), String> {
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
    fn test_parse_pipe_path_valid() {
        let (alias, bucket, key) = parse_pipe_path("myalias/mybucket/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "file.txt");
    }

    #[test]
    fn test_parse_pipe_path_with_prefix() {
        let (alias, bucket, key) = parse_pipe_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_pipe_path_no_key() {
        assert!(parse_pipe_path("myalias/mybucket").is_err());
    }

    #[test]
    fn test_parse_pipe_path_empty() {
        assert!(parse_pipe_path("").is_err());
    }
}
