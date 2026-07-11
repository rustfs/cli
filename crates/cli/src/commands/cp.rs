//! cp command - Copy objects
//!
//! Copies objects between local filesystem and S3, or between S3 locations.

use clap::Args;
use rc_core::{
    AliasManager, ObjectEncryptionRequest, ObjectStore as _, ParsedPath, RemotePath, parse_path,
};
use rc_s3::S3Client;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig, ProgressBar};

const CP_AFTER_HELP: &str = "\
Examples:
  rc object copy ./report.json local/my-bucket/reports/
  rc cp ./report.json local/my-bucket/reports/
  rc object copy local/source-bucket/archive.tar.gz ./downloads/archive.tar.gz";

const REMOTE_PATH_SUGGESTION: &str =
    "Use a local filesystem path or a remote path in the form alias/bucket[/key].";

/// Copy objects
#[derive(Args, Debug)]
#[command(after_help = CP_AFTER_HELP)]
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

    /// Apply SSE-S3 to the remote destination path
    #[arg(long = "enc-s3")]
    pub enc_s3: Vec<String>,

    /// Apply SSE-KMS to the remote destination path as TARGET=KMS_KEY_ID
    #[arg(long = "enc-kms")]
    pub enc_kms: Vec<String>,
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

    if args.preserve {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--preserve is not implemented; refusing to continue with a silently ignored option",
        );
    }
    if args.storage_class.is_some() {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--storage-class is not implemented; refusing to continue with a silently ignored option",
        );
    }

    let alias_manager = AliasManager::new().ok();

    // Parse source and target paths
    let source = match parse_cp_path(&args.source, alias_manager.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &format!("Invalid source path: {e}"),
                REMOTE_PATH_SUGGESTION,
            );
        }
    };

    let target = match parse_cp_path(&args.target, alias_manager.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &format!("Invalid target path: {e}"),
                REMOTE_PATH_SUGGESTION,
            );
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
            if args.recursive {
                return formatter.fail(
                    ExitCode::UnsupportedFeature,
                    "Recursive S3-to-S3 copy is not implemented",
                );
            }
            // S3 to S3
            copy_s3_to_s3(src, dst, &args, &formatter).await
        }
        (ParsedPath::Local(_), ParsedPath::Local(_)) => formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Cannot copy between two local paths. Use system cp command.",
            "Use your local shell cp command when both paths are on the filesystem.",
        ),
    }
}

