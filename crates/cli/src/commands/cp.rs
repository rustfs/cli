//! cp command - Copy objects
//!
//! Copies objects between local filesystem and S3, or between S3 locations.

use clap::Args;
use rc_core::{parse_path, AliasManager, ObjectStore as _, ParsedPath, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;
use std::path::Path;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Copy objects
#[derive(Args, Debug)]
pub struct CpArgs {
    /// Source path (local path or alias/bucket/key)
    pub source: String,

    /// Destination path (local path or alias/bucket/key)
    pub target: String,

    /// Copy recursively
    #[arg(short, long)]
    pub recursive: bool,

    /// Preserve file attributes
    #[arg(short, long)]
    pub preserve: bool,

    /// Continue on errors
    #[arg(long)]
    pub continue_on_error: bool,

    /// Overwrite destination if it exists
    #[arg(long, default_value = "true")]
    pub overwrite: bool,

    /// Only show what would be copied (dry run)
    #[arg(long)]
    pub dry_run: bool,

    /// Storage class for destination (S3 only)
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Content type for uploaded files
    #[arg(long)]
    pub content_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct CpOutput {
    status: &'static str,
    source: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_human: Option<String>,
}

/// Execute the cp command
pub async fn execute(args: CpArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse source and target paths
    let source = match parse_path(&args.source) {
        Ok(p) => p,
        Err(e) => {
            formatter.error(&format!("Invalid source path: {e}"));
            return ExitCode::UsageError;
        }
    };

    let target = match parse_path(&args.target) {
        Ok(p) => p,
        Err(e) => {
            formatter.error(&format!("Invalid target path: {e}"));
            return ExitCode::UsageError;
        }
    };

    // Determine copy direction
    match (&source, &target) {
        (ParsedPath::Local(src), ParsedPath::Remote(dst)) => {
            // Local to S3
            copy_local_to_s3(src, dst, &args, &formatter).await
        }
        (ParsedPath::Remote(src), ParsedPath::Local(dst)) => {
            // S3 to Local
            copy_s3_to_local(src, dst, &args, &formatter).await
        }
        (ParsedPath::Remote(src), ParsedPath::Remote(dst)) => {
            // S3 to S3
            copy_s3_to_s3(src, dst, &args, &formatter).await
        }
        (ParsedPath::Local(_), ParsedPath::Local(_)) => {
            formatter.error("Cannot copy between two local paths. Use system cp command.");
            ExitCode::UsageError
        }
    }
}

async fn copy_local_to_s3(
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    // Check if source exists
    if !src.exists() {
        formatter.error(&format!("Source not found: {}", src.display()));
        return ExitCode::NotFound;
    }

    // If source is a directory, require recursive flag
    if src.is_dir() && !args.recursive {
        formatter.error("Source is a directory. Use -r/--recursive to copy directories.");
        return ExitCode::UsageError;
    }

    // Load alias and create client
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let alias = match alias_manager.get(&dst.alias) {
        Ok(a) => a,
        Err(_) => {
            formatter.error(&format!("Alias '{}' not found", dst.alias));
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

    if src.is_file() {
        // Single file upload
        upload_file(&client, src, dst, args, formatter).await
    } else {
        // Directory upload
        upload_directory(&client, src, dst, args, formatter).await
    }
}

async fn upload_file(
    client: &S3Client,
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    // Determine destination key
    let dst_key = if dst.key.is_empty() || dst.key.ends_with('/') {
        // If destination is a directory, use source filename
        let filename = src.file_name().unwrap_or_default().to_string_lossy();
        format!("{}{}", dst.key, filename)
    } else {
        dst.key.clone()
    };

    let target = RemotePath::new(&dst.alias, &dst.bucket, &dst_key);
    let src_display = src.display().to_string();
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst_key);

    if args.dry_run {
        formatter.println(&format!("Would copy: {src_display} -> {dst_display}"));
        return ExitCode::Success;
    }

    // Read file content
    let data = match std::fs::read(src) {
        Ok(d) => d,
        Err(e) => {
            formatter.error(&format!("Failed to read {src_display}: {e}"));
            return ExitCode::GeneralError;
        }
    };

    let size = data.len() as i64;

    // Determine content type
    let guessed_type: Option<String> = mime_guess::from_path(src)
        .first()
        .map(|m| m.essence_str().to_string());
    let content_type = args.content_type.as_deref().or(guessed_type.as_deref());

    // Upload
    match client.put_object(&target, data, content_type).await {
        Ok(info) => {
            if formatter.is_json() {
                let output = CpOutput {
                    status: "success",
                    source: src_display,
                    target: dst_display,
                    size_bytes: Some(size),
                    size_human: Some(humansize::format_size(size as u64, humansize::BINARY)),
                };
                formatter.json(&output);
            } else {
                formatter.println(&format!(
                    "{src_display} -> {dst_display} ({})",
                    info.size_human.unwrap_or_default()
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to upload {src_display}: {e}"));
            ExitCode::NetworkError
        }
    }
}

async fn upload_directory(
    client: &S3Client,
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    use std::fs;

    let mut success_count = 0;
    let mut error_count = 0;

    // Walk directory
    fn walk_dir(dir: &Path, base: &Path) -> std::io::Result<Vec<(std::path::PathBuf, String)>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let relative = path.strip_prefix(base).unwrap_or(&path);
                let relative_str = relative.to_string_lossy().to_string();
                files.push((path, relative_str));
            } else if path.is_dir() {
                files.extend(walk_dir(&path, base)?);
            }
        }
        Ok(files)
    }

    let files = match walk_dir(src, src) {
        Ok(f) => f,
        Err(e) => {
            formatter.error(&format!("Failed to read directory: {e}"));
            return ExitCode::GeneralError;
        }
    };

    for (file_path, relative_path) in files {
        // Build destination key
        let dst_key = if dst.key.is_empty() {
            relative_path.replace('\\', "/")
        } else if dst.key.ends_with('/') {
            format!("{}{}", dst.key, relative_path.replace('\\', "/"))
        } else {
            format!("{}/{}", dst.key, relative_path.replace('\\', "/"))
        };

        let target = RemotePath::new(&dst.alias, &dst.bucket, &dst_key);

        let result = upload_file(client, &file_path, &target, args, formatter).await;

        if result == ExitCode::Success {
            success_count += 1;
        } else {
            error_count += 1;
            if !args.continue_on_error {
                return result;
            }
        }
    }

    if error_count > 0 {
        formatter.warning(&format!(
            "Completed with errors: {success_count} succeeded, {error_count} failed"
        ));
        ExitCode::GeneralError
    } else {
        if !formatter.is_json() {
            formatter.success(&format!("Uploaded {success_count} file(s)."));
        }
        ExitCode::Success
    }
}

async fn copy_s3_to_local(
    src: &RemotePath,
    dst: &Path,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    // Load alias and create client
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

    // Check if source is a prefix (directory-like)
    let is_prefix = src.key.is_empty() || src.key.ends_with('/');

    if is_prefix || args.recursive {
        // Download multiple objects
        download_prefix(&client, src, dst, args, formatter).await
    } else {
        // Download single object
        download_file(&client, src, dst, args, formatter).await
    }
}

async fn download_file(
    client: &S3Client,
    src: &RemotePath,
    dst: &Path,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);

    // Determine destination path
    let dst_path = if dst.is_dir() || dst.to_string_lossy().ends_with('/') {
        let filename = src.key.rsplit('/').next().unwrap_or(&src.key);
        dst.join(filename)
    } else {
        dst.to_path_buf()
    };

    let dst_display = dst_path.display().to_string();

    if args.dry_run {
        formatter.println(&format!("Would copy: {src_display} -> {dst_display}"));
        return ExitCode::Success;
    }

    // Check if destination exists
    if dst_path.exists() && !args.overwrite {
        formatter.error(&format!(
            "Destination exists: {dst_display}. Use --overwrite to replace."
        ));
        return ExitCode::Conflict;
    }

    // Create parent directories
    if let Some(parent) = dst_path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                formatter.error(&format!("Failed to create directory: {e}"));
                return ExitCode::GeneralError;
            }
        }
    }

    // Download object
    match client.get_object(src).await {
        Ok(data) => {
            let size = data.len() as i64;

            if let Err(e) = std::fs::write(&dst_path, &data) {
                formatter.error(&format!("Failed to write {dst_display}: {e}"));
                return ExitCode::GeneralError;
            }

            if formatter.is_json() {
                let output = CpOutput {
                    status: "success",
                    source: src_display,
                    target: dst_display,
                    size_bytes: Some(size),
                    size_human: Some(humansize::format_size(size as u64, humansize::BINARY)),
                };
                formatter.json(&output);
            } else {
                formatter.println(&format!(
                    "{src_display} -> {dst_display} ({})",
                    humansize::format_size(size as u64, humansize::BINARY)
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                formatter.error(&format!("Object not found: {src_display}"));
                ExitCode::NotFound
            } else {
                formatter.error(&format!("Failed to download {src_display}: {e}"));
                ExitCode::NetworkError
            }
        }
    }
}

async fn download_prefix(
    client: &S3Client,
    src: &RemotePath,
    dst: &Path,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    use rc_core::ListOptions;

    let mut success_count = 0;
    let mut error_count = 0;
    let mut continuation_token: Option<String> = None;

    loop {
        let options = ListOptions {
            recursive: true,
            max_keys: Some(1000),
            continuation_token: continuation_token.clone(),
            ..Default::default()
        };

        match client.list_objects(src, options).await {
            Ok(result) => {
                for item in result.items {
                    if item.is_dir {
                        continue;
                    }

                    // Calculate relative path from prefix
                    let relative_key = item.key.strip_prefix(&src.key).unwrap_or(&item.key);
                    let dst_path =
                        dst.join(relative_key.replace('/', std::path::MAIN_SEPARATOR_STR));

                    let obj_src = RemotePath::new(&src.alias, &src.bucket, &item.key);
                    let result = download_file(client, &obj_src, &dst_path, args, formatter).await;

                    if result == ExitCode::Success {
                        success_count += 1;
                    } else {
                        error_count += 1;
                        if !args.continue_on_error {
                            return result;
                        }
                    }
                }

                if result.truncated {
                    continuation_token = result.continuation_token;
                } else {
                    break;
                }
            }
            Err(e) => {
                formatter.error(&format!("Failed to list objects: {e}"));
                return ExitCode::NetworkError;
            }
        }
    }

    if error_count > 0 {
        formatter.warning(&format!(
            "Completed with errors: {success_count} succeeded, {error_count} failed"
        ));
        ExitCode::GeneralError
    } else if success_count == 0 {
        formatter.warning("No objects found to download.");
        ExitCode::Success
    } else {
        if !formatter.is_json() {
            formatter.success(&format!("Downloaded {success_count} file(s)."));
        }
        ExitCode::Success
    }
}

async fn copy_s3_to_s3(
    src: &RemotePath,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    // For S3-to-S3, we need to handle same or different aliases
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return ExitCode::GeneralError;
        }
    };

    // For now, only support same-alias copies (server-side copy)
    if src.alias != dst.alias {
        formatter.error("Cross-alias S3-to-S3 copy not yet supported. Use download + upload.");
        return ExitCode::UnsupportedFeature;
    }

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

    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst.key);

    if args.dry_run {
        formatter.println(&format!("Would copy: {src_display} -> {dst_display}"));
        return ExitCode::Success;
    }

    match client.copy_object(src, dst).await {
        Ok(info) => {
            if formatter.is_json() {
                let output = CpOutput {
                    status: "success",
                    source: src_display,
                    target: dst_display,
                    size_bytes: info.size_bytes,
                    size_human: info.size_human,
                };
                formatter.json(&output);
            } else {
                formatter.println(&format!(
                    "{src_display} -> {dst_display} ({})",
                    info.size_human.unwrap_or_default()
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
                formatter.error(&format!("Failed to copy: {e}"));
                ExitCode::NetworkError
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_path() {
        let result = parse_path("./file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_remote_path() {
        let result = parse_path("myalias/bucket/file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
    }
}
