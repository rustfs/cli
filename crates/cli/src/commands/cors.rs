//! cors command - Manage bucket CORS configuration
//!
//! List, replace, or remove bucket CORS rules.

use clap::{Args, Subcommand};
use quick_xml::de::from_str as from_xml_str;
use rc_core::{AliasManager, CorsConfiguration, CorsRule, ObjectStore as _};
use rc_s3::S3Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const CORS_AFTER_HELP: &str = "\
Examples:
  rc bucket cors list local/my-bucket
  rc bucket cors get local/my-bucket
  rc bucket cors set local/my-bucket cors.xml
  rc bucket cors set local/my-bucket -
  rc bucket cors set local/my-bucket --file cors.json
  rc cors remove local/my-bucket";

const VALID_CORS_METHODS: [&str; 5] = ["GET", "PUT", "HEAD", "POST", "DELETE"];

/// Manage bucket CORS configuration
#[derive(Args, Debug)]
#[command(after_help = CORS_AFTER_HELP)]
pub struct CorsArgs {
    #[command(subcommand)]
    pub command: CorsCommands,
}

#[derive(Subcommand, Debug)]
pub enum CorsCommands {
    /// List bucket CORS rules
    #[command(alias = "get")]
    List(BucketArg),

    /// Replace bucket CORS rules from a JSON or XML file
    Set(SetCorsArgs),

    /// Remove bucket CORS rules
    Remove(BucketArg),
}

#[derive(Args, Debug)]
pub struct BucketArg {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct SetCorsArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Path to the CORS configuration file, or '-' to read from stdin
    pub source: Option<String>,

    /// Path to the CORS configuration file, or '-' to read from stdin
    #[arg(long, conflicts_with = "source")]
    pub file: Option<String>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct CorsListOutput {
    bucket: String,
    rules: Vec<CorsRule>,
}

#[derive(Debug, Serialize)]
struct CorsOperationOutput {
    bucket: String,
    rule_count: usize,
    action: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CorsConfigurationXml {
    #[serde(rename = "CORSRule", default)]
    rules: Vec<CorsRuleXml>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CorsRuleXml {
    #[serde(rename = "ID")]
    id: Option<String>,
    #[serde(rename = "AllowedOrigin", default)]
    allowed_origins: Vec<String>,
    #[serde(rename = "AllowedMethod", default)]
    allowed_methods: Vec<String>,
    #[serde(rename = "AllowedHeader", default)]
    allowed_headers: Vec<String>,
    #[serde(rename = "ExposeHeader", default)]
    expose_headers: Vec<String>,
    max_age_seconds: Option<i32>,
}

/// Execute the cors command
pub async fn execute(args: CorsArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        CorsCommands::List(args) => execute_list(args, output_config).await,
        CorsCommands::Set(args) => execute_set(args, output_config).await,
        CorsCommands::Remove(args) => execute_remove(args, output_config).await,
    }
}

async fn execute_list(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the cors command.",
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_cors(&bucket).await {
        Ok(rules) => {
            if formatter.is_json() {
                formatter.json(&CorsListOutput { bucket, rules });
            } else if rules.is_empty() {
                formatter.println("No CORS rules found.");
            } else {
                formatter.println(&format!("Bucket CORS rules for '{bucket}':"));
                for (index, rule) in rules.iter().enumerate() {
                    formatter.println(&format!("  Rule {}:", index + 1));
                    if let Some(id) = &rule.id {
                        formatter.println(&format!("    ID: {id}"));
                    }
                    formatter.println(&format!(
                        "    Allowed origins: {}",
                        rule.allowed_origins.join(", ")
                    ));
                    formatter.println(&format!(
                        "    Allowed methods: {}",
                        rule.allowed_methods.join(", ")
                    ));
                    if let Some(headers) = &rule.allowed_headers {
                        formatter.println(&format!("    Allowed headers: {}", headers.join(", ")));
                    }
                    if let Some(headers) = &rule.expose_headers {
                        formatter.println(&format!("    Expose headers: {}", headers.join(", ")));
                    }
                    if let Some(seconds) = rule.max_age_seconds {
                        formatter.println(&format!("    Max age seconds: {seconds}"));
                    }
                }
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to get bucket CORS: {error}"),
        ),
    }
}

async fn execute_set(args: SetCorsArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the cors command.",
            );
        }
    };

    let source = match cors_input_source(&args) {
        Ok(source) => source,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Provide a CORS config file path, or '-' to read the configuration from stdin.",
            );
        }
    };

    let file_contents = match read_cors_source(&source).await {
        Ok(contents) => contents,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read CORS source '{}': {error}", source),
            );
        }
    };

    let config = match parse_cors_configuration(&file_contents) {
        Ok(config) => config,
        Err(error) => {
            return formatter.fail(
                ExitCode::UsageError,
                &format!("Invalid CORS configuration: {error}"),
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.set_bucket_cors(&bucket, config.rules.clone()).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&CorsOperationOutput {
                    bucket,
                    rule_count: config.rules.len(),
                    action: "set".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Applied {} bucket CORS rule(s).",
                    config.rules.len()
                ));
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to set bucket CORS: {error}"),
        ),
    }
}