fn parse_cp_path(path: &str, alias_manager: Option<&AliasManager>) -> rc_core::Result<ParsedPath> {
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

async fn copy_local_to_s3(
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    let target = ParsedPath::Remote(dst.clone());
    let encryption = match parse_destination_encryption(&args.enc_s3, &args.enc_kms, &target) {
        Ok(encryption) => encryption,
        Err(error) => {
            return formatter.fail(ExitCode::UsageError, &error);
        }
    };

    // Check if source exists
    if !src.exists() {
        return formatter.fail_with_suggestion(
            ExitCode::NotFound,
            &format!("Source not found: {}", src.display()),
            "Check the local source path and retry the copy command.",
        );
    }

    // If source is a directory, require recursive flag
    if src.is_dir() && !args.recursive {
        return formatter.fail_with_suggestion(
            ExitCode::UsageError,
            "Source is a directory. Use -r/--recursive to copy directories.",
            "Retry with -r or --recursive to copy a directory tree.",
        );
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
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", dst.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };

    if src.is_file() {
        // Single file upload
        upload_file(&client, src, dst, args, formatter, encryption.as_ref()).await
    } else {
        // Directory upload
        upload_directory(&client, src, dst, args, formatter, encryption.as_ref()).await
    }
}

/// Multipart upload threshold: files larger than this size use multipart upload.
const MULTIPART_THRESHOLD: u64 = rc_s3::multipart::DEFAULT_PART_SIZE;
/// Download progress threshold: avoid flicker for tiny downloads while surfacing meaningful waits.
const DOWNLOAD_PROGRESS_THRESHOLD: u64 = 4 * 1024 * 1024;

fn update_download_progress(
    progress: &mut Option<ProgressBar>,
    output_config: &OutputConfig,
    bytes_downloaded: u64,
    total_size: Option<u64>,
) {
    let Some(total_size) = total_size else {
        return;
    };

    if total_size < DOWNLOAD_PROGRESS_THRESHOLD {
        return;
    }

    let progress_bar =
        progress.get_or_insert_with(|| ProgressBar::new(output_config.clone(), total_size));
    progress_bar.set_position(bytes_downloaded);
}

fn print_upload_success(
    formatter: &Formatter,
    info: &rc_core::ObjectInfo,
    src_display: &str,
    dst_display: &str,
) {
    if formatter.is_json() {
        let output = CpOutput {
            status: "success",
            source: src_display.to_string(),
            target: dst_display.to_string(),
            size_bytes: info.size_bytes,
            size_human: info.size_human.clone(),
        };
        formatter.json(&output);
    } else {
        let styled_src = formatter.style_file(src_display);
        let styled_dst = formatter.style_file(dst_display);
        let styled_size = formatter.style_size(&info.size_human.clone().unwrap_or_default());
        formatter.println(&format!("{styled_src} -> {styled_dst} ({styled_size})"));
    }
}

async fn upload_file(
    client: &S3Client,
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
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
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!("Would copy: {styled_src} -> {styled_dst}"));
        return ExitCode::Success;
    }

    // Get file size for progress bar decision
    let file_size = match std::fs::metadata(src) {
        Ok(m) => m.len(),
        Err(e) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read {src_display}: {e}"),
            );
        }
    };

    // Determine content type
    let guessed_type: Option<String> = mime_guess::from_path(src)
        .first()
        .map(|m| m.essence_str().to_string());
    let content_type = select_upload_content_type(
        args.content_type.as_deref(),
        guessed_type.as_deref(),
        file_size,
    );

    // Show progress bar for large files
    let progress = if file_size > MULTIPART_THRESHOLD {
        tracing::debug!(
            file_size,
            threshold = MULTIPART_THRESHOLD,
            "Using multipart upload for large file"
        );
        Some(ProgressBar::new(formatter.output_config(), file_size))
    } else {
        tracing::debug!(file_size, "Using single put_object for small file");
        None
    };

    // Upload
    match client
        .put_object_from_path(&target, src, content_type, encryption, |bytes_sent| {
            if let Some(ref pb) = progress {
                pb.set_position(bytes_sent);
            }
        })
        .await
    {
        Ok(info) => {
            if let Some(ref pb) = progress {
                pb.finish_and_clear();
            }
            print_upload_success(formatter, &info, &src_display, &dst_display);
            ExitCode::Success
        }
        Err(e) => {
            if let Some(ref pb) = progress {
                pb.finish_and_clear();
            }
            formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to upload {src_display}: {e}"),
            )
        }
    }
}

fn select_upload_content_type<'a>(
    explicit_type: Option<&'a str>,
    guessed_type: Option<&'a str>,
    file_size: u64,
) -> Option<&'a str> {
    if file_size > MULTIPART_THRESHOLD {
        explicit_type
    } else {
        explicit_type.or(guessed_type)
    }
}

async fn upload_directory(
    client: &S3Client,
    src: &Path,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
    encryption: Option<&ObjectEncryptionRequest>,
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
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read directory: {e}"),
            );
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

        let result = upload_file(client, &file_path, &target, args, formatter, encryption).await;

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
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", src.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
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

