//! find command - Find objects matching criteria
//!
//! Searches for objects matching specified patterns and filters.

use clap::Args;
use rc_core::{AliasManager, ListOptions, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;
use std::io::Write as _;
use std::process::Command;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Find objects matching criteria
#[derive(Args, Debug)]
pub struct FindArgs {
    /// Path to search (alias/bucket[/prefix])
    pub path: String,

    /// Pattern to match object names (glob-style: *, ?)
    #[arg(long)]
    pub name: Option<String>,

    /// Match objects larger than size (e.g., 1K, 1M, 1G)
    #[arg(long)]
    pub larger: Option<String>,

    /// Match objects smaller than size (e.g., 1K, 1M, 1G)
    #[arg(long)]
    pub smaller: Option<String>,

    /// Match objects newer than duration (e.g., 1h, 1d, 7d)
    #[arg(long)]
    pub newer: Option<String>,

    /// Match objects older than duration (e.g., 1h, 1d, 7d)
    #[arg(long)]
    pub older: Option<String>,

    /// Maximum depth to search (0 = unlimited)
    #[arg(long, default_value = "0")]
    pub maxdepth: usize,

    /// Print only count of matches
    #[arg(long)]
    pub count: bool,

    /// Execute command for each match (use {} as placeholder)
    #[arg(long)]
    pub exec: Option<String>,

    /// Print full path (default: relative to search path)
    #[arg(long)]
    pub print: bool,
}

#[derive(Debug, Serialize)]
struct FindOutput {
    matches: Vec<MatchInfo>,
    total_count: usize,
    total_size_bytes: i64,
    total_size_human: String,
}

#[derive(Debug, Serialize)]
struct MatchInfo {
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_human: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
}

/// Execute the find command
pub async fn execute(args: FindArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse path
    let (alias_name, bucket, prefix) = match parse_find_path(&args.path) {
        Ok(p) => p,
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

    // Build filters
    let filters = match build_filters(&args) {
        Ok(f) => f,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    // Search for objects
    let remote_path = RemotePath::new(&alias_name, &bucket, prefix.as_deref().unwrap_or(""));
    let matches = match find_objects(&client, &remote_path, &filters, args.maxdepth).await {
        Ok(m) => m,
        Err(e) => {
            formatter.error(&format!("Search failed: {e}"));
            return ExitCode::NetworkError;
        }
    };

    // Execute command for each match if requested.
    // This mode is human-output only because command output cannot be embedded in JSON.
    if let Some(exec_template) = args.exec.as_deref() {
        if formatter.is_json() {
            formatter.error("--exec cannot be used with --json output");
            return ExitCode::UsageError;
        }

        for m in &matches {
            let object_path = full_object_path(&alias_name, &bucket, &m.key);
            let command_text = exec_template.replace("{}", &object_path);
            let output = match run_shell_command(&command_text) {
                Ok(output) => output,
                Err(e) => {
                    formatter.error(&format!("Failed to run command for {}: {}", object_path, e));
                    return ExitCode::GeneralError;
                }
            };

            if std::io::stdout().write_all(&output.stdout).is_err() {
                formatter.error("Failed to write command stdout");
                return ExitCode::GeneralError;
            }
            if std::io::stderr().write_all(&output.stderr).is_err() {
                formatter.error("Failed to write command stderr");
                return ExitCode::GeneralError;
            }

            if !output.status.success() {
                formatter.error(&format!(
                    "Command failed for {}: {}",
                    object_path, command_text
                ));
                return ExitCode::GeneralError;
            }
        }
    }

    let display_matches: Vec<MatchInfo> = matches
        .iter()
        .map(|m| MatchInfo {
            key: if args.print {
                full_object_path(&alias_name, &bucket, &m.key)
            } else {
                m.key.clone()
            },
            size_bytes: m.size_bytes,
            size_human: m.size_human.clone(),
            last_modified: m.last_modified.clone(),
        })
        .collect();

    // Calculate totals
    let total_count = matches.len();
    let total_size: i64 = matches.iter().filter_map(|m| m.size_bytes).sum();

    if args.count {
        // Only print count
        if formatter.is_json() {
            let output = serde_json::json!({
                "count": total_count,
                "total_size_bytes": total_size,
                "total_size_human": humansize::format_size(total_size as u64, humansize::BINARY)
            });
            formatter.json(&output);
        } else {
            let total_size_human = humansize::format_size(total_size as u64, humansize::BINARY);
            formatter.println(&format!(
                "Found {} object(s), total size: {}",
                formatter.style_size(&total_count.to_string()),
                formatter.style_size(&total_size_human)
            ));
        }
    } else if formatter.is_json() {
        let output = FindOutput {
            matches: display_matches,
            total_count,
            total_size_bytes: total_size,
            total_size_human: humansize::format_size(total_size as u64, humansize::BINARY),
        };
        formatter.json(&output);
    } else if display_matches.is_empty() {
        formatter.println("No matches found.");
    } else {
        for m in &display_matches {
            let size = m.size_human.as_deref().unwrap_or("0B");
            let styled_size = formatter.style_size(&format!("{:>10}", size));
            let styled_key = formatter.style_file(&m.key);
            formatter.println(&format!("{styled_size} {styled_key}"));
        }
        let total_size_human = humansize::format_size(total_size as u64, humansize::BINARY);
        formatter.println(&format!(
            "\nTotal: {} object(s), {}",
            formatter.style_size(&total_count.to_string()),
            formatter.style_size(&total_size_human)
        ));
    }

    ExitCode::Success
}

fn full_object_path(alias: &str, bucket: &str, key: &str) -> String {
    format!("{alias}/{bucket}/{key}")
}

fn run_shell_command(command: &str) -> std::io::Result<std::process::Output> {
    #[cfg(target_family = "windows")]
    {
        Command::new("cmd").args(["/C", command]).output()
    }
    #[cfg(not(target_family = "windows"))]
    {
        Command::new("sh").args(["-c", command]).output()
    }
}

/// Filters for find command
struct FindFilters {
    name_pattern: Option<glob::Pattern>,
    min_size: Option<i64>,
    max_size: Option<i64>,
    newer_than: Option<jiff::Timestamp>,
    older_than: Option<jiff::Timestamp>,
}

fn build_filters(args: &FindArgs) -> Result<FindFilters, String> {
    // Parse name pattern
    let name_pattern = if let Some(ref pattern) = args.name {
        Some(glob::Pattern::new(pattern).map_err(|e| format!("Invalid name pattern: {e}"))?)
    } else {
        None
    };

    // Parse size filters
    let min_size = args.larger.as_ref().map(|s| parse_size(s)).transpose()?;
    let max_size = args.smaller.as_ref().map(|s| parse_size(s)).transpose()?;

    // Parse time filters
    let now = jiff::Timestamp::now();
    let newer_than = args
        .newer
        .as_ref()
        .map(|d| parse_duration_ago(d, now))
        .transpose()?;
    let older_than = args
        .older
        .as_ref()
        .map(|d| parse_duration_ago(d, now))
        .transpose()?;

    Ok(FindFilters {
        name_pattern,
        min_size,
        max_size,
        newer_than,
        older_than,
    })
}

/// Parse size string (e.g., "1K", "10M", "1G", "10KB")
fn parse_size(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Size cannot be empty".to_string());
    }

    // Find where the numeric part ends
    let suffix_start = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());

    let num_str = &s[..suffix_start];
    let suffix = &s[suffix_start..];

    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("Invalid size number: {num_str}"))?;

    let multiplier = match suffix.to_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        "T" | "TB" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("Unknown size suffix: {suffix}")),
    };

    Ok(num * multiplier)
}

