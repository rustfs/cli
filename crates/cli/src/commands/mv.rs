//! mv command - Move objects
//!
//! Moves objects between locations (copy + delete).

use clap::Args;
use rc_core::{
    AliasManager, CopyObjectOptions, DeleteRequestOptions, ListOptions, ObjectEncryptionRequest,
    ObjectInfo, ObjectStore as _, ParsedPath, RemotePath, parse_path,
};
use rc_s3::S3Client;
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::cp;
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Move objects
#[derive(Args, Debug)]
pub struct MvArgs {
    /// Source path (local path or alias/bucket/key)
    pub source: String,

    /// Destination path (local path or alias/bucket/key)
    pub target: String,

    /// Move recursively
    #[arg(short, long)]
    pub recursive: bool,

    /// Continue on errors
    #[arg(long)]
    pub continue_on_error: bool,

    /// Only show what would be moved (dry run)
    #[arg(long)]
    pub dry_run: bool,

    /// Apply SSE-S3 to the remote destination path
    #[arg(long = "enc-s3")]
    pub enc_s3: Vec<String>,

    /// Apply SSE-KMS to the remote destination path as TARGET=KMS_KEY_ID
    #[arg(long = "enc-kms")]
    pub enc_kms: Vec<String>,
}

#[derive(Debug, Serialize)]
struct MvOutput {
    status: &'static str,
    source: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
}

/// Execute the mv command
pub async fn execute(args: MvArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let alias_manager = AliasManager::new().ok();

    // Parse source and target paths
    let source = match parse_mv_path(&args.source, alias_manager.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            formatter.error(&format!("Invalid source path: {e}"));
            return ExitCode::UsageError;
        }
    };

    let target = match parse_mv_path(&args.target, alias_manager.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            formatter.error(&format!("Invalid target path: {e}"));
            return ExitCode::UsageError;
        }
    };

    // Determine move direction
    match (&source, &target) {
        (ParsedPath::Local(src), ParsedPath::Remote(dst)) => {
            // Local to S3: upload then delete local
            move_local_to_s3(src, dst, &args, &formatter).await
        }
        (ParsedPath::Remote(src), ParsedPath::Local(dst)) => {
            // S3 to Local: download then delete from S3
            move_s3_to_local(src, dst, &args, &formatter).await
        }
        (ParsedPath::Remote(src), ParsedPath::Remote(dst)) => {
            // S3 to S3: copy then delete source
            move_s3_to_s3(src, dst, &args, &formatter).await
        }
        (ParsedPath::Local(_), ParsedPath::Local(_)) => {
            formatter.error("Cannot move between two local paths. Use system mv command.");
            ExitCode::UsageError
        }
    }
}

fn parse_mv_path(path: &str, alias_manager: Option<&AliasManager>) -> rc_core::Result<ParsedPath> {
    let parsed = parse_path(path)?;

    let ParsedPath::Remote(remote) = &parsed else {
        return Ok(parsed);
    };

    if let Some(manager) = alias_manager
        && matches!(manager.exists(&remote.alias), Ok(true))
    {
        return Ok(parsed);
    }

    if Path::new(path).exists() {
        return Ok(ParsedPath::Local(PathBuf::from(path)));
    }

    Ok(parsed)
}

async fn move_local_to_s3(
    src: &std::path::Path,
    dst: &RemotePath,
    args: &MvArgs,
    formatter: &Formatter,
) -> ExitCode {
    use crate::commands::cp;

    // First, copy local to S3
    let mut cp_args = cp::CpArgs::single(
        src.to_string_lossy().to_string(),
        format!("{}/{}/{}", dst.alias, dst.bucket, dst.key),
    );
    cp_args.recursive = args.recursive;
    cp_args.continue_on_error = args.continue_on_error;
    cp_args.dry_run = args.dry_run;
    cp_args.enc_s3.clone_from(&args.enc_s3);
    cp_args.enc_kms.clone_from(&args.enc_kms);

    let cp_result = cp::execute(
        cp_args,
        OutputConfig {
            json: formatter.is_json(),
            quiet: formatter.is_quiet(),
            ..Default::default()
        },
    )
    .await;

    if cp_result != ExitCode::Success {
        return cp_result;
    }

    // If not dry run, delete local file(s)
    if !args.dry_run {
        if src.is_file()
            && let Err(e) = std::fs::remove_file(src)
        {
            formatter.error(&format!("Failed to delete local file: {e}"));
            return ExitCode::GeneralError;
        } else if src.is_dir()
            && args.recursive
            && let Err(e) = std::fs::remove_dir_all(src)
        {
            formatter.error(&format!("Failed to delete local directory: {e}"));
            return ExitCode::GeneralError;
        }
    }

    ExitCode::Success
}