pub(super) async fn download_file(
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
        let filename = match safe_download_relative_path(filename, "") {
            Ok(filename) => filename,
            Err(error) => {
                return formatter.fail(
                    ExitCode::UsageError,
                    &format!("Unsafe object key '{}': {error}", src.key),
                );
            }
        };
        dst.join(filename)
    } else {
        dst.to_path_buf()
    };

    let dst_display = dst_path.display().to_string();

    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!("Would copy: {styled_src} -> {styled_dst}"));
        return ExitCode::Success;
    }

    // Check if destination exists
    if dst_path.exists() && !args.overwrite {
        return formatter.fail_with_suggestion(
            ExitCode::Conflict,
            &format!("Destination exists: {dst_display}. Use --overwrite to replace."),
            "Retry with --overwrite if replacing the destination file is intended.",
        );
    }

    // Create parent directories
    if let Some(parent) = dst_path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to create directory: {e}"),
        );
    }

    let output_config = formatter.output_config();
    let mut progress = None;

    // Download object
    let result = client
        .download_object_to_path(src, &dst_path, |bytes_downloaded, total_size| {
            update_download_progress(&mut progress, &output_config, bytes_downloaded, total_size);
        })
        .await;

    if let Some(ref pb) = progress {
        pb.finish_and_clear();
    }

    match result {
        Ok(size) => {
            let size = size as i64;

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
                let styled_src = formatter.style_file(&src_display);
                let styled_dst = formatter.style_file(&dst_display);
                let styled_size =
                    formatter.style_size(&humansize::format_size(size as u64, humansize::BINARY));
                formatter.println(&format!("{styled_src} -> {styled_dst} ({styled_size})"));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                formatter.fail_with_suggestion(
                    ExitCode::NotFound,
                    &format!("Object not found: {src_display}"),
                    "Check the object key and bucket path, then retry the copy command.",
                )
            } else {
                formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to download {src_display}: {e}"),
                )
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
                    let relative_path = match safe_download_relative_path(&item.key, &src.key) {
                        Ok(path) => path,
                        Err(error) => {
                            error_count += 1;
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
                    let dst_path = match safe_download_destination(dst, &relative_path).await {
                        Ok(path) => path,
                        Err(error) => {
                            error_count += 1;
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
                return formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to list objects: {e}"),
                );
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

pub(super) fn safe_download_relative_path(key: &str, prefix: &str) -> Result<PathBuf, String> {
    let relative = key
        .strip_prefix(prefix)
        .ok_or_else(|| format!("key is outside requested prefix '{prefix}'"))?
        .trim_start_matches('/');

    let mut path = PathBuf::new();
    for component in relative.split(['/', '\\']) {
        if component.is_empty() {
            continue;
        }
        if matches!(component, "." | "..") {
            return Err("path traversal components are not allowed".to_string());
        }
        if component.contains(':') {
            return Err("colon characters are not allowed in download paths".to_string());
        }
        if component.ends_with(['.', ' ']) {
            return Err("download path components must not end in a dot or space".to_string());
        }
        let stem = component.split('.').next().unwrap_or_default();
        let stem = stem.to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err("reserved Windows device names are not allowed".to_string());
        }
        path.push(component);
    }

    if path.as_os_str().is_empty() {
        return Err("object key does not contain a file path".to_string());
    }

    Ok(path)
}

pub(super) async fn safe_download_destination(
    root: &Path,
    relative: &Path,
) -> Result<PathBuf, String> {
    let mut destination = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("destination path contains a non-normal component".to_string());
        };
        destination.push(component);
        match tokio::fs::symlink_metadata(&destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "destination component '{}' is a symbolic link",
                    destination.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect destination '{}': {error}",
                    destination.display()
                ));
            }
        }
    }

    Ok(destination)
}

