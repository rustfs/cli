//! pipe command - Stream stdin to S3
//!
//! Reads from stdin and uploads to S3. Useful for piping output from other commands.

use clap::Args;
use rc_core::{
    AliasManager, ObjectAttributes, ObjectEncryptionRequest, ObjectStore as _,
    ObjectWriteEncryption, ObjectWriteOptions, RemotePath,
};
use rc_s3::S3Client;
use serde::Serialize;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};
use crate::secret_input::resolve_secret_locator;

use super::cp::{exit_code_for_core_error, validate_destination_storage_class};

/// Stream stdin to an object
#[derive(Args)]
pub struct PipeArgs {
    /// Destination path (alias/bucket/key)
    pub target: String,

    /// Content type for the uploaded object
    #[arg(long, default_value = "application/octet-stream")]
    pub content_type: String,

    /// Storage class for the object
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Apply SSE-S3 to the upload target
    #[arg(long = "enc-s3", default_value = "false")]
    pub enc_s3: bool,

    /// Apply SSE-KMS to the upload target
    #[arg(long = "enc-kms")]
    pub enc_kms: Option<String>,

    /// Read a 32-byte SSE-C destination key from a protected file
    #[arg(long = "enc-c-key-file")]
    pub enc_c_key_file: Option<PathBuf>,

    /// Read a 32-byte SSE-C destination key from the named environment variable
    #[arg(long = "enc-c-key-env")]
    pub enc_c_key_env: Option<String>,
}

impl fmt::Debug for PipeArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PipeArgs { .. }")
    }
}

#[derive(Debug, Serialize)]
struct PipeOutput {
    status: &'static str,
    target: String,
    size_bytes: i64,
    size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
}

/// Execute the pipe command
pub async fn execute(args: PipeArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    if let Err(error) = validate_destination_storage_class(args.storage_class.as_deref()) {
        return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
    }
    let customer_key_locator =
        match resolve_secret_locator(args.enc_c_key_file.clone(), args.enc_c_key_env.clone()) {
            Ok(locator) => locator,
            Err(error) => {
                return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
            }
        };
    if customer_key_locator.is_some() && (args.enc_s3 || args.enc_kms.is_some()) {
        return formatter.fail(
            ExitCode::UsageError,
            "--enc-s3, --enc-kms, and SSE-C destination key options cannot be combined",
        );
    }
    if args.enc_s3 && args.enc_kms.is_some() {
        return formatter.fail(
            ExitCode::UsageError,
            "--enc-s3, --enc-kms, and SSE-C destination key options cannot be combined",
        );
    }
    let customer_key = match customer_key_locator
        .map(|locator| locator.load_customer_key())
        .transpose()
    {
        Ok(key) => key,
        Err(error) => {
            return formatter.fail(exit_code_for_core_error(&error), &error.to_string());
        }
    };
    let encryption = match (args.enc_s3, args.enc_kms.as_deref(), customer_key) {
        (true, None, None) => Some(ObjectWriteEncryption::Managed(
            ObjectEncryptionRequest::SseS3,
        )),
        (false, Some(key_id), None) => Some(ObjectWriteEncryption::Managed(
            ObjectEncryptionRequest::SseKms {
                key_id: key_id.to_string(),
            },
        )),
        (false, None, Some(key)) => Some(ObjectWriteEncryption::SseCustomer { key }),
        (false, None, None) => None,
        _ => unreachable!("encryption combinations are validated before loading the key"),
    };

    // Parse the target path
    let (alias_name, bucket, key) = match parse_pipe_path(&args.target) {
        Ok(parsed) => parsed,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    if key.is_empty() {
        formatter.error("Object key is required for pipe command.");
        return ExitCode::UsageError;
    }

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

    // Read from stdin
    let mut buffer = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buffer) {
        formatter.error(&format!("Failed to read from stdin: {e}"));
        return ExitCode::GeneralError;
    }

    let size = buffer.len() as i64;
    let target = RemotePath::new(&alias_name, &bucket, &key);
    let target_display = format!("{alias_name}/{bucket}/{key}");

    // Upload
    let options = ObjectWriteOptions {
        attributes: Some(ObjectAttributes {
            content_type: Some(args.content_type.clone()),
            ..ObjectAttributes::default()
        }),
        storage_class: args.storage_class.clone(),
        encryption,
        ..ObjectWriteOptions::default()
    };
    match client
        .put_object_with_options(&target, buffer, &options)
        .await
    {
        Ok(info) => {
            if formatter.is_json() {
                let output = PipeOutput {
                    status: "success",
                    target: target_display,
                    size_bytes: size,
                    size_human: humansize::format_size(size as u64, humansize::BINARY),
                    etag: info.etag,
                };
                formatter.json(&output);
            } else {
                formatter.success(&format!(
                    "Uploaded to {target_display} ({})",
                    humansize::format_size(size as u64, humansize::BINARY)
                ));
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to upload: {e}"));
            exit_code_for_core_error(&e)
        }
    }
}

/// Parse pipe path into (alias, bucket, key)
fn parse_pipe_path(path: &str) -> Result<(String, String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    if parts.len() < 3 {
        return Err(format!(
            "Invalid path format: '{path}'. Expected: alias/bucket/key"
        ));
    }

    let alias = parts[0].to_string();
    let bucket = parts[1].to_string();
    let key = parts[2].to_string();

    if bucket.is_empty() {
        return Err("Bucket name cannot be empty".to_string());
    }

    if key.is_empty() {
        return Err("Object key cannot be empty".to_string());
    }

    Ok((alias, bucket, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pipe_path_valid() {
        let (alias, bucket, key) = parse_pipe_path("myalias/mybucket/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "file.txt");
    }

    #[test]
    fn test_parse_pipe_path_with_prefix() {
        let (alias, bucket, key) = parse_pipe_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_pipe_path_no_key() {
        assert!(parse_pipe_path("myalias/mybucket").is_err());
    }

    #[test]
    fn test_parse_pipe_path_empty() {
        assert!(parse_pipe_path("").is_err());
    }

    #[tokio::test]
    async fn pipe_conflicting_encryption_flags_return_usage_error() {
        let args = PipeArgs {
            target: "local/bucket/file.txt".to_string(),
            content_type: "application/octet-stream".to_string(),
            storage_class: None,
            enc_s3: true,
            enc_kms: Some("kms-key".to_string()),
            enc_c_key_file: None,
            enc_c_key_env: None,
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn pipe_storage_class_errors_have_distinct_exit_codes_before_io() {
        for (storage_class, expected) in [
            ("NOT_A_CLASS", ExitCode::UsageError),
            ("STANDARD_IA", ExitCode::UnsupportedFeature),
        ] {
            let args = PipeArgs {
                target: "local/bucket/file.txt".to_string(),
                content_type: "application/octet-stream".to_string(),
                storage_class: Some(storage_class.to_string()),
                enc_s3: false,
                enc_kms: None,
                enc_c_key_file: None,
                enc_c_key_env: None,
            };

            assert_eq!(execute(args, OutputConfig::default()).await, expected);
        }
    }
}
