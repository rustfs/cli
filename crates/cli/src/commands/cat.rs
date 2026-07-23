//! cat command - Display object contents
//!
//! Outputs the entire content of an object to stdout.

use clap::Args;
use rc_core::{AliasManager, ObjectReadOptions, ObjectStore, RemotePath};
use rc_s3::S3Client;

use crate::commands::{exit_code_for_core_error, validate_version_selector};
use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Display object contents
#[derive(Args, Debug)]
pub struct CatArgs {
    /// Object path (alias/bucket/key)
    pub path: String,

    /// Encrypt/decrypt with the given key (base64 encoded)
    #[arg(long)]
    pub enc_key: Option<String>,

    /// Rewind to a specific time
    #[arg(long)]
    pub rewind: Option<String>,

    /// Specific version ID to retrieve
    #[arg(long)]
    pub version_id: Option<String>,
}

/// Execute the cat command
pub async fn execute(args: CatArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if let Err(error) =
        validate_version_selector(args.version_id.as_deref(), args.rewind.as_deref())
    {
        return formatter.fail(ExitCode::UsageError, &error);
    }
    if args.enc_key.is_some() {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--enc-key is not implemented for cat",
        );
    }
    if args.rewind.is_some() {
        return formatter.fail(
            ExitCode::UnsupportedFeature,
            "--rewind is not implemented for cat",
        );
    }
    let read_options = match ObjectReadOptions::for_version(args.version_id.clone()) {
        Ok(options) => options,
        Err(error) => return formatter.fail(ExitCode::UsageError, &error.to_string()),
    };

    // Parse the path
    let (alias_name, bucket, key) = match parse_cat_path(&args.path) {
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

    let path = RemotePath::new(&alias_name, &bucket, &key);

    // Get object content
    let mut stdout = tokio::io::stdout();
    match ObjectStore::write_object_to_with_options(
        &client,
        &path,
        &read_options,
        &mut stdout,
        None,
    )
    .await
    {
        Ok(_) => ExitCode::Success,
        Err(e) => {
            let exit_code = exit_code_for_core_error(&e);
            formatter.error_with_code(exit_code, &format!("Failed to read {}: {e}", args.path));
            exit_code
        }
    }
}

/// Parse cat path into (alias, bucket, key)
fn parse_cat_path(path: &str) -> Result<(String, String, String), String> {
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
    fn test_parse_cat_path_valid() {
        let (alias, bucket, key) = parse_cat_path("myalias/mybucket/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "file.txt");
    }

    #[test]
    fn test_parse_cat_path_with_prefix() {
        let (alias, bucket, key) = parse_cat_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_cat_path_no_key() {
        assert!(parse_cat_path("myalias/mybucket").is_err());
    }

    #[test]
    fn test_parse_cat_path_empty() {
        assert!(parse_cat_path("").is_err());
    }
}