async fn move_s3_to_local(
    src: &RemotePath,
    dst: &std::path::Path,
    args: &MvArgs,
    formatter: &Formatter,
) -> ExitCode {
    use crate::commands::cp;

    if args.recursive {
        return move_s3_prefix_to_local(src, dst, args, formatter).await;
    }

    // First, copy S3 to local
    let mut cp_args = cp::CpArgs::single(
        format!("{}/{}/{}", src.alias, src.bucket, src.key),
        dst.to_string_lossy().to_string(),
    );
    cp_args.recursive = args.recursive;
    cp_args.continue_on_error = args.continue_on_error;
    cp_args.dry_run = args.dry_run;
    cp_args.enc_s3.clone_from(&args.enc_s3);
    cp_args.enc_kms.clone_from(&args.enc_kms);

    let cp_result = cp::execute(
        cp_args,
        OutputConfig {
            json: formatter.is_json(),
            quiet: formatter.is_quiet(),
            ..Default::default()
        },
    )
    .await;

    if cp_result != ExitCode::Success {
        return cp_result;
    }

    // If not dry run, delete S3 object(s)
    if !args.dry_run {
        let alias_manager = match AliasManager::new() {
            Ok(am) => am,
            Err(e) => {
                formatter.error(&format!("Failed to load aliases: {e}"));
                return ExitCode::GeneralError;
            }
        };

        let alias = match alias_manager.get(&src.alias) {
            Ok(a) => a,
            Err(_) => {
                formatter.error(&format!("Alias '{}' not found", src.alias));
                return ExitCode::NotFound;
            }
        };

        let client = match S3Client::new(alias).await {
            Ok(c) => c,
            Err(e) => {
                formatter.error(&format!("Failed to create S3 client: {e}"));
                return ExitCode::NetworkError;
            }
        };

        if let Err(e) = client.delete_object(src).await {
            formatter.error(&format!("Failed to delete source: {e}"));
            return ExitCode::NetworkError;
        }
    }

    ExitCode::Success
}

async fn move_s3_prefix_to_local(
    src: &RemotePath,
    dst: &Path,
    args: &MvArgs,
    formatter: &Formatter,
) -> ExitCode {
    use crate::commands::cp;

    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            formatter.error(&format!("Failed to load aliases: {error}"));
            return ExitCode::GeneralError;
        }
    };
    let alias = match alias_manager.get(&src.alias) {
        Ok(alias) => alias,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", src.alias));
            return ExitCode::NotFound;
        }
    };
    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            formatter.error(&format!("Failed to create S3 client: {error}"));
            return ExitCode::NetworkError;
        }
    };

    let mut continuation_token = None;
    let mut objects = Vec::new();
    loop {
        let result = match client
            .list_objects(
                src,
                ListOptions {
                    recursive: true,
                    max_keys: Some(1000),
                    continuation_token: continuation_token.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                formatter.error(&format!("Failed to list source objects: {error}"));
                return ExitCode::NetworkError;
            }
        };

        objects.extend(result.items.into_iter().filter(|item| !item.is_dir));
        if !result.truncated {
            break;
        }
        continuation_token = result.continuation_token;
    }

    let mut cp_args = cp::CpArgs::single(src.to_string(), dst.to_string_lossy().to_string());
    cp_args.recursive = true;
    cp_args.continue_on_error = args.continue_on_error;
    cp_args.dry_run = args.dry_run;
    let mut errors = 0usize;

    for item in objects {
        let relative = match cp::safe_download_relative_path(
            &item.key,
            &src.key,
            rc_core::ObjectKeyPolicy::for_local_destination(false),
        ) {
            Ok(relative) => relative,
            Err(error) => {
                errors += 1;
                formatter.error(&format!(
                    "Refusing unsafe object key '{}': {error}",
                    item.key
                ));
                if !args.continue_on_error {
                    return ExitCode::UsageError;
                }
                continue;
            }
        };
        let object = RemotePath::new(&src.alias, &src.bucket, &item.key);
        let target = match cp::safe_download_destination(dst, &relative).await {
            Ok(target) => target,
            Err(error) => {
                errors += 1;
                formatter.error(&format!(
                    "Refusing unsafe destination for '{}': {error}",
                    item.key
                ));
                if !args.continue_on_error {
                    return ExitCode::UsageError;
                }
                continue;
            }
        };
        let result = cp::download_file(&client, &object, &target, &cp_args, formatter).await;
        if result != ExitCode::Success {
            errors += 1;
            if !args.continue_on_error {
                return result;
            }
            continue;
        }

        if !args.dry_run
            && let Err(error) = client.delete_object(&object).await
        {
            errors += 1;
            formatter.error(&format!(
                "Downloaded but failed to delete source '{}': {error}",
                object
            ));
            if !args.continue_on_error {
                return ExitCode::GeneralError;
            }
        }
    }

    if errors == 0 {
        ExitCode::Success
    } else {
        ExitCode::GeneralError
    }
}

