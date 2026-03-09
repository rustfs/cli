//! stat command - Show object metadata
//!
//! Displays detailed metadata information about an object.

use clap::Args;
use rc_core::{AliasManager, ObjectInfo, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::BTreeMap;

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
    #[serde(skip_serializing_if = "metadata_is_none_or_empty")]
    metadata: Option<BTreeMap<String, String>>,
}

/// Returns true if metadata is None or contains an empty map.
fn metadata_is_none_or_empty(metadata: &Option<BTreeMap<String, String>>) -> bool {
    match metadata {
        None => true,
        Some(m) => m.is_empty(),
    }
}

/// Build display metadata fields for human-readable output.
///
/// Includes Content-Type and all user-defined metadata.
fn build_display_metadata(info: &ObjectInfo) -> BTreeMap<String, String> {
    let mut display_metadata = BTreeMap::new();

    if let Some(content_type) = &info.content_type {
        display_metadata.insert("Content-Type".to_string(), content_type.clone());
    }

    if let Some(metadata) = &info.metadata {
        for (key, value) in metadata {
            display_metadata.insert(
                format!("X-Amz-Meta-{}", capitalize_meta_key(key)),
                value.clone(),
            );
        }
    }

    display_metadata
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
                    last_modified: info.last_modified.map(|d| d.to_string()),
                    size_bytes: info.size_bytes,
                    size_human: info.size_human.clone(),
                    etag: info.etag.clone(),
                    content_type: info.content_type.clone(),
                    storage_class: info.storage_class.clone(),
                    version_id: args.version_id,
                    metadata: info
                        .metadata
                        .as_ref()
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<BTreeMap<_, _>>()
                        })
                        .filter(|m| !m.is_empty()),
                };
                formatter.json(&output);
            } else {
                // Helper to format key-value pairs with styling
                let format_kv = |key: &str, value: &str| {
                    format!(
                        "{} : {}",
                        formatter.style_key(&format!("{:<9}", key)),
                        value
                    )
                };

                formatter.println(&format_kv("Name", &formatter.style_file(&info.key)));
                if let Some(modified) = info.last_modified {
                    let date_str = modified.strftime("%Y-%m-%d %H:%M:%S UTC").to_string();
                    formatter.println(&format_kv("Date", &formatter.style_date(&date_str)));
                }
                if let Some(size) = info.size_bytes {
                    formatter.println(&format_kv(
                        "Size",
                        &formatter.style_size(&format!("{size} bytes")),
                    ));
                }
                if let Some(human) = &info.size_human {
                    formatter.println(&format_kv("Size", &formatter.style_size(human)));
                }
                if let Some(etag) = &info.etag {
                    formatter.println(&format_kv("ETag", etag));
                }
                formatter.println(&format_kv("Type", "file"));
                if let Some(sc) = &info.storage_class {
                    formatter.println(&format_kv("Class", sc));
                }
                let display_metadata = build_display_metadata(&info);
                if !display_metadata.is_empty() {
                    formatter.println(&format_kv("Metadata", ""));
                    for (key, value) in &display_metadata {
                        formatter.println(&format_kv(key, value));
                    }
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

/// Capitalize the first letter of each hyphen-separated segment
fn capitalize_meta_key(s: &str) -> String {
    s.split('-')
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
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

    #[test]
    fn test_capitalize_meta_key() {
        assert_eq!(capitalize_meta_key("content-type"), "Content-Type");
        assert_eq!(
            capitalize_meta_key("content-disposition"),
            "Content-Disposition"
        );
        assert_eq!(capitalize_meta_key("a"), "A");
        assert_eq!(capitalize_meta_key(""), "");
        assert_eq!(capitalize_meta_key("already"), "Already");
        assert_eq!(capitalize_meta_key("x-custom-key"), "X-Custom-Key");
    }

    #[test]
    fn test_stat_output_serialization_with_metadata() {
        let mut meta = BTreeMap::new();
        meta.insert("content-disposition".to_string(), "attachment".to_string());
        let output = StatOutput {
            name: "file.txt".to_string(),
            last_modified: None,
            size_bytes: Some(1024),
            size_human: Some("1 KiB".to_string()),
            etag: None,
            content_type: None,
            storage_class: None,
            version_id: None,
            metadata: Some(meta),
        };
        let json = serde_json::to_string(&output).expect("should serialize");
        assert!(json.contains("\"metadata\""));
        assert!(json.contains("content-disposition"));
        assert!(json.contains("attachment"));
    }

    #[test]
    fn test_stat_output_serialization_without_metadata() {
        let output = StatOutput {
            name: "file.txt".to_string(),
            last_modified: None,
            size_bytes: Some(1024),
            size_human: Some("1 KiB".to_string()),
            etag: None,
            content_type: None,
            storage_class: None,
            version_id: None,
            metadata: None,
        };
        let json = serde_json::to_string(&output).expect("should serialize");
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn test_stat_output_serialization_empty_metadata_omitted() {
        let output = StatOutput {
            name: "file.txt".to_string(),
            last_modified: None,
            size_bytes: Some(1024),
            size_human: Some("1 KiB".to_string()),
            etag: None,
            content_type: None,
            storage_class: None,
            version_id: None,
            metadata: Some(BTreeMap::new()),
        };
        let json = serde_json::to_string(&output).expect("should serialize");
        // Empty BTreeMap is treated as None via skip_serializing_if helper
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn test_build_display_metadata_includes_content_type() {
        let mut user_meta = std::collections::HashMap::new();
        user_meta.insert("custom-key".to_string(), "custom-value".to_string());

        let info = ObjectInfo {
            key: "file.txt".to_string(),
            size_bytes: Some(1),
            size_human: Some("1 B".to_string()),
            last_modified: None,
            etag: None,
            storage_class: None,
            content_type: Some("text/plain".to_string()),
            metadata: Some(user_meta),
            is_dir: false,
        };

        let display_metadata = build_display_metadata(&info);
        assert_eq!(
            display_metadata.get("Content-Type"),
            Some(&"text/plain".to_string())
        );
        assert_eq!(
            display_metadata.get("X-Amz-Meta-Custom-Key"),
            Some(&"custom-value".to_string())
        );
    }
}
