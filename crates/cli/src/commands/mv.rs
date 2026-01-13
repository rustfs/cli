//! mv command - Move objects
//!
//! Moves objects between locations (copy + delete).

use clap::Args;
use rc_core::{parse_path, AliasManager, ObjectStore as _, ParsedPath, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

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

async fn move_local_to_s3(
    src: &std::path::Path,
    dst: &RemotePath,
    args: &MvArgs,
    formatter: &Formatter,
) -> ExitCode {
    use crate::commands::cp;

    // First, copy local to S3
    let cp_args = cp::CpArgs {
        source: src.to_string_lossy().to_string(),
        target: format!("{}/{}/{}", dst.alias, dst.bucket, dst.key),
        recursive: args.recursive,
        preserve: false,
        continue_on_error: args.continue_on_error,
        overwrite: true,
        dry_run: args.dry_run,
        storage_class: None,
        content_type: None,
    };

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
        if src.is_file() {
            if let Err(e) = std::fs::remove_file(src) {
                formatter.error(&format!("Failed to delete local file: {e}"));
                return ExitCode::GeneralError;
            }
        } else if src.is_dir() && args.recursive {
            if let Err(e) = std::fs::remove_dir_all(src) {
                formatter.error(&format!("Failed to delete local directory: {e}"));
                return ExitCode::GeneralError;
            }
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

    // First, copy S3 to local
    let cp_args = cp::CpArgs {
        source: format!("{}/{}/{}", src.alias, src.bucket, src.key),
        target: dst.to_string_lossy().to_string(),
        recursive: args.recursive,
        preserve: false,
        continue_on_error: args.continue_on_error,
        overwrite: true,
        dry_run: args.dry_run,
        storage_class: None,
        content_type: None,
    };

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

async fn move_s3_to_s3(
    src: &RemotePath,
    dst: &RemotePath,
    args: &MvArgs,
    formatter: &Formatter,
) -> ExitCode {
    // For S3-to-S3, we need same alias for server-side copy
    if src.alias != dst.alias {
        formatter.error("Cross-alias S3-to-S3 move not yet supported.");
        return ExitCode::UnsupportedFeature;
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

    let src_display = format!("{}/{}/{}", src.alias, src.bucket, src.key);
    let dst_display = format!("{}/{}/{}", dst.alias, dst.bucket, dst.key);

    if args.dry_run {
        formatter.println(&format!("Would move: {src_display} -> {dst_display}"));
        return ExitCode::Success;
    }

    // Copy
    match client.copy_object(src, dst).await {
        Ok(info) => {
            // Delete source
            if let Err(e) = client.delete_object(src).await {
                formatter.error(&format!("Copied but failed to delete source: {e}"));
                return ExitCode::GeneralError;
            }

            if formatter.is_json() {
                let output = MvOutput {
                    status: "success",
                    source: src_display,
                    target: dst_display,
                    size_bytes: info.size_bytes,
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
                formatter.error(&format!("Failed to move: {e}"));
                ExitCode::NetworkError
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_paths() {
        // Local path
        let local = parse_path("./file.txt").unwrap();
        assert!(matches!(local, ParsedPath::Local(_)));

        // Remote path
        let remote = parse_path("myalias/bucket/file.txt").unwrap();
        assert!(matches!(remote, ParsedPath::Remote(_)));
    }
}
