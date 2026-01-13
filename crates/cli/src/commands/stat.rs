//! stat command - Show object metadata
//!
//! Displays detailed metadata information about an object.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Show object metadata
#[derive(Args, Debug)]
pub struct StatArgs {
    /// Object path (alias/bucket/key)
    pub path: String,

    /// Show version ID information
    #[arg(long)]
    pub version_id: Option<String>,

    /// Rewind to a specific time
    #[arg(long)]
    pub rewind: Option<String>,
}

#[derive(Debug, Serialize)]
struct StatOutput {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_human: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_id: Option<String>,
}

/// Execute the stat command
pub async fn execute(args: StatArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse the path
    let (alias_name, bucket, key) = match parse_stat_path(&args.path) {
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

    // Get object metadata
    match client.head_object(&path).await {
        Ok(info) => {
            if formatter.is_json() {
                let output = StatOutput {
                    name: info.key.clone(),
                    last_modified: info.last_modified.map(|d| d.to_rfc3339()),
                    size_bytes: info.size_bytes,
                    size_human: info.size_human.clone(),
                    etag: info.etag.clone(),
                    content_type: info.content_type.clone(),
                    storage_class: info.storage_class.clone(),
                    version_id: args.version_id,
                };
                formatter.json(&output);
            } else {
                formatter.println(&format!("Name      : {}", info.key));
                if let Some(modified) = info.last_modified {
                    formatter.println(&format!(
                        "Date      : {}",
                        modified.format("%Y-%m-%d %H:%M:%S UTC")
                    ));
                }
                if let Some(size) = info.size_bytes {
                    formatter.println(&format!("Size      : {size} bytes"));
                }
                if let Some(human) = &info.size_human {
                    formatter.println(&format!("Size      : {human}"));
                }
                if let Some(etag) = &info.etag {
                    formatter.println(&format!("ETag      : {etag}"));
                }
                if let Some(ct) = &info.content_type {
                    formatter.println(&format!("Type      : {ct}"));
                }
                if let Some(sc) = &info.storage_class {
                    formatter.println(&format!("Class     : {sc}"));
                }
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
                formatter.error(&format!("Failed to get object metadata: {e}"));
                ExitCode::NetworkError
            }
        }
    }
}

/// Parse stat path into (alias, bucket, key)
fn parse_stat_path(path: &str) -> Result<(String, String, String), String> {
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
    fn test_parse_stat_path_valid() {
        let (alias, bucket, key) = parse_stat_path("myalias/mybucket/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "file.txt");
    }

    #[test]
    fn test_parse_stat_path_with_prefix() {
        let (alias, bucket, key) = parse_stat_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_stat_path_no_key() {
        assert!(parse_stat_path("myalias/mybucket").is_err());
    }

    #[test]
    fn test_parse_stat_path_no_bucket() {
        assert!(parse_stat_path("myalias").is_err());
    }

    #[test]
    fn test_parse_stat_path_empty() {
        assert!(parse_stat_path("").is_err());
    }
}
