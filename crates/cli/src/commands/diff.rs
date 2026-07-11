//! diff command - Compare objects between two locations
//!
//! Shows differences between two S3 paths or between local and remote.

use clap::Args;
use rc_core::{AliasManager, ListOptions, ObjectStore as _, ParsedPath, RemotePath, parse_path};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Compare objects between two locations
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// First path (alias/bucket/prefix or local path)
    pub first: String,

    /// Second path (alias/bucket/prefix or local path)
    pub second: String,

    /// Recursive comparison
    #[arg(short, long)]
    pub recursive: bool,

    /// Show only differences (default: show all)
    #[arg(long)]
    pub diff_only: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiffEntry {
    pub key: String,
    pub status: DiffStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub second_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub second_modified: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Same,
    Different,
    OnlyFirst,
    OnlySecond,
}

#[derive(Debug, Serialize)]
struct DiffOutput {
    first: String,
    second: String,
    entries: Vec<DiffEntry>,
    summary: DiffSummary,
}

#[derive(Debug, Serialize)]
struct DiffSummary {
    same: usize,
    different: usize,
    only_first: usize,
    only_second: usize,
    total: usize,
}

#[derive(Debug, Clone)]
struct FileInfo {
    size: Option<i64>,
    modified: Option<String>,
    etag: Option<String>,
}

/// Execute the diff command
pub async fn execute(args: DiffArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse both paths
    let first_parsed = parse_path(&args.first);
    let second_parsed = parse_path(&args.second);

    // Both must be remote for now (local support can be added later)
    let (first_path, second_path) = match (&first_parsed, &second_parsed) {
        (Ok(ParsedPath::Remote(f)), Ok(ParsedPath::Remote(s))) => (f.clone(), s.clone()),
        (Ok(ParsedPath::Local(_)), _) | (_, Ok(ParsedPath::Local(_))) => {
            formatter.error("Local paths are not yet supported in diff command");
            return ExitCode::UsageError;
        }
        (Err(e), _) => {
            formatter.error(&format!("Invalid first path: {e}"));
            return ExitCode::UsageError;
        }
        (_, Err(e)) => {
            formatter.error(&format!("Invalid second path: {e}"));
            return ExitCode::UsageError;
        }
    };

    // Load aliases
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    // Create clients for both paths
    let first_alias = match alias_manager.get(&first_path.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", first_path.alias));
            return ExitCode::NotFound;
        }
    };

    let second_alias = match alias_manager.get(&second_path.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", second_path.alias));
            return ExitCode::NotFound;
        }
    };

    let first_client = match S3Client::new(first_alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create client for first path: {e}"));
            return ExitCode::NetworkError;
        }
    };

    let second_client = match S3Client::new(second_alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create client for second path: {e}"));
            return ExitCode::NetworkError;
        }
    };

    // List objects from both paths
    let first_objects = match list_objects_map(&first_client, &first_path, args.recursive).await {
        Ok(o) => o,
        Err(e) => {
            formatter.error(&format!("Failed to list first path: {e}"));
            return ExitCode::NetworkError;
        }
    };

    let second_objects = match list_objects_map(&second_client, &second_path, args.recursive).await
    {
        Ok(o) => o,
        Err(e) => {
            formatter.error(&format!("Failed to list second path: {e}"));
            return ExitCode::NetworkError;
        }
    };

    // Compare objects
    let entries = compare_objects(&first_objects, &second_objects, args.diff_only);

    // Calculate summary
    let mut summary = DiffSummary {
        same: 0,
        different: 0,
        only_first: 0,
        only_second: 0,
        total: entries.len(),
    };

    for entry in &entries {
        match entry.status {
            DiffStatus::Same => summary.same += 1,
            DiffStatus::Different => summary.different += 1,
            DiffStatus::OnlyFirst => summary.only_first += 1,
            DiffStatus::OnlySecond => summary.only_second += 1,
        }
    }

    // Determine exit code before moving summary
    let has_differences =
        summary.different > 0 || summary.only_first > 0 || summary.only_second > 0;

    if formatter.is_json() {
        let output = DiffOutput {
            first: args.first.clone(),
            second: args.second.clone(),
            entries,
            summary,
        };
        formatter.json(&output);
    } else {
        // Print diff entries
        for entry in &entries {
            let status_char = match entry.status {
                DiffStatus::Same => "=",
                DiffStatus::Different => "≠",
                DiffStatus::OnlyFirst => "<",
                DiffStatus::OnlySecond => ">",
            };

            let size_info = match entry.status {
                DiffStatus::Same => entry.first_size.map(format_size).unwrap_or_default(),
                DiffStatus::Different => {
                    let first = entry.first_size.map(format_size).unwrap_or_default();
                    let second = entry.second_size.map(format_size).unwrap_or_default();
                    format!("{first} → {second}")
                }
                DiffStatus::OnlyFirst => entry.first_size.map(format_size).unwrap_or_default(),
                DiffStatus::OnlySecond => entry.second_size.map(format_size).unwrap_or_default(),
            };

            formatter.println(&format!(
                "{status_char} {:<50} {size_info}",
                formatter.sanitize_text(&entry.key)
            ));
        }

        // Print summary
        formatter.println("");
        formatter.println(&format!(
            "Summary: {} same, {} different, {} only in first, {} only in second",
            summary.same, summary.different, summary.only_first, summary.only_second
        ));
    }

    // Return appropriate exit code
    if has_differences {
        ExitCode::GeneralError // Indicates differences found
    } else {
        ExitCode::Success
    }
}

