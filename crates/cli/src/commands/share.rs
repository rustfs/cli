//! share command - Generate presigned URLs
//!
//! Creates time-limited URLs for sharing objects without authentication.

use clap::Args;
use rc_core::{AliasManager, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

/// Generate presigned URLs for sharing objects
#[derive(Args, Debug)]
pub struct ShareArgs {
    /// Path to the object (alias/bucket/key)
    pub path: String,

    /// Expiration time (e.g., 1h, 1d, 7d). Default: 7d
    #[arg(short, long, default_value = "7d")]
    pub expire: String,

    /// Generate upload URL instead of download URL
    #[arg(long)]
    pub upload: bool,

    /// Content-Type for upload URL
    #[arg(long)]
    pub content_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct ShareOutput {
    url: String,
    path: String,
    #[serde(rename = "type")]
    url_type: String,
    expires_in: String,
    expires_secs: u64,
}

/// Execute the share command
pub async fn execute(args: ShareArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    // Parse path
    let (alias_name, bucket, key) = match parse_share_path(&args.path) {
        Ok(p) => p,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    // Parse expiration
    let expires_secs = match parse_expiration(&args.expire) {
        Ok(secs) => secs,
        Err(e) => {
            formatter.error(&e);
            return ExitCode::UsageError;
        }
    };

    // Validate expiration (max 7 days for most S3-compatible services)
    if expires_secs > 604800 {
        formatter.error("Expiration cannot exceed 7 days (604800 seconds)");
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

    let remote_path = RemotePath::new(&alias_name, &bucket, &key);

    // For download URLs, verify object exists
    if !args.upload && client.head_object(&remote_path).await.is_err() {
        formatter.error(&format!("Object not found: {}", args.path));
        return ExitCode::NotFound;
    }

    // Generate presigned URL
    let url = if args.upload {
        match client
            .presign_put(&remote_path, expires_secs, args.content_type.as_deref())
            .await
        {
            Ok(url) => url,
            Err(e) => {
                formatter.error(&format!("Failed to generate upload URL: {e}"));
                return ExitCode::NetworkError;
            }
        }
    } else {
        match client.presign_get(&remote_path, expires_secs).await {
            Ok(url) => url,
            Err(e) => {
                formatter.error(&format!("Failed to generate download URL: {e}"));
                return ExitCode::NetworkError;
            }
        }
    };

    let url_type = if args.upload { "upload" } else { "download" };
    let expires_human = format_duration(expires_secs);

    if formatter.is_json() {
        let output = ShareOutput {
            url,
            path: args.path.clone(),
            url_type: url_type.to_string(),
            expires_in: expires_human,
            expires_secs,
        };
        formatter.json(&output);
    } else {
        let styled_type = formatter.style_key(url_type);
        let styled_url = formatter.style_url(&url);
        let styled_expires = formatter.style_date(&expires_human);
        formatter.println(&format!("Share URL ({styled_type}):"));
        formatter.println(&styled_url);
        formatter.println("");
        formatter.println(&format!("Expires in: {styled_expires}"));
        if args.upload {
            formatter.println("");
            formatter.println("Upload with: curl -X PUT -T <file> \"<url>\"");
        }
    }

    ExitCode::Success
}

/// Parse expiration string (e.g., "1h", "1d", "7d")
fn parse_expiration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("Expiration cannot be empty".to_string());
    }

    let (num_str, suffix) = if s.ends_with(|c: char| c.is_ascii_alphabetic()) {
        let idx = s.len() - 1;
        (&s[..idx], &s[idx..])
    } else {
        (s, "s") // Default to seconds
    };

    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("Invalid expiration number: {num_str}"))?;

    let multiplier = match suffix.to_lowercase().as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        _ => return Err(format!("Unknown expiration suffix: {suffix}")),
    };

    let seconds = num
        .checked_mul(multiplier)
        .ok_or_else(|| "Expiration is too large".to_string())?;

    Ok(seconds)
}

/// Format duration in human-readable form
fn format_duration(secs: u64) -> String {
    if secs >= 86400 {
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days} day(s)")
        }
    } else if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins > 0 {
            format!("{hours}h {mins}m")
        } else {
            format!("{hours} hour(s)")
        }
    } else if secs >= 60 {
        let mins = secs / 60;
        format!("{mins} minute(s)")
    } else {
        format!("{secs} second(s)")
    }
}

/// Parse share path into (alias, bucket, key)
fn parse_share_path(path: &str) -> Result<(String, String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(3, '/').collect();

    if parts.len() < 3 || parts[2].is_empty() {
        return Err("Object key is required (alias/bucket/key)".to_string());
    }

    Ok((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_expiration() {
        assert_eq!(parse_expiration("60").unwrap(), 60);
        assert_eq!(parse_expiration("60s").unwrap(), 60);
        assert_eq!(parse_expiration("5m").unwrap(), 300);
        assert_eq!(parse_expiration("1h").unwrap(), 3600);
        assert_eq!(parse_expiration("1d").unwrap(), 86400);
        assert_eq!(parse_expiration("7d").unwrap(), 604800);
    }

    #[test]
    fn test_parse_expiration_errors() {
        assert!(parse_expiration("").is_err());
        assert!(parse_expiration("abc").is_err());
        assert!(parse_expiration("1x").is_err());
        assert!(parse_expiration("30500568904944w").is_err());
    }

    #[test]
    fn test_parse_share_path() {
        let (alias, bucket, key) = parse_share_path("myalias/mybucket/path/to/file.txt").unwrap();
        assert_eq!(alias, "myalias");
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "path/to/file.txt");
    }

    #[test]
    fn test_parse_share_path_errors() {
        assert!(parse_share_path("").is_err());
        assert!(parse_share_path("myalias").is_err());
        assert!(parse_share_path("myalias/mybucket").is_err());
        assert!(parse_share_path("myalias/mybucket/").is_err());
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(30), "30 second(s)");
        assert_eq!(format_duration(120), "2 minute(s)");
        assert_eq!(format_duration(3600), "1 hour(s)");
        assert_eq!(format_duration(86400), "1 day(s)");
        assert_eq!(format_duration(90000), "1d 1h");
    }
}
