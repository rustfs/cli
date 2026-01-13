//! ls command - List buckets and objects
//!
//! Lists buckets when given an alias only, or lists objects when given a bucket path.

use clap::Args;
use rc_core::{AliasManager, ListOptions, ObjectInfo, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// List buckets or objects
#[derive(Args, Debug)]
pub struct LsArgs {
    /// Remote path (alias/ or alias/bucket[/prefix])
    pub path: String,

    /// List recursively
    #[arg(short, long)]
    pub recursive: bool,

    /// Show versions (requires versioning support)
    #[arg(long)]
    pub versions: bool,

    /// Include incomplete uploads
    #[arg(long)]
    pub incomplete: bool,

    /// Summarize output (show totals only)
    #[arg(long)]
    pub summarize: bool,
}

/// Output structure for ls command (JSON format)
#[derive(Debug, Serialize)]
struct LsOutput {
    items: Vec<ObjectInfo>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<Summary>,
}

#[derive(Debug, Serialize)]
struct Summary {
    total_objects: usize,
    total_size_bytes: i64,
    total_size_human: String,
}

/// Execute the ls command
pub async fn execute(args: LsArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse the path
    let (alias_name, bucket, prefix) = match parse_ls_path(&args.path) {
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

    // If no bucket specified, list buckets
    if bucket.is_none() {
        return list_buckets(&client, &formatter, args.summarize).await;
    }

    let bucket = bucket.unwrap();
    let path = RemotePath::new(&alias_name, &bucket, prefix.unwrap_or_default());

    // List objects
    list_objects(&client, &path, &args, &formatter).await
}

async fn list_buckets(client: &S3Client, formatter: &Formatter, summarize: bool) -> ExitCode {
    match client.list_buckets().await {
        Ok(buckets) => {
            if formatter.is_json() {
                let output = LsOutput {
                    items: buckets.clone(),
                    truncated: false,
                    continuation_token: None,
                    summary: if summarize {
                        Some(Summary {
                            total_objects: buckets.len(),
                            total_size_bytes: 0,
                            total_size_human: "0 B".to_string(),
                        })
                    } else {
                        None
                    },
                };
                formatter.json(&output);
            } else {
                for bucket in &buckets {
                    let date = bucket
                        .last_modified
                        .map(|d| d.strftime("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "                   ".to_string());
                    formatter.println(&format!("[{date}]     0B {}/", bucket.key));
                }
                if summarize {
                    formatter.println(&format!("\nTotal: {} buckets", buckets.len()));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to list buckets: {e}"));
            ExitCode::NetworkError
        }
    }
}

async fn list_objects(
    client: &S3Client,
    path: &RemotePath,
    args: &LsArgs,
    formatter: &Formatter,
) -> ExitCode {
    let options = ListOptions {
        recursive: args.recursive,
        max_keys: Some(1000),
        ..Default::default()
    };

    let mut all_items = Vec::new();
    let mut continuation_token: Option<String> = None;
    let mut is_truncated;

    // Paginate through all results
    loop {
        let opts = ListOptions {
            continuation_token: continuation_token.clone(),
            ..options.clone()
        };

        match client.list_objects(path, opts).await {
            Ok(result) => {
                all_items.extend(result.items);
                is_truncated = result.truncated;
                continuation_token = result.continuation_token.clone();

                if !result.truncated {
                    break;
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchBucket") {
                    formatter.error(&format!("Bucket not found: {}", path.bucket));
                    return ExitCode::NotFound;
                }
                formatter.error(&format!("Failed to list objects: {e}"));
                return ExitCode::NetworkError;
            }
        }
    }

    // Calculate summary
    let total_objects = all_items.iter().filter(|i| !i.is_dir).count();
    let total_size: i64 = all_items.iter().filter_map(|i| i.size_bytes).sum();

    if formatter.is_json() {
        let output = LsOutput {
            items: all_items,
            truncated: is_truncated,
            continuation_token,
            summary: if args.summarize {
                Some(Summary {
                    total_objects,
                    total_size_bytes: total_size,
                    total_size_human: humansize::format_size(total_size as u64, humansize::BINARY),
                })
            } else {
                None
            },
        };
        formatter.json(&output);
    } else {
        for item in &all_items {
            let date = item
                .last_modified
                .map(|d| d.strftime("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "                   ".to_string());

            if item.is_dir {
                formatter.println(&format!("[{date}]     0B {}", item.key));
            } else {
                let size = item.size_human.clone().unwrap_or_else(|| "0 B".to_string());
                formatter.println(&format!("[{date}] {:>6} {}", size, item.key));
            }
        }

        if args.summarize {
            formatter.println(&format!(
                "\nTotal: {} objects, {}",
                total_objects,
                humansize::format_size(total_size as u64, humansize::BINARY)
            ));
        }
    }

    ExitCode::Success
}

/// Parse ls path into (alias, bucket, prefix)
fn parse_ls_path(path: &str) -> Result<(String, Option<String>, Option<String>), String> {
    let path = path.trim_end_matches('/');

    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    match parts.len() {
        1 => Ok((parts[0].to_string(), None, None)),
        2 => Ok((parts[0].to_string(), Some(parts[1].to_string()), None)),
        3 => Ok((
            parts[0].to_string(),
            Some(parts[1].to_string()),
            Some(format!("{}/", parts[2])),
        )),
        _ => Err(format!("Invalid path format: {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ls_path_alias_only() {
        let (alias, bucket, prefix) = parse_ls_path("myalias").unwrap();
        assert_eq!(alias, "myalias");
        assert!(bucket.is_none());
        assert!(prefix.is_none());
    }

    #[test]
    fn test_parse_ls_path_alias_bucket() {
        let (alias, bucket, prefix) = parse_ls_path("myalias/mybucket").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, Some("mybucket".to_string()));
        assert!(prefix.is_none());
    }

    #[test]
    fn test_parse_ls_path_with_prefix() {
        let (alias, bucket, prefix) = parse_ls_path("myalias/mybucket/path/to").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, Some("mybucket".to_string()));
        assert_eq!(prefix, Some("path/to/".to_string()));
    }

    #[test]
    fn test_parse_ls_path_trailing_slash() {
        let (alias, bucket, prefix) = parse_ls_path("myalias/mybucket/").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, Some("mybucket".to_string()));
        assert!(prefix.is_none());
    }

    #[test]
    fn test_parse_ls_path_empty() {
        assert!(parse_ls_path("").is_err());
    }
}
