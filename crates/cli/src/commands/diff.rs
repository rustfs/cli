//! diff command - Compare objects between two locations
//!
//! Shows differences between two S3 paths or between local and remote.

use clap::{Args, ValueEnum};
use rc_core::{AliasManager, ListOptions, ObjectStore as _, ParsedPath, RemotePath, parse_path};
use rc_s3::S3Client;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use super::object_identity::identity_etag_from_metadata;
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// How `rc diff` decides that two objects hold the same data.
///
/// These mirror `rc mirror --compare` so the two commands cannot disagree about
/// whether a pair of objects is already in sync.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum CompareMode {
    /// Same when ETags match, or when sizes match and the second object records
    /// the first object's ETag in `x-amz-meta-rc-source-etag`.
    #[default]
    Auto,
    /// Same only when both ETags are present and identical.
    Etag,
    /// Same when sizes match, ignoring ETag differences.
    Size,
}

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

    /// How to decide that two objects hold the same data
    #[arg(long, value_enum, default_value_t = CompareMode::Auto)]
    pub compare: CompareMode,
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
    /// Full object key, retained so `auto` compare can HeadObject this entry.
    key: String,
    size: Option<i64>,
    modified: Option<String>,
    etag: Option<String>,
    /// Source ETag recorded by a previous `rc mirror` or cross-alias `rc cp`.
    /// ListObjects never returns user metadata, so this is filled by HeadObject.
    identity_etag: Option<String>,
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

    let mut second_objects =
        match list_objects_map(&second_client, &second_path, args.recursive).await {
            Ok(o) => o,
            Err(e) => {
                formatter.error(&format!("Failed to list second path: {e}"));
                return ExitCode::NetworkError;
            }
        };

    enrich_second_identity(
        &second_client,
        &second_path,
        &first_objects,
        &mut second_objects,
        args.compare,
    )
    .await;

    // Compare objects
    let entries = compare_objects(
        &first_objects,
        &second_objects,
        args.diff_only,
        args.compare,
    );

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

            let map_key = if relative_key.is_empty() {
                // Single object case
                Path::new(&item.key)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| item.key.clone())
            } else {
                relative_key
            };
            objects.insert(
                map_key,
                FileInfo {
                    key: item.key,
                    size: item.size_bytes,
                    modified: item.last_modified.map(|t| t.to_string()),
                    etag: item.etag,
                    identity_etag: None,
                },
            );
        }

        if result.truncated {
            continuation_token = result.continuation_token;
        } else {
            break;
        }
    }

    Ok(objects)
}

/// Decide whether the second object already holds the first object's data.
///
/// A client-streamed copy cannot preserve the source ETag, so `auto` also
/// accepts a recorded source identity. This is the same rule `rc mirror` uses to
/// skip a copy, which keeps `diff` from reporting a difference for a pair that
/// `mirror` considers synchronized.
fn objects_match(first: &FileInfo, second: &FileInfo, compare: CompareMode) -> bool {
    let (Some(first_size), Some(second_size)) = (first.size, second.size) else {
        return false;
    };
    if first_size != second_size {
        return false;
    }
    match compare {
        CompareMode::Size => true,
        CompareMode::Etag => first.etag.is_some() && first.etag == second.etag,
        CompareMode::Auto => {
            if first.etag.is_some() && first.etag == second.etag {
                return true;
            }
            first
                .etag
                .as_ref()
                .zip(second.identity_etag.as_ref())
                .is_some_and(|(first_etag, identity_etag)| first_etag == identity_etag)
        }
    }
}

/// Whether HeadObject on the second entry could still prove the pair identical.
///
/// Restricted to same-size pairs whose listed ETags differ, so an unchanged tree
/// costs no extra requests.
fn second_needs_identity_lookup(first: &FileInfo, second: &FileInfo, compare: CompareMode) -> bool {
    if !matches!(compare, CompareMode::Auto) {
        return false;
    }
    if first.size.is_none() || first.size != second.size {
        return false;
    }
    let Some(first_etag) = first.etag.as_ref() else {
        return false;
    };
    if second.etag.as_ref() == Some(first_etag) {
        return false;
    }
    second.identity_etag.is_none()
}