/// Copy one object for a move, picking the transfer that the alias pair allows.
///
/// `target_client` is `Some` only for a cross-alias move, where server-side
/// CopyObject is not available and the object must stream through the client.
#[derive(Debug)]
struct MoveCopyResult {
    object: ObjectInfo,
    source_version_id: Option<String>,
    source_etag: Option<String>,
}

async fn copy_for_move(
    source_client: &S3Client,
    target_client: Option<&S3Client>,
    source: &RemotePath,
    target: &RemotePath,
    encryption: Option<&ObjectEncryptionRequest>,
) -> rc_core::Result<MoveCopyResult> {
    match target_client {
        Some(target_client) => {
            let result = cp::copy_object_across_aliases(
                source_client,
                target_client,
                source,
                target,
                encryption,
            )
            .await?;
            Ok(MoveCopyResult {
                object: result.object,
                source_version_id: result.source_version_id,
                source_etag: result.source_etag,
            })
        }
        None => {
            let source_info = source_client.head_object(source).await?;
            let copy_options =
                CopyObjectOptions::for_source_version(source_info.version_id.clone())?;
            let object = source_client
                .copy_object_with_options(source, target, &copy_options, encryption)
                .await?;
            Ok(MoveCopyResult {
                object,
                source_version_id: source_info.version_id,
                source_etag: source_info.etag,
            })
        }
    }
}

async fn delete_moved_source(
    client: &S3Client,
    source: &RemotePath,
    copied: &MoveCopyResult,
) -> rc_core::Result<()> {
    match move_delete_condition(copied)? {
        MoveDeleteCondition::Version(version_id) => {
            rc_core::ObjectStore::delete_object_with_options(
                client,
                source,
                DeleteRequestOptions {
                    version_id: Some(version_id),
                    ..DeleteRequestOptions::default()
                },
            )
            .await?;
            Ok(())
        }
        MoveDeleteCondition::Etag(etag) => client.delete_object_if_match(source, &etag).await,
    }
}

enum MoveDeleteCondition {
    Version(String),
    Etag(String),
}

fn move_delete_condition(copied: &MoveCopyResult) -> rc_core::Result<MoveDeleteCondition> {
    if let Some(version_id) = copied.source_version_id.clone() {
        return Ok(MoveDeleteCondition::Version(version_id));
    }
    copied
        .source_etag
        .clone()
        .map(MoveDeleteCondition::Etag)
        .ok_or_else(|| {
            rc_core::Error::Conflict(
                "Refusing to delete moved source because its ETag is unavailable".to_string(),
            )
        })
}