async fn execute_remove(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the cors command.",
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.delete_bucket_cors(&bucket).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&CorsOperationOutput {
                    bucket,
                    rule_count: 0,
                    action: "removed".to_string(),
                });
            } else {
                formatter.success("Bucket CORS configuration removed successfully.");
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to remove bucket CORS: {error}"),
        ),
    }
}

async fn setup_client(
    alias_name: &str,
    bucket: &str,
    force: bool,
    formatter: &Formatter,
) -> Result<S3Client, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to load aliases: {error}"),
            ));
        }
    };

    let alias = match alias_manager.get(alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            return Err(formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Alias '{alias_name}' not found"),
                "Run `rc alias list` to inspect configured aliases or add one with `rc alias set ...`.",
            ));
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to create S3 client: {error}"),
            ));
        }
    };

    let caps = match client.capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            if force {
                rc_core::Capabilities::default()
            } else {
                return Err(formatter.fail(
                    ExitCode::NetworkError,
                    &format!("Failed to detect capabilities: {error}"),
                ));
            }
        }
    };

    if !force && !caps.cors {
        return Err(formatter.fail_with_suggestion(
            ExitCode::UnsupportedFeature,
            "Backend does not support bucket CORS. Use --force to attempt anyway.",
            "Retry with --force only if you know the backend supports bucket CORS.",
        ));
    }

    match client.bucket_exists(bucket).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Bucket '{bucket}' does not exist"),
                "Check the bucket path and retry the cors command.",
            ));
        }
        Err(error) => {
            return Err(formatter.fail(
                ExitCode::NetworkError,
                &format!("Failed to check bucket: {error}"),
            ));
        }
    }

    Ok(client)
}

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(2, '/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }

    let bucket = parts[1].trim_end_matches('/');
    if bucket.is_empty() {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }

    Ok((parts[0].to_string(), bucket.to_string()))
}

fn parse_cors_configuration(contents: &str) -> Result<CorsConfiguration, String> {
    let trimmed = contents.trim_start();
    let mut config = if trimmed.starts_with('<') {
        parse_cors_configuration_xml(contents)?
    } else {
        serde_json::from_str(contents).map_err(|error| error.to_string())?
    };

    if config.rules.is_empty() {
        return Err("configuration must contain at least one rule".to_string());
    }

    for (index, rule) in config.rules.iter_mut().enumerate() {
        if rule.allowed_origins.is_empty() {
            return Err(format!(
                "rule {} must include at least one allowed origin",
                index + 1
            ));
        }
        if rule.allowed_methods.is_empty() {
            return Err(format!(
                "rule {} must include at least one allowed method",
                index + 1
            ));
        }

        rule.allowed_origins = trim_non_empty_values(&rule.allowed_origins)
            .ok_or_else(|| format!("rule {} contains an empty allowed origin", index + 1))?;
        rule.allowed_methods = trim_non_empty_values(&rule.allowed_methods)
            .ok_or_else(|| format!("rule {} contains an empty allowed method", index + 1))?
            .into_iter()
            .map(|method| method.to_ascii_uppercase())
            .collect();

        if let Some(invalid_method) = rule
            .allowed_methods
            .iter()
            .find(|method| !VALID_CORS_METHODS.contains(&method.as_str()))
        {
            return Err(format!(
                "rule {} contains unsupported method '{}'",
                index + 1,
                invalid_method
            ));
        }

        rule.allowed_headers = trim_optional_values(rule.allowed_headers.take());
        rule.expose_headers = trim_optional_values(rule.expose_headers.take());
    }

    Ok(config)
}

fn parse_cors_configuration_xml(contents: &str) -> Result<CorsConfiguration, String> {
    let config: CorsConfigurationXml = from_xml_str(contents).map_err(|error| error.to_string())?;
    Ok(CorsConfiguration {
        rules: config
            .rules
            .into_iter()
            .map(|rule| CorsRule {
                id: rule.id,
                allowed_origins: rule.allowed_origins,
                allowed_methods: rule.allowed_methods,
                allowed_headers: trim_optional_values(Some(rule.allowed_headers)),
                expose_headers: trim_optional_values(Some(rule.expose_headers)),
                max_age_seconds: rule.max_age_seconds,
            })
            .collect(),
    })
}

fn cors_input_source(args: &SetCorsArgs) -> Result<String, String> {
    match (args.source.as_deref(), args.file.as_deref()) {
        (Some(source), None) => Ok(source.to_string()),
        (None, Some(file)) => Ok(file.to_string()),
        (None, None) => Err("CORS configuration source is required".to_string()),
        (Some(_), Some(_)) => {
            Err("Specify either a positional source or --file, not both".to_string())
        }
    }
}