/// Fill recorded source identities for entries that could still match.
///
/// ListObjects omits user metadata, so the identity has to come from HeadObject.
/// A failed lookup leaves the entry unenriched and it is reported as different.
async fn enrich_second_identity(
    client: &S3Client,
    path: &RemotePath,
    first: &HashMap<String, FileInfo>,
    second: &mut HashMap<String, FileInfo>,
    compare: CompareMode,
) {
    let pending: Vec<String> = second
        .iter()
        .filter(|(key, second_info)| {
            first.get(*key).is_some_and(|first_info| {
                second_needs_identity_lookup(first_info, second_info, compare)
            })
        })
        .map(|(key, _)| key.clone())
        .collect();

    for map_key in pending {
        let Some(second_info) = second.get(&map_key) else {
            continue;
        };
        let object_path = RemotePath::new(&path.alias, &path.bucket, &second_info.key);
        if let Ok(info) = client.head_object(&object_path).await
            && let Some(identity_etag) = identity_etag_from_metadata(info.metadata.as_ref())
            && let Some(entry) = second.get_mut(&map_key)
        {
            entry.identity_etag = Some(identity_etag);
        }
    }
}

fn compare_objects(
    first: &HashMap<String, FileInfo>,
    second: &HashMap<String, FileInfo>,
    diff_only: bool,
    compare: CompareMode,
) -> Vec<DiffEntry> {
    let mut entries = Vec::new();

    // Check objects in first
    for (key, first_info) in first {
        if let Some(second_info) = second.get(key) {
            // Object exists in both
            let status = if objects_match(first_info, second_info, compare) {
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

    fn entry(size: i64, etag: Option<&str>) -> FileInfo {
        FileInfo {
            key: "prefix/file.txt".to_string(),
            size: Some(size),
            modified: None,
            etag: etag.map(ToOwned::to_owned),
            identity_etag: None,
        }
    }

    fn entry_with_identity(size: i64, etag: &str, identity_etag: &str) -> FileInfo {
        FileInfo {
            identity_etag: Some(identity_etag.to_string()),
            ..entry(size, Some(etag))
        }
    }

    fn one(key: &str, info: FileInfo) -> HashMap<String, FileInfo> {
        HashMap::from([(key.to_string(), info)])
    }

    #[test]
    fn test_compare_objects_same() {
        let first = one("file.txt", entry(100, Some("abc123")));
        let second = one("file.txt", entry(100, Some("abc123")));

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Same);
    }

    #[test]
    fn test_compare_objects_different() {
        let first = one("file.txt", entry(100, Some("abc123")));
        let second = one("file.txt", entry(200, Some("def456")));

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn test_compare_objects_missing_etag_is_different() {
        let first = one("file.txt", entry(100, None));
        let second = one("file.txt", entry(100, Some("second-etag")));

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);

        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn test_compare_objects_only_first() {
        let first = one("file.txt", entry(100, None));
        let second = HashMap::new();

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::OnlyFirst);
    }

    #[test]
    fn test_compare_objects_only_second() {
        let first = HashMap::new();
        let second = one("file.txt", entry(100, None));

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, DiffStatus::OnlySecond);
    }

    #[test]
    fn auto_compare_treats_a_recorded_source_identity_as_same() {
        let first = one("file.txt", entry(100, Some("source-etag")));
        let second = one(
            "file.txt",
            entry_with_identity(100, "multipart-etag-1", "source-etag"),
        );

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);

        assert_eq!(
            entries[0].status,
            DiffStatus::Same,
            "auto must agree with mirror --compare auto"
        );
    }

    #[test]
    fn etag_compare_ignores_a_recorded_source_identity() {
        let first = one("file.txt", entry(100, Some("source-etag")));
        let second = one(
            "file.txt",
            entry_with_identity(100, "multipart-etag-1", "source-etag"),
        );

        let entries = compare_objects(&first, &second, false, CompareMode::Etag);

        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn size_compare_ignores_etag_differences() {
        let first = one("file.txt", entry(100, Some("source-etag")));
        let second = one("file.txt", entry(100, Some("other-etag")));

        let entries = compare_objects(&first, &second, false, CompareMode::Size);

        assert_eq!(entries[0].status, DiffStatus::Same);
    }

    #[test]
    fn auto_compare_reports_a_mismatched_identity_as_different() {
        let first = one("file.txt", entry(100, Some("source-etag")));
        let second = one(
            "file.txt",
            entry_with_identity(100, "multipart-etag-1", "other-etag"),
        );

        let entries = compare_objects(&first, &second, false, CompareMode::Auto);

        assert_eq!(entries[0].status, DiffStatus::Different);
    }

    #[test]
    fn size_mismatch_is_different_in_every_compare_mode() {
        let first = one("file.txt", entry(100, Some("source-etag")));
        let second = one(
            "file.txt",
            entry_with_identity(200, "source-etag", "source-etag"),
        );

        for compare in [CompareMode::Auto, CompareMode::Etag, CompareMode::Size] {
            let entries = compare_objects(&first, &second, false, compare);
            assert_eq!(
                entries[0].status,
                DiffStatus::Different,
                "{compare:?} must not call different sizes the same"
            );
        }
    }

    #[test]
    fn unknown_sizes_are_never_assumed_equal() {
        let mut missing = entry(100, Some("source-etag"));
        missing.size = None;
        let first = one("file.txt", missing.clone());
        let second = one("file.txt", missing);

        for compare in [CompareMode::Auto, CompareMode::Etag, CompareMode::Size] {
            let entries = compare_objects(&first, &second, false, compare);
            assert_eq!(
                entries[0].status,
                DiffStatus::Different,
                "{compare:?} must not assume equality without sizes"
            );
        }
    }

    #[test]
    fn identity_lookup_is_limited_to_auto_same_size_etag_mismatches() {
        let source = entry(100, Some("source-etag"));
        let mismatched = entry(100, Some("other-etag"));

        assert!(second_needs_identity_lookup(
            &source,
            &mismatched,
            CompareMode::Auto
        ));

        assert!(
            !second_needs_identity_lookup(
                &source,
                &entry(100, Some("source-etag")),
                CompareMode::Auto
            ),
            "matching ETags already prove equality"
        );
        assert!(
            !second_needs_identity_lookup(
                &source,
                &entry_with_identity(100, "other-etag", "source-etag"),
                CompareMode::Auto
            ),
            "an entry that already has an identity needs no lookup"
        );
        assert!(
            !second_needs_identity_lookup(
                &source,
                &entry(200, Some("other-etag")),
                CompareMode::Auto
            ),
            "different sizes can never match"
        );
        assert!(!second_needs_identity_lookup(
            &source,
            &mismatched,
            CompareMode::Etag
        ));
        assert!(!second_needs_identity_lookup(
            &source,
            &mismatched,
            CompareMode::Size
        ));

        let mut unknown_size = source.clone();
        unknown_size.size = None;
        assert!(
            !second_needs_identity_lookup(&unknown_size, &mismatched, CompareMode::Auto),
            "an unknown source size cannot be reconciled by metadata"
        );

        let mut no_etag = source.clone();
        no_etag.etag = None;
        assert!(
            !second_needs_identity_lookup(&no_etag, &mismatched, CompareMode::Auto),
            "without a source ETag there is nothing to match an identity against"
        );
    }

    #[test]
    fn diff_only_hides_matching_entries_in_auto_mode() {
        let first = HashMap::from([
            ("same.txt".to_string(), entry(100, Some("source-etag"))),
            ("changed.txt".to_string(), entry(100, Some("source-etag"))),
        ]);
        let second = HashMap::from([
            (
                "same.txt".to_string(),
                entry_with_identity(100, "multipart-etag-1", "source-etag"),
            ),
            ("changed.txt".to_string(), entry(100, Some("other-etag"))),
        ]);

        let entries = compare_objects(&first, &second, true, CompareMode::Auto);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "changed.txt");
        assert_eq!(entries[0].status, DiffStatus::Different);
    }
}