async fn move_s3_to_s3(
    src: &RemotePath,
    dst: &RemotePath,
    args: &MvArgs,
    formatter: &Formatter,
) -> ExitCode {
    let target = ParsedPath::Remote(dst.clone());
    let encryption = match crate::commands::cp::parse_destination_encryption(
        &args.enc_s3,
        &args.enc_kms,
        &target,
    ) {
        Ok(encryption) => encryption,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error),
    };

    if args.recursive && remote_prefixes_overlap(src, dst) {
        formatter.error("Recursive move source and destination prefixes must not overlap.");
        return ExitCode::UsageError;
    }

    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&src.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", src.alias));
            return ExitCode::NotFound;
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            formatter.error(&format!("Failed to create S3 client: {e}"));
            return ExitCode::NetworkError;
        }
    };

    // A different alias means a different endpoint or credentials, so server-side
    // CopyObject cannot be used. Build a destination client and stream through it.
    let target_client = if src.alias == dst.alias {
        None
    } else {
        let target_alias = match alias_manager.get(&dst.alias) {
            Ok(a) => a,
            Err(_) => {
                formatter.error(&format!("Alias '{}' not found", dst.alias));
                return ExitCode::NotFound;
            }
        };
        match S3Client::new(target_alias).await {
            Ok(c) => Some(c),
            Err(e) => {
                formatter.error(&format!("Failed to create destination S3 client: {e}"));
                return ExitCode::NetworkError;
            }
        }
    };

    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst.key);

    if args.dry_run {
        formatter.println(&format!("Would move: {src_display} -> {dst_display}"));
        return ExitCode::Success;
    }

    // Recursive move for prefix/directory semantics.
    if args.recursive {
        let mut continuation_token: Option<String> = None;
        let mut moved_count = 0usize;
        let mut error_count = 0usize;
        let src_prefix = src.key.clone();
        let mut objects = Vec::new();

        loop {
            let list_opts = ListOptions {
                recursive: true,
                continuation_token: continuation_token.clone(),
                ..Default::default()
            };

            let list_result = match client.list_objects(src, list_opts).await {
                Ok(result) => result,
                Err(e) => {
                    formatter.error(&format!("Failed to list source objects: {e}"));
                    return ExitCode::NetworkError;
                }
            };

            objects.extend(list_result.items.into_iter().filter(|item| !item.is_dir));

            if !list_result.truncated {
                break;
            }
            continuation_token = match list_result.continuation_token.clone() {
                Some(token) => Some(token),
                None => {
                    formatter.error(
                        "Backend indicated truncated results but did not provide a continuation token; stopping to avoid an infinite loop.",
                    );
                    return ExitCode::GeneralError;
                }
            };
        }

        for item in &objects {
            let relative = if src_prefix.is_empty() {
                item.key.clone()
            } else if let Some(rest) = item.key.strip_prefix(&src_prefix) {
                rest.trim_start_matches('/').to_string()
            } else {
                error_count += 1;
                formatter.error(&format!(
                    "Source listing returned key '{}' outside prefix '{}'",
                    item.key, src_prefix
                ));
                if !args.continue_on_error {
                    return ExitCode::GeneralError;
                }
                continue;
            };

            if relative.is_empty() {
                error_count += 1;
                formatter.error(&format!(
                    "Cannot derive a destination key for source '{}'",
                    item.key
                ));
                if !args.continue_on_error {
                    return ExitCode::UsageError;
                }
                continue;
            }

            let target_key = if dst.key.is_empty() {
                relative.clone()
            } else if dst.key.ends_with('/') {
                format!("{}{}", dst.key, relative)
            } else {
                format!("{}/{}", dst.key, relative)
            };

            let src_obj = RemotePath::new(&src.alias, &src.bucket, &item.key);
            let dst_obj = RemotePath::new(&dst.alias, &dst.bucket, &target_key);
            let src_obj_display = src_obj.to_string();
            let dst_obj_display = dst_obj.to_string();

            match copy_for_move(
                &client,
                target_client.as_ref(),
                &src_obj,
                &dst_obj,
                encryption.as_ref(),
            )
            .await
            {
                Ok(copied) => match delete_moved_source(&client, &src_obj, &copied).await {
                    Ok(()) => {
                        moved_count += 1;
                        if !formatter.is_json() {
                            formatter.println(&format!("{src_obj_display} -> {dst_obj_display}"));
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        formatter.error(&format!(
                            "Copied but failed to delete source '{src_obj_display}': {e}"
                        ));
                        if !args.continue_on_error {
                            return ExitCode::GeneralError;
                        }
                    }
                },
                Err(e) => {
                    error_count += 1;
                    formatter.error(&format!(
                        "Failed to move '{src_obj_display}' -> '{dst_obj_display}': {e}"
                    ));
                    if !args.continue_on_error {
                        return ExitCode::NetworkError;
                    }
                }
            }
        }

        if formatter.is_json() {
            #[derive(Serialize)]
            struct MvRecursiveOutput {
                status: &'static str,
                source: String,
                target: String,
                moved: usize,
                errors: usize,
            }

            formatter.json(&MvRecursiveOutput {
                status: if error_count == 0 {
                    "success"
                } else {
                    "partial"
                },
                source: src_display,
                target: dst_display,
                moved: moved_count,
                errors: error_count,
            });
        } else if error_count == 0 {
            formatter.println(&format!("Moved {moved_count} object(s)."));
        } else {
            formatter.println(&format!(
                "Moved {moved_count} object(s), {error_count} failed."
            ));
        }

        if error_count == 0 {
            ExitCode::Success
        } else {
            ExitCode::GeneralError
        }
    } else {
        // Copy
        match copy_for_move(
            &client,
            target_client.as_ref(),
            src,
            dst,
            encryption.as_ref(),
        )
        .await
        {
            Ok(copied) => {
                // Delete source
                if let Err(e) = delete_moved_source(&client, src, &copied).await {
                    formatter.error(&format!("Copied but failed to delete source: {e}"));
                    return ExitCode::GeneralError;
                }

                if formatter.is_json() {
                    let output = MvOutput {
                        status: "success",
                        source: src_display,
                        target: dst_display,
                        size_bytes: copied.object.size_bytes,
                    };
                    formatter.json(&output);
                } else {
                    formatter.println(&format!(
                        "{src_display} -> {dst_display} ({})",
                        copied.object.size_human.unwrap_or_default()
                    ));
                }
                ExitCode::Success
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                    formatter.error(&format!("Source not found: {src_display}"));
                    ExitCode::NotFound
                } else {
                    formatter.error(&format!("Failed to move: {e}"));
                    ExitCode::NetworkError
                }
            }
        }
    }
}

