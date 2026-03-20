//! ls command - List buckets and objects
//!
//! Lists buckets when given an alias only, or lists objects when given a bucket path.

use clap::Args;
use rc_core::{AliasManager, ListOptions, ObjectInfo, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::HashMap;

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

    if bucket.is_none() {
        if args.recursive {
            // If no bucket specified & recursive flag is provided, list all objects on the server
            return list_all_objects(&client, alias_name, &formatter, args.summarize).await;
        } else {
            // Else no bucket specified, list buckets
            return list_buckets(&client, &formatter, args.summarize).await;
        }
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
                    let styled_date = formatter.style_date(&format!("[{date}]"));
                    let styled_size = formatter.style_size(&format!("{:>10}", "0B"));
                    let styled_name = formatter.style_dir(&format!("{}/", bucket.key));
                    formatter.println(&format!("{styled_date} {styled_size} {styled_name}"));
                }
                if summarize {
                    formatter.println(&format!(
                        "\nTotal: {} buckets",
                        formatter.style_size(&buckets.len().to_string())
                    ));
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

    let (all_items, is_truncated, continuation_token) =
        match list_objects_with_paging(client, path, &options).await {
            Ok(r) => r,
            Err((message, exit_code)) => {
                formatter.error(&message);
                return exit_code;
            }
        };

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
            let styled_date = formatter.style_date(&format!("[{date}]"));

            if item.is_dir {
                let styled_size = formatter.style_size(&format!("{:>10}", "0B"));
                let styled_name = formatter.style_dir(&item.key);
                formatter.println(&format!("{styled_date} {styled_size} {styled_name}"));
            } else {
                let size = item.size_human.clone().unwrap_or_else(|| "0 B".to_string());
                let styled_size = formatter.style_size(&format!("{:>10}", size));
                let styled_name = formatter.style_file(&item.key);
                formatter.println(&format!("{styled_date} {styled_size} {styled_name}"));
            }
        }

        if args.summarize {
            let total_size_human = humansize::format_size(total_size as u64, humansize::BINARY);
            formatter.println(&format!(
                "\nTotal: {} objects, {}",
                formatter.style_size(&total_objects.to_string()),
                formatter.style_size(&total_size_human)
            ));
        }
    }

    ExitCode::Success
}

async fn list_all_objects(
    client: &S3Client,
    alias: String,
    formatter: &Formatter,
    summarize: bool,
) -> ExitCode {
    let buckets = match client.list_buckets().await {
        Ok(buckets) => buckets,
        Err(e) => {
            formatter.error(&format!("Failed to list buckets: {e}"));
            return ExitCode::NetworkError;
        }
    };

    let options = ListOptions {
        recursive: true,
        max_keys: Some(1000),
        ..Default::default()
    };

    let mut all_items: HashMap<&str, Vec<ObjectInfo>> = HashMap::new();
    let mut is_truncated = false;
    let mut continuation_token: Option<String> = None;

    // List all objects in each bucket
    for bucket in &buckets {
        let path = &RemotePath::new(&alias, &bucket.key, "");
        let new_items: Vec<ObjectInfo>;

        (new_items, is_truncated, continuation_token) =
            match list_objects_with_paging(client, path, &options).await {
                Ok(r) => r,
                Err((message, exit_code)) => {
                    formatter.error(&message);
                    return exit_code;
                }
            };

        all_items.entry(&bucket.key).or_default().extend(new_items);
    }

    // Calculate summary
    let total_objects = all_items.values().flatten().filter(|i| !i.is_dir).count();
    let total_size = all_items
        .values()
        .flatten()
        .filter_map(|i| i.size_bytes)
        .sum();

    if formatter.is_json() {
        let output = LsOutput {
            items: all_items
                .into_iter()
                .flat_map(|(bucket, objects)| {
                    objects.into_iter().map(move |mut obj| {
                        obj.key = format!("{}/{}", bucket, obj.key);
                        obj
                    })
                })
                .collect(),
            truncated: is_truncated,
            continuation_token,
            summary: if summarize {
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
        // Ensure deterministic bucket order in text output
        let mut bucket_names: Vec<&str> = all_items.keys().copied().collect();
        bucket_names.sort_unstable();

        for bucket_name in bucket_names {
            if let Some(objects) = all_items.get(bucket_name) {
                for item in objects {
                    let date = item
                        .last_modified
                        .map(|d| d.strftime("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "                   ".to_string());
                    let styled_date = formatter.style_date(&format!("[{date}]"));

                    if item.is_dir {
                        let styled_size = formatter.style_size(&format!("{:>10}", "0B"));
                        let styled_name =
                            formatter.style_dir(&format!("{}/{}", bucket_name, &item.key));
                        formatter.println(&format!("{styled_date} {styled_size} {styled_name}"));
                    } else {
                        let size = item.size_human.clone().unwrap_or_else(|| "0 B".to_string());
                        let styled_size = formatter.style_size(&format!("{:>10}", size));
                        let styled_name =
                            formatter.style_file(&format!("{}/{}", bucket_name, &item.key));
                        formatter.println(&format!("{styled_date} {styled_size} {styled_name}"));
                    }
                }
            }
        }

        if summarize {
            let total_size_human = humansize::format_size(total_size as u64, humansize::BINARY);
            formatter.println(&format!(
                "\nTotal: {} objects, {}",
                formatter.style_size(&total_objects.to_string()),
                formatter.style_size(&total_size_human)
            ));
        }
    }

    ExitCode::Success
}

// List objects using pagination, returns (Vec<ObjectInfo>, is_truncated, Option<continuation_token>)
async fn list_objects_with_paging(
    client: &S3Client,
    path: &RemotePath,
    options: &ListOptions,
) -> Result<(Vec<ObjectInfo>, bool, Option<String>), (String, ExitCode)> {
    let mut all_items = Vec::new();
    let mut is_truncated;
    let mut continuation_token = None;

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
                    return Err((
                        format!("Bucket not found: {}", path.bucket),
                        ExitCode::NotFound,
                    ));
                }
                return Err((
                    format!("Failed to list objects: {e}"),
                    ExitCode::NetworkError,
                ));
            }
        }
    }

    Ok((all_items, is_truncated, continuation_token))
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