async fn read_cors_source(source: &str) -> Result<String, String> {
    if source == "-" {
        let mut stdin = tokio::io::stdin();
        let mut bytes = Vec::new();
        stdin
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        String::from_utf8(bytes).map_err(|error| error.to_string())
    } else {
        tokio::fs::read_to_string(source)
            .await
            .map_err(|error| error.to_string())
    }
}

fn trim_non_empty_values(values: &[String]) -> Option<Vec<String>> {
    let trimmed: Vec<String> = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();

    if trimmed.len() == values.len() {
        Some(trimmed)
    } else {
        None
    }
}

fn trim_optional_values(values: Option<Vec<String>>) -> Option<Vec<String>> {
    values.and_then(|values| {
        let trimmed: Vec<String> = values
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bucket_path_success() {
        let (alias, bucket) = parse_bucket_path("local/my-bucket").expect("should parse");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");

        let (alias, bucket) = parse_bucket_path("local/my-bucket/").expect("should parse");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "my-bucket");
    }

    #[test]
    fn test_parse_bucket_path_error() {
        assert!(parse_bucket_path("").is_err());
        assert!(parse_bucket_path("local").is_err());
        assert!(parse_bucket_path("/bucket").is_err());
        assert!(parse_bucket_path("local///").is_err());
    }

    #[test]
    fn test_parse_cors_configuration_normalizes_methods() {
        let config = parse_cors_configuration(
            r#"{
                "rules": [
                    {
                        "id": "web-app",
                        "allowedOrigins": [" https://app.example.com "],
                        "allowedMethods": ["get", "post"],
                        "allowedHeaders": [" Authorization "],
                        "maxAgeSeconds": 600
                    }
                ]
            }"#,
        )
        .expect("parse config");

        assert_eq!(config.rules.len(), 1);
        assert_eq!(
            config.rules[0].allowed_origins,
            vec!["https://app.example.com".to_string()]
        );
        assert_eq!(
            config.rules[0].allowed_methods,
            vec!["GET".to_string(), "POST".to_string()]
        );
        assert_eq!(
            config.rules[0].allowed_headers,
            Some(vec!["Authorization".to_string()])
        );
    }

    #[test]
    fn test_parse_cors_configuration_rejects_empty_rules() {
        let error = parse_cors_configuration(r#"{"rules":[]}"#).expect_err("empty rules");
        assert!(error.contains("at least one rule"));
    }

    #[test]
    fn test_parse_cors_configuration_rejects_invalid_method() {
        let error = parse_cors_configuration(
            r#"{
                "rules": [
                    {
                        "allowedOrigins": ["*"],
                        "allowedMethods": ["PATCH"]
                    }
                ]
            }"#,
        )
        .expect_err("invalid method");

        assert!(error.contains("unsupported method"));
    }

    #[test]
    fn test_parse_cors_configuration_accepts_xml() {
        let config = parse_cors_configuration(
            r#"
<CORSConfiguration>
  <CORSRule>
    <ID>mc-rule</ID>
    <AllowedOrigin>https://console.example.com</AllowedOrigin>
    <AllowedMethod>GET</AllowedMethod>
    <AllowedMethod>POST</AllowedMethod>
    <AllowedHeader>*</AllowedHeader>
    <ExposeHeader>ETag</ExposeHeader>
    <MaxAgeSeconds>1200</MaxAgeSeconds>
  </CORSRule>
</CORSConfiguration>
"#,
        )
        .expect("parse xml config");

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].id.as_deref(), Some("mc-rule"));
        assert_eq!(
            config.rules[0].allowed_origins,
            vec!["https://console.example.com".to_string()]
        );
        assert_eq!(
            config.rules[0].allowed_methods,
            vec!["GET".to_string(), "POST".to_string()]
        );
        assert_eq!(config.rules[0].allowed_headers, Some(vec!["*".to_string()]));
    }

    #[test]
    fn test_cors_input_source_prefers_positional_argument() {
        let args = SetCorsArgs {
            path: "local/my-bucket".to_string(),
            source: Some("cors.xml".to_string()),
            file: None,
            force: false,
        };

        assert_eq!(cors_input_source(&args).as_deref(), Ok("cors.xml"));
    }

    #[test]
    fn test_cors_input_source_supports_legacy_file_flag() {
        let args = SetCorsArgs {
            path: "local/my-bucket".to_string(),
            source: None,
            file: Some("cors.json".to_string()),
            force: false,
        };

        assert_eq!(cors_input_source(&args).as_deref(), Ok("cors.json"));
    }

    #[test]
    fn test_cors_input_source_requires_source() {
        let args = SetCorsArgs {
            path: "local/my-bucket".to_string(),
            source: None,
            file: None,
            force: false,
        };

        assert!(cors_input_source(&args).is_err());
    }
}