/// Parse duration string and return timestamp that far in the past
fn parse_duration_ago(s: &str, now: jiff::Timestamp) -> Result<jiff::Timestamp, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Duration cannot be empty".to_string());
    }

    let (num_str, suffix) = if s.ends_with(|c: char| c.is_ascii_alphabetic()) {
        let idx = s.len() - 1;
        (&s[..idx], &s[idx..])
    } else {
        (s, "s") // Default to seconds
    };

    let num: i64 = num_str
        .parse()
        .map_err(|_| format!("Invalid duration number: {num_str}"))?;

    let seconds = match suffix.to_lowercase().as_str() {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86400,
        "w" => num * 604800,
        _ => return Err(format!("Unknown duration suffix: {suffix}")),
    };

    let duration = jiff::Span::new().seconds(seconds);
    now.checked_sub(duration)
        .map_err(|e| format!("Duration overflow: {e}"))
}

async fn find_objects(
    client: &S3Client,
    path: &RemotePath,
    filters: &FindFilters,
    maxdepth: usize,
) -> Result<Vec<MatchInfo>, rc_core::Error> {
    let mut matches = Vec::new();
    let mut continuation_token: Option<String> = None;
    let base_prefix = &path.key;
    let base_depth = base_prefix.matches('/').count();

    loop {
        let options = ListOptions {
            recursive: true,
            max_keys: Some(1000),
            continuation_token: continuation_token.clone(),
            ..Default::default()
        };

        let result = client.list_objects(path, options).await?;

        for item in result.items {
            // Skip directories
            if item.is_dir {
                continue;
            }

            // Check depth
            if maxdepth > 0 {
                let item_depth = item.key.matches('/').count();
                if item_depth - base_depth > maxdepth {
                    continue;
                }
            }

            // Check name pattern
            if let Some(ref pattern) = filters.name_pattern {
                let filename = item.key.rsplit('/').next().unwrap_or(&item.key);
                if !pattern.matches(filename) {
                    continue;
                }
            }

            // Check size filters
            if let Some(size) = item.size_bytes {
                if let Some(min) = filters.min_size
                    && size < min
                {
                    continue;
                }
                if let Some(max) = filters.max_size
                    && size > max
                {
                    continue;
                }
            }

            // Check time filters
            if let Some(modified) = item.last_modified {
                if let Some(ref newer) = filters.newer_than
                    && modified < *newer
                {
                    continue;
                }
                if let Some(ref older) = filters.older_than
                    && modified > *older
                {
                    continue;
                }
            }

            // Match found
            matches.push(MatchInfo {
                key: item.key,
                size_bytes: item.size_bytes,
                size_human: item.size_human,
                last_modified: item.last_modified.map(|t| t.to_string()),
            });
        }

        if result.truncated {
            continuation_token = result.continuation_token;
        } else {
            break;
        }
    }

    Ok(matches)
}

/// Parse find path into (alias, bucket, prefix)
fn parse_find_path(path: &str) -> Result<(String, String, Option<String>), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    match parts.len() {
        1 => Err("Bucket name is required".to_string()),
        2 => Ok((parts[0].to_string(), parts[1].to_string(), None)),
        3 => Ok((
            parts[0].to_string(),
            parts[1].to_string(),
            Some(parts[2].to_string()),
        )),
        _ => Err(format!("Invalid path format: '{path}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("10KB").unwrap(), 10 * 1024);
    }

    #[test]
    fn test_parse_size_invalid() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("1X").is_err());
    }

    #[test]
    fn test_parse_find_path() {
        let (alias, bucket, prefix) = parse_find_path("myalias/mybucket").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert!(prefix.is_none());

        let (alias, bucket, prefix) = parse_find_path("myalias/mybucket/path/to").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(prefix, Some("path/to".to_string()));
    }

    #[test]
    fn test_parse_find_path_errors() {
        assert!(parse_find_path("").is_err());
        assert!(parse_find_path("myalias").is_err());
    }

    #[test]
    fn test_full_object_path() {
        assert_eq!(
            full_object_path("test", "bucket", "a/b.txt"),
            "test/bucket/a/b.txt"
        );
    }
}