async fn copy_s3_to_s3(
    src: &RemotePath,
    dst: &RemotePath,
    args: &CpArgs,
    formatter: &Formatter,
) -> ExitCode {
    let target = ParsedPath::Remote(dst.clone());
    let encryption = match parse_destination_encryption(&args.enc_s3, &args.enc_kms, &target) {
        Ok(encryption) => encryption,
        Err(error) => {
            return formatter.fail(ExitCode::UsageError, &error);
        }
    };

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
        return formatter.fail_with_suggestion(
            ExitCode::UnsupportedFeature,
            "Cross-alias S3-to-S3 copy not yet supported. Use download + upload.",
            "Copy via a local path or split the operation into download and upload steps.",
        );
    }

    let alias = match alias_manager.get(&src.alias) {
        Ok(a) => a,
        Err(_) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{}' not found", src.alias),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            );
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(c) => c,
        Err(e) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {e}"),
            );
        }
    };

    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst.key);

    if args.dry_run {
        let styled_src = formatter.style_file(&src_display);
        let styled_dst = formatter.style_file(&dst_display);
        formatter.println(&format!("Would copy: {styled_src} -> {styled_dst}"));
        return ExitCode::Success;
    }

    match client.head_object(src).await {
        Ok(info)
            if info
                .size_bytes
                .is_some_and(|size| size > 5 * 1024 * 1024 * 1024) =>
        {
            return formatter.fail(
                ExitCode::UnsupportedFeature,
                "Objects larger than 5 GiB require multipart copy, which is not implemented",
            );
        }
        Ok(_) => {}
        Err(rc_core::Error::NotFound(_)) => {
            return formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Source not found: {src_display}"),
                "Check the source bucket and object key, then retry the copy command.",
            );
        }
        Err(error) => {
            return formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to inspect source object: {error}"),
            );
        }
    }

    match client.copy_object(src, dst, encryption.as_ref()).await {
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
                let styled_src = formatter.style_file(&src_display);
                let styled_dst = formatter.style_file(&dst_display);
                let styled_size = formatter.style_size(&info.size_human.unwrap_or_default());
                formatter.println(&format!("{styled_src} -> {styled_dst} ({styled_size})"));
            }
            ExitCode::Success
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("NotFound") || err_str.contains("NoSuchKey") {
                formatter.fail_with_suggestion(
                    ExitCode::NotFound,
                    &format!("Source not found: {src_display}"),
                    "Check the source bucket and object key, then retry the copy command.",
                )
            } else {
                formatter.fail(ExitCode::NetworkError, &format!("Failed to copy: {e}"))
            }
        }
    }
}

fn parse_kms_target(value: &str) -> Result<(String, String), String> {
    let (target, key_id) = value
        .split_once('=')
        .ok_or_else(|| "Expected TARGET=KMS_KEY_ID for --enc-kms".to_string())?;

    if target.is_empty() || key_id.is_empty() {
        return Err("Expected TARGET=KMS_KEY_ID for --enc-kms".to_string());
    }

    Ok((target.to_string(), key_id.to_string()))
}