fn remote_prefixes_overlap(source: &RemotePath, target: &RemotePath) -> bool {
    if source.alias != target.alias || source.bucket != target.bucket {
        return false;
    }

    let source = source.key.trim_matches('/');
    let target = target.key.trim_matches('/');
    if source.is_empty() || target.is_empty() || source == target {
        return true;
    }

    target
        .strip_prefix(source)
        .is_some_and(|rest| rest.starts_with('/'))
        || source
            .strip_prefix(target)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::{Alias, ConfigManager};
    use tempfile::TempDir;

    fn temp_alias_manager() -> (AliasManager, TempDir) {
        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let config_manager = ConfigManager::with_path(config_path);
        let alias_manager = AliasManager::with_config_manager(config_manager);
        (alias_manager, temp_dir)
    }

    #[test]
    fn test_parse_paths() {
        // Local path
        let local = parse_path("./file.txt").unwrap();
        assert!(matches!(local, ParsedPath::Local(_)));

        // Remote path
        let remote = parse_path("myalias/bucket/file.txt").unwrap();
        assert!(matches!(remote, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_parse_local_absolute_path() {
        // Use platform-appropriate absolute path
        #[cfg(unix)]
        let path = "/tmp/file.txt";
        #[cfg(windows)]
        let path = "C:\\temp\\file.txt";

        let result = parse_path(path).unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
        if let ParsedPath::Local(p) = result {
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn test_parse_remote_path_components() {
        let result = parse_path("s3/mybucket/path/to/file.txt").unwrap();
        if let ParsedPath::Remote(r) = result {
            assert_eq!(r.alias, "s3");
            assert_eq!(r.bucket, "mybucket");
            assert_eq!(r.key, "path/to/file.txt");
        } else {
            panic!("Expected Remote path");
        }
    }

    #[test]
    fn test_parse_mv_path_prefers_existing_local_path_when_alias_missing() {
        let (alias_manager, temp_dir) = temp_alias_manager();
        let full = temp_dir.path().join("issue-2094-mv-local").join("file.txt");
        let full_str = full.to_string_lossy().to_string();

        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&full, b"test").expect("write local file");

        let parsed = parse_mv_path(&full_str, Some(&alias_manager)).expect("parse path");
        assert!(matches!(parsed, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_mv_path_keeps_remote_when_alias_exists() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        alias_manager
            .set(Alias::new("target", "http://localhost:9000", "a", "b"))
            .expect("set alias");

        let parsed = parse_mv_path("target/bucket/file.txt", Some(&alias_manager))
            .expect("parse remote path");
        assert!(matches!(parsed, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_parse_mv_path_keeps_remote_when_local_missing() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        let parsed = parse_mv_path("missing/bucket/file.txt", Some(&alias_manager))
            .expect("parse remote path");
        assert!(matches!(parsed, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_mv_args_defaults() {
        let args = MvArgs {
            source: "src".to_string(),
            target: "dst".to_string(),
            recursive: false,
            continue_on_error: false,
            dry_run: false,
            enc_s3: Vec::new(),
            enc_kms: Vec::new(),
        };
        assert!(!args.recursive);
        assert!(!args.dry_run);
        assert!(!args.continue_on_error);
    }

    #[test]
    fn test_mv_args_store_encryption_flags() {
        let args = MvArgs {
            source: "src".to_string(),
            target: "dst".to_string(),
            recursive: false,
            continue_on_error: false,
            dry_run: false,
            enc_s3: vec!["local/bucket/dst.txt".to_string()],
            enc_kms: vec!["local/bucket/dst.txt=kms-key".to_string()],
        };

        assert_eq!(args.enc_s3.len(), 1);
        assert_eq!(args.enc_kms.len(), 1);
    }

    #[test]
    fn recursive_move_rejects_overlapping_remote_prefixes() {
        let source = RemotePath::new("local", "bucket", "source/");
        let nested_target = RemotePath::new("local", "bucket", "source/archive/");
        let parent_target = RemotePath::new("local", "bucket", "");
        let separate_target = RemotePath::new("local", "bucket", "archive/");

        assert!(remote_prefixes_overlap(&source, &nested_target));
        assert!(remote_prefixes_overlap(&source, &parent_target));
        assert!(!remote_prefixes_overlap(&source, &separate_target));
    }

    #[test]
    fn moved_source_deletion_prefers_exact_version_then_etag_condition() {
        let versioned = MoveCopyResult {
            object: ObjectInfo::file("target", 1),
            source_version_id: Some("source-v1".to_string()),
            source_etag: Some("source-etag".to_string()),
        };
        assert!(matches!(
            move_delete_condition(&versioned),
            Ok(MoveDeleteCondition::Version(version)) if version == "source-v1"
        ));

        let unversioned = MoveCopyResult {
            object: ObjectInfo::file("target", 1),
            source_version_id: None,
            source_etag: Some("source-etag".to_string()),
        };
        assert!(matches!(
            move_delete_condition(&unversioned),
            Ok(MoveDeleteCondition::Etag(etag)) if etag == "source-etag"
        ));

        let without_identity = MoveCopyResult {
            object: ObjectInfo::file("target", 1),
            source_version_id: None,
            source_etag: None,
        };
        assert!(matches!(
            move_delete_condition(&without_identity),
            Err(rc_core::Error::Conflict(_))
        ));
    }

    #[test]
    fn test_mv_output_serialization() {
        let output = MvOutput {
            status: "success",
            source: "src/file.txt".to_string(),
            target: "dst/file.txt".to_string(),
            size_bytes: Some(2048),
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"size_bytes\":2048"));
    }

    #[test]
    fn test_mv_output_skips_none_size() {
        let output = MvOutput {
            status: "success",
            source: "src".to_string(),
            target: "dst".to_string(),
            size_bytes: None,
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(!json.contains("size_bytes"));
    }
}