async fn list_objects_map(
    client: &S3Client,
    path: &RemotePath,
    recursive: bool,
) -> Result<HashMap<String, FileInfo>, rc_core::Error> {
    let mut objects = HashMap::new();
    let mut continuation_token: Option<String> = None;
    let base_prefix = &path.key;

    loop {
        let options = ListOptions {
            recursive,
            max_keys: Some(1000),
            continuation_token: continuation_token.clone(),
            ..Default::default()
        };

        let result = client.list_objects(path, options).await?;

        for item in result.items {
            if item.is_dir {
                continue;
            }

            // Get relative key (remove base prefix)
            let relative_key = item.key.strip_prefix(base_prefix).unwrap_or(&item.key);
            let relative_key = relative_key.trim_start_matches('/').to_string();

            if relative_key.is_empty() {
                // Single object case
                let filename = Path::new(&item.key)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or(item.key.clone());
                objects.insert(
                    filename,
                    FileInfo {
                        size: item.size_bytes,
                        modified: item.last_modified.map(|t| t.to_string()),
                        etag: item.etag,
                    },
                );
            } else {
                objects.insert(
                    relative_key,
                    FileInfo {
                        size: item.size_bytes,
                        modified: item.last_modified.map(|t| t.to_string()),
                        etag: item.etag,
                    },
                );
            }
        }

        if result.truncated {
            continuation_token = result.continuation_token;
        } else {
            break;
        }
    }

    Ok(objects)
}

fn compare_objects(
    first: &HashMap<String, FileInfo>,
    second: &HashMap<String, FileInfo>,
    diff_only: bool,
) -> Vec<DiffEntry> {
    let mut entries = Vec::new();

    // Check objects in first
    for (key, first_info) in first {
        if let Some(second_info) = second.get(key) {
            // Object exists in both
            let is_same = first_info.size == second_info.size
                && matches!(
                    (&first_info.etag, &second_info.etag),
                    (Some(first_etag), Some(second_etag)) if first_etag == second_etag
                );

            let status = if is_same {
                DiffStatus::Same
            } else {
                DiffStatus::Different
            };

            if !diff_only || status != DiffStatus::Same {
                entries.push(DiffEntry {
                    key: key.clone(),
                    status,
                    first_size: first_info.size,
                    second_size: second_info.size,
                    first_modified: first_info.modified.clone(),
                    second_modified: second_info.modified.clone(),
                });
            }
        } else {
            // Only in first
            entries.push(DiffEntry {
                key: key.clone(),
                status: DiffStatus::OnlyFirst,
                first_size: first_info.size,
                second_size: None,
                first_modified: first_info.modified.clone(),
                second_modified: None,
            });
        }
    }

    // Check objects only in second
    for (key, second_info) in second {
        if !first.contains_key(key) {
            entries.push(DiffEntry {
                key: key.clone(),
                status: DiffStatus::OnlySecond,
                first_size: None,
                second_size: second_info.size,
                first_modified: None,
                second_modified: second_info.modified.clone(),
            });
        }
    }

    // Sort by key
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    entries
}

fn format_size(size: i64) -> String {
    humansize::format_size(size as u64, humansize::BINARY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_objects_same() {
        let mut first = HashMap::new();
        first.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc123".to_string()),
            },
        );

        let mut second = HashMap::new();
        second.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc123".to_string()),
            },
        );

        let entries = compare_objects(&first, &second, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Same);
    }

    #[test]
    fn test_compare_objects_different() {
        let mut first = HashMap::new();
        first.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("abc123".to_string()),
            },
        );

        let mut second = HashMap::new();
        second.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(200),
                modified: None,
                etag: Some("def456".to_string()),
            },
        );

        let entries = compare_objects(&first, &second, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn test_compare_objects_missing_etag_is_different() {
        let first = HashMap::from([(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: None,
            },
        )]);
        let second = HashMap::from([(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: Some("second-etag".to_string()),
            },
        )]);

        let entries = compare_objects(&first, &second, false);

        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn test_compare_objects_only_first() {
        let mut first = HashMap::new();
        first.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: None,
            },
        );

        let second = HashMap::new();

        let entries = compare_objects(&first, &second, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::OnlyFirst);
    }

    #[test]
    fn test_compare_objects_only_second() {
        let first = HashMap::new();

        let mut second = HashMap::new();
        second.insert(
            "file.txt".to_string(),
            FileInfo {
                size: Some(100),
                modified: None,
                etag: None,
            },
        );

        let entries = compare_objects(&first, &second, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::OnlySecond);
    }
}