pub(crate) fn parse_destination_encryption(
    enc_s3: &[String],
    enc_kms: &[String],
    target: &ParsedPath,
) -> Result<Option<ObjectEncryptionRequest>, String> {
    if enc_s3.is_empty() && enc_kms.is_empty() {
        return Ok(None);
    }

    let remote = match target {
        ParsedPath::Remote(remote) => remote,
        ParsedPath::Local(_) => {
            return Err("Destination encryption flags must reference a remote destination".into());
        }
    };

    let target_display = remote.to_string();
    let s3_matches = enc_s3.iter().any(|value| value == &target_display);
    let kms_targets = enc_kms
        .iter()
        .map(|value| parse_kms_target(value))
        .collect::<Result<Vec<_>, _>>()?;
    let kms_match = kms_targets
        .iter()
        .find(|(candidate, _)| candidate == &target_display);

    if !enc_s3.is_empty() && !s3_matches {
        return Err(format!(
            "--enc-s3 target must exactly match the remote destination: {target_display}"
        ));
    }

    if !enc_kms.is_empty() && kms_match.is_none() {
        return Err(format!(
            "--enc-kms target must exactly match the remote destination: {target_display}"
        ));
    }

    match (s3_matches, kms_match) {
        (true, Some(_)) => Err(format!(
            "--enc-s3 and --enc-kms cannot target the same destination: {target_display}"
        )),
        (true, None) => Ok(Some(ObjectEncryptionRequest::SseS3)),
        (false, Some((_, key_id))) => Ok(Some(ObjectEncryptionRequest::SseKms {
            key_id: key_id.clone(),
        })),
        (false, None) => Ok(None),
    }
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
    fn test_parse_local_path() {
        let result = parse_path("./file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_remote_path() {
        let result = parse_path("myalias/bucket/file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_parse_local_absolute_path() {
        // Use platform-appropriate absolute path
        #[cfg(unix)]
        let path = "/home/user/file.txt";
        #[cfg(windows)]
        let path = "C:\\Users\\user\\file.txt";

        let result = parse_path(path).unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
        if let ParsedPath::Local(p) = result {
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn test_parse_local_relative_path() {
        let result = parse_path("../file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_remote_path_bucket_only() {
        let result = parse_path("myalias/bucket/").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
        if let ParsedPath::Remote(r) = result {
            assert_eq!(r.alias, "myalias");
            assert_eq!(r.bucket, "bucket");
            assert!(r.key.is_empty());
        }
    }

    #[test]
    fn test_parse_remote_path_with_deep_key() {
        let result = parse_path("myalias/bucket/dir1/dir2/file.txt").unwrap();
        assert!(matches!(result, ParsedPath::Remote(_)));
        if let ParsedPath::Remote(r) = result {
            assert_eq!(r.alias, "myalias");
            assert_eq!(r.bucket, "bucket");
            assert_eq!(r.key, "dir1/dir2/file.txt");
        }
    }

    #[test]
    fn test_download_progress_created_for_large_transfer() {
        let output_config = OutputConfig::default();
        let mut progress = None;

        update_download_progress(
            &mut progress,
            &output_config,
            1024,
            Some(DOWNLOAD_PROGRESS_THRESHOLD),
        );

        let progress = progress.expect("large download should create progress bar");
        assert!(progress.is_visible());
        progress.finish_and_clear();
    }

    #[test]
    fn test_download_progress_skips_small_transfer() {
        let output_config = OutputConfig::default();
        let mut progress = None;

        update_download_progress(
            &mut progress,
            &output_config,
            1024,
            Some(DOWNLOAD_PROGRESS_THRESHOLD - 1),
        );

        assert!(progress.is_none());
    }

    #[test]
    fn test_download_progress_skips_unknown_total_size() {
        let output_config = OutputConfig::default();
        let mut progress = None;

        update_download_progress(&mut progress, &output_config, 1024, None);

        assert!(progress.is_none());
    }

    #[test]
    fn test_download_progress_respects_no_progress_config() {
        let output_config = OutputConfig {
            no_progress: true,
            ..Default::default()
        };
        let mut progress = None;

        update_download_progress(
            &mut progress,
            &output_config,
            1024,
            Some(DOWNLOAD_PROGRESS_THRESHOLD),
        );

        let progress = progress.expect("large download should create progress state");
        assert!(!progress.is_visible());
    }

    #[test]
    fn download_relative_path_preserves_safe_nested_keys() {
        let relative = safe_download_relative_path("reports/2026/july/data.csv", "reports/")
            .expect("safe key should resolve");

        assert_eq!(
            relative,
            PathBuf::from("2026").join("july").join("data.csv")
        );
    }

    #[test]
    fn download_relative_path_rejects_traversal_and_absolute_keys() {
        for key in [
            "reports/../../escaped",
            "reports/..\\..\\escaped",
            "reports/C:/escaped",
            "reports/safe:stream",
            "reports/CON.txt",
            "reports/trailing.",
            "/absolute/path",
        ] {
            assert!(
                safe_download_relative_path(key, "reports/").is_err(),
                "unsafe key should be rejected: {key}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_destination_rejects_existing_symlink_components() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("create destination root");
        let outside = tempfile::tempdir().expect("create outside directory");
        symlink(outside.path(), root.path().join("linked")).expect("create test symlink");

        let result =
            safe_download_destination(root.path(), &PathBuf::from("linked/file.txt")).await;

        assert!(result.is_err());
    }

    #[test]
    fn test_select_upload_content_type_uses_guess_for_small_files() {
        let selected =
            select_upload_content_type(None, Some("text/plain"), MULTIPART_THRESHOLD - 1);

        assert_eq!(selected, Some("text/plain"));
    }

    #[test]
    fn test_select_upload_content_type_skips_guess_for_multipart_files() {
        let selected =
            select_upload_content_type(None, Some("text/plain"), MULTIPART_THRESHOLD + 1);

        assert_eq!(selected, None);
    }

    #[test]
    fn test_select_upload_content_type_uses_guess_at_multipart_boundary() {
        let selected = select_upload_content_type(None, Some("text/plain"), MULTIPART_THRESHOLD);

        assert_eq!(selected, Some("text/plain"));
    }

    #[test]
    fn test_select_upload_content_type_keeps_explicit_type_for_multipart_files() {
        let selected = select_upload_content_type(
            Some("application/octet-stream"),
            Some("text/plain"),
            MULTIPART_THRESHOLD + 1,
        );

        assert_eq!(selected, Some("application/octet-stream"));
    }

    #[test]
    fn test_parse_cp_path_prefers_existing_local_path_when_alias_missing() {
        let (alias_manager, temp_dir) = temp_alias_manager();
        let full = temp_dir.path().join("issue-2094-local").join("file.txt");
        let full_str = full.to_string_lossy().to_string();

        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&full, b"test").expect("write local file");

        let parsed = parse_cp_path(&full_str, Some(&alias_manager)).expect("parse path");
        assert!(matches!(parsed, ParsedPath::Local(_)));
    }

    #[test]
    fn test_parse_cp_path_keeps_remote_when_alias_exists() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        alias_manager
            .set(Alias::new("target", "http://localhost:9000", "a", "b"))
            .expect("set alias");

        let parsed = parse_cp_path("target/bucket/file.txt", Some(&alias_manager))
            .expect("parse remote path");
        assert!(matches!(parsed, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_parse_cp_path_keeps_remote_when_local_missing() {
        let (alias_manager, _temp_dir) = temp_alias_manager();
        let parsed = parse_cp_path("missing/bucket/file.txt", Some(&alias_manager))
            .expect("parse remote path");
        assert!(matches!(parsed, ParsedPath::Remote(_)));
    }

    #[test]
    fn test_cp_args_defaults() {
        let args = CpArgs {
            source: "src".to_string(),
            target: "dst".to_string(),
            recursive: false,
            preserve: false,
            continue_on_error: false,
            overwrite: true,
            dry_run: false,
            storage_class: None,
            content_type: None,
            enc_s3: Vec::new(),
            enc_kms: Vec::new(),
        };
        assert!(args.overwrite);
        assert!(!args.recursive);
        assert!(!args.dry_run);
    }

    #[test]
    fn parse_enc_kms_target_requires_equals_separator() {
        let error = parse_kms_target("local/bucket/file.txt").expect_err("missing key separator");
        assert!(error.contains("Expected TARGET=KMS_KEY_ID"));
    }

    #[test]
    fn destination_encryption_rejects_local_targets() {
        let error = parse_destination_encryption(
            &[String::from("./local.txt")],
            &[],
            &ParsedPath::Local(std::path::PathBuf::from("./local.txt")),
        )
        .expect_err("local target should be rejected");

        assert!(error.contains("must reference a remote destination"));
    }

    #[test]
    fn destination_encryption_detects_conflicting_flags_for_same_target() {
        let target = ParsedPath::Remote(RemotePath::new("local", "bucket", "file.txt"));
        let error = parse_destination_encryption(
            &[String::from("local/bucket/file.txt")],
            &[String::from("local/bucket/file.txt=kms-key")],
            &target,
        )
        .expect_err("same target conflict should fail");

        assert!(error.contains("cannot target the same destination"));
    }

    #[test]
    fn destination_encryption_rejects_unmatched_s3_target() {
        let target = ParsedPath::Remote(RemotePath::new("local", "bucket", "file.txt"));
        let error =
            parse_destination_encryption(&[String::from("local/bucket/typo.txt")], &[], &target)
                .expect_err("unmatched s3 target should fail");

        assert!(error.contains("must exactly match the remote destination"));
    }

    #[test]
    fn destination_encryption_rejects_unmatched_kms_target() {
        let target = ParsedPath::Remote(RemotePath::new("local", "bucket", "file.txt"));
        let error = parse_destination_encryption(
            &[],
            &[String::from("local/bucket/typo.txt=kms-key")],
            &target,
        )
        .expect_err("unmatched kms target should fail");

        assert!(error.contains("must exactly match the remote destination"));
    }

    #[test]
    fn test_cp_output_serialization() {
        let output = CpOutput {
            status: "success",
            source: "src/file.txt".to_string(),
            target: "dst/file.txt".to_string(),
            size_bytes: Some(1024),
            size_human: Some("1 KiB".to_string()),
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"size_bytes\":1024"));
    }

    #[test]
    fn test_cp_output_skips_none_fields() {
        let output = CpOutput {
            status: "success",
            source: "src".to_string(),
            target: "dst".to_string(),
            size_bytes: None,
            size_human: None,
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(!json.contains("size_bytes"));
        assert!(!json.contains("size_human"));
    }
}
