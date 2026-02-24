//! tag command - Manage bucket and object tags
//!
//! Get, set, or remove tags on buckets and objects.

use clap::{Args, Subcommand};
use rc_core::{AliasManager, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::HashMap;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Manage bucket and object tags
#[derive(Args, Debug)]
pub struct TagArgs {
    #[command(subcommand)]
    pub command: TagCommands,
}

#[derive(Subcommand, Debug)]
pub enum TagCommands {
    /// List tags for a bucket or object
    List(TagPathArg),

    /// Set tags for a bucket or object
    Set(SetTagArgs),

    /// Remove all tags from a bucket or object
    Remove(TagPathArg),
}

#[derive(Args, Debug)]
pub struct TagPathArg {
    /// Path to a bucket or object (alias/bucket or alias/bucket/key)
    pub path: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct SetTagArgs {
    /// Path to a bucket or object (alias/bucket or alias/bucket/key)
    pub path: String,

    /// Tags to set (key=value format, can specify multiple)
    #[arg(short, long, value_name = "KEY=VALUE", num_args = 1..)]
    pub tags: Vec<String>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct TagOutput {
    path: String,
    tags: HashMap<String, String>,
    count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TagTarget {
    Bucket {
        alias: String,
        bucket: String,
    },
    Object {
        alias: String,
        bucket: String,
        key: String,
    },
}

impl TagTarget {
    fn alias_name(&self) -> &str {
        match self {
            Self::Bucket { alias, .. } | Self::Object { alias, .. } => alias,
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            Self::Bucket { .. } => "bucket",
            Self::Object { .. } => "object",
        }
    }
}

/// Execute the tag command
pub async fn execute(args: TagArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        TagCommands::List(path_arg) => execute_list(path_arg, output_config).await,
        TagCommands::Set(set_args) => execute_set(set_args, output_config).await,
        TagCommands::Remove(path_arg) => execute_remove(path_arg, output_config).await,
    }
}

async fn execute_list(args: TagPathArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let target = match parse_tag_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(target.alias_name(), args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let tags = match get_tags_for_target(&client, &target).await {
        Ok(tags) => tags,
        Err(error) => {
            formatter.error(&format!("Failed to get tags: {error}"));
            return ExitCode::GeneralError;
        }
    };

    if formatter.is_json() {
        formatter.json(&TagOutput {
            path: args.path,
            count: tags.len(),
            tags,
        });
    } else if tags.is_empty() {
        formatter.println("No tags found.");
    } else {
        formatter.println(&format!("Tags for {} '{}':", target.kind_name(), args.path));
        for (key, value) in &tags {
            formatter.println(&format!("  {key}={value}"));
        }
    }

    ExitCode::Success
}

async fn execute_set(args: SetTagArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if args.tags.is_empty() {
        formatter.error("At least one tag is required (--tags key=value)");
        return ExitCode::UsageError;
    }

    let target = match parse_tag_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let tags = match parse_tags(&args.tags) {
        Ok(tags) => tags,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(target.alias_name(), args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match set_tags_for_target(&client, &target, tags.clone()).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&TagOutput {
                    path: args.path,
                    count: tags.len(),
                    tags,
                });
            } else {
                formatter.println(&format!(
                    "Set {} tag(s) on {} '{}'",
                    tags.len(),
                    target.kind_name(),
                    args.path
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set tags: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: TagPathArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let target = match parse_tag_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(target.alias_name(), args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match delete_tags_for_target(&client, &target).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = serde_json::json!({
                    "path": args.path,
                    "status": "removed",
                });
                formatter.json(&output);
            } else {
                formatter.println(&format!(
                    "Removed all tags from {} '{}'",
                    target.kind_name(),
                    args.path
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to remove tags: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn get_tags_for_target(
    client: &S3Client,
    target: &TagTarget,
) -> rc_core::Result<HashMap<String, String>> {
    match target {
        TagTarget::Bucket { bucket, .. } => client.get_bucket_tags(bucket).await,
        TagTarget::Object { alias, bucket, key } => {
            let path = RemotePath::new(alias, bucket, key);
            client.get_object_tags(&path).await
        }
    }
}

async fn set_tags_for_target(
    client: &S3Client,
    target: &TagTarget,
    tags: HashMap<String, String>,
) -> rc_core::Result<()> {
    match target {
        TagTarget::Bucket { bucket, .. } => client.set_bucket_tags(bucket, tags).await,
        TagTarget::Object { alias, bucket, key } => {
            let path = RemotePath::new(alias, bucket, key);
            client.set_object_tags(&path, tags).await
        }
    }
}

async fn delete_tags_for_target(client: &S3Client, target: &TagTarget) -> rc_core::Result<()> {
    match target {
        TagTarget::Bucket { bucket, .. } => client.delete_bucket_tags(bucket).await,
        TagTarget::Object { alias, bucket, key } => {
            let path = RemotePath::new(alias, bucket, key);
            client.delete_object_tags(&path).await
        }
    }
}

fn parse_tags(tags: &[String]) -> Result<HashMap<String, String>, String> {
    let mut parsed = HashMap::new();

    for tag_str in tags {
        match tag_str.split_once('=') {
            Some((key, value)) => {
                if key.is_empty() {
                    return Err(format!(
                        "Invalid tag format: '{tag_str}' (key cannot be empty)"
                    ));
                }
                parsed.insert(key.to_string(), value.to_string());
            }
            None => {
                return Err(format!(
                    "Invalid tag format: '{tag_str}' (expected key=value)"
                ));
            }
        }
    }

    Ok(parsed)
}

async fn setup_client(
    alias_name: &str,
    force: bool,
    formatter: &Formatter,
) -> Result<S3Client, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            formatter.error(&format!("Failed to load aliases: {error}"));
            return Err(ExitCode::GeneralError);
        }
    };

    let alias = match alias_manager.get(alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            return Err(ExitCode::NotFound);
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            formatter.error(&format!("Failed to create S3 client: {error}"));
            return Err(ExitCode::NetworkError);
        }
    };

    if !force {
        match client.capabilities().await {
            Ok(caps) => {
                if !caps.tagging {
                    formatter
                        .error("Backend does not support tagging. Use --force to attempt anyway.");
                    return Err(ExitCode::UnsupportedFeature);
                }
            }
            Err(error) => {
                formatter.error(&format!("Failed to detect capabilities: {error}"));
                return Err(ExitCode::NetworkError);
            }
        }
    }

    Ok(client)
}

fn parse_tag_path(path: &str) -> Result<TagTarget, String> {
    if path.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    if parts.len() < 2 || parts[0].is_empty() {
        return Err("Path must include alias and bucket (alias/bucket)".to_string());
    }

    let bucket = parts[1].trim_end_matches('/');
    if bucket.is_empty() {
        return Err("Bucket name is required (alias/bucket)".to_string());
    }

    if parts.len() == 2 || (parts.len() == 3 && parts[2].is_empty()) {
        return Ok(TagTarget::Bucket {
            alias: parts[0].to_string(),
            bucket: bucket.to_string(),
        });
    }

    Ok(TagTarget::Object {
        alias: parts[0].to_string(),
        bucket: bucket.to_string(),
        key: parts[2].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tag_path_bucket() {
        let target = parse_tag_path("myalias/mybucket").unwrap();
        assert_eq!(
            target,
            TagTarget::Bucket {
                alias: "myalias".to_string(),
                bucket: "mybucket".to_string(),
            }
        );

        let target = parse_tag_path("myalias/mybucket/").unwrap();
        assert_eq!(
            target,
            TagTarget::Bucket {
                alias: "myalias".to_string(),
                bucket: "mybucket".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_tag_path_object() {
        let target = parse_tag_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(
            target,
            TagTarget::Object {
                alias: "myalias".to_string(),
                bucket: "mybucket".to_string(),
                key: "path/to/file.txt".to_string(),
            }
        );
    }

    #[test]
    fn test_parse_tag_path_errors() {
        assert!(parse_tag_path("").is_err());
        assert!(parse_tag_path("myalias").is_err());
        assert!(parse_tag_path("/mybucket").is_err());
        assert!(parse_tag_path("myalias/").is_err());
    }

    #[test]
    fn test_parse_tags() {
        let tags = parse_tags(&["env=prod".to_string(), "team=infra".to_string()]).unwrap();
        assert_eq!(tags.get("env"), Some(&"prod".to_string()));
        assert_eq!(tags.get("team"), Some(&"infra".to_string()));
    }

    #[test]
    fn test_parse_tags_errors() {
        assert!(parse_tags(&["invalid".to_string()]).is_err());
        assert!(parse_tags(&["=value".to_string()]).is_err());
    }
}
