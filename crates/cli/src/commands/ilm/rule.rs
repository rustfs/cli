//! ilm rule command - Manage bucket lifecycle rules
//!
//! Add, edit, list, remove, export, and import lifecycle rules on a bucket.

use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table};
use rc_core::{
    AliasManager, LifecycleConfiguration, LifecycleExpiration, LifecycleRule, LifecycleRuleStatus,
    LifecycleTransition, NoncurrentVersionExpiration, NoncurrentVersionTransition,
    ObjectStore as _,
};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

#[derive(Subcommand, Debug)]
pub enum RuleCommands {
    /// Add a new lifecycle rule to a bucket
    Add(AddRuleArgs),

    /// Edit an existing lifecycle rule
    Edit(EditRuleArgs),

    /// List lifecycle rules on a bucket
    List(BucketArg),

    /// Remove lifecycle rules from a bucket
    Remove(RemoveRuleArgs),

    /// Export lifecycle rules as JSON
    Export(BucketArg),

    /// Import lifecycle rules from a JSON file
    Import(ImportRuleArgs),
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
pub struct AddRuleArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Expiration days for current versions
    #[arg(long)]
    pub expiry_days: Option<i32>,

    /// Expiration date for current versions (ISO 8601)
    #[arg(long)]
    pub expiry_date: Option<String>,

    /// Transition days for current versions
    #[arg(long)]
    pub transition_days: Option<i32>,

    /// Transition date for current versions (ISO 8601)
    #[arg(long)]
    pub transition_date: Option<String>,

    /// Target storage class for transition
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Expiration days for noncurrent versions
    #[arg(long)]
    pub noncurrent_expiry_days: Option<i32>,

    /// Transition days for noncurrent versions
    #[arg(long)]
    pub noncurrent_transition_days: Option<i32>,

    /// Storage class for noncurrent version transition
    #[arg(long)]
    pub noncurrent_transition_storage_class: Option<String>,

    /// Key prefix filter
    #[arg(long)]
    pub prefix: Option<String>,

    /// Remove expired delete markers
    #[arg(long)]
    pub expired_object_delete_marker: bool,

    /// Maximum number of noncurrent versions to retain
    #[arg(long)]
    pub newer_noncurrent_versions: Option<i32>,

    /// Create the rule in disabled state
    #[arg(long)]
    pub disable: bool,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct EditRuleArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// ID of the rule to edit
    #[arg(long)]
    pub id: String,

    /// Expiration days for current versions
    #[arg(long)]
    pub expiry_days: Option<i32>,

    /// Expiration date for current versions (ISO 8601)
    #[arg(long)]
    pub expiry_date: Option<String>,

    /// Transition days for current versions
    #[arg(long)]
    pub transition_days: Option<i32>,

    /// Transition date for current versions (ISO 8601)
    #[arg(long)]
    pub transition_date: Option<String>,

    /// Target storage class for transition
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Expiration days for noncurrent versions
    #[arg(long)]
    pub noncurrent_expiry_days: Option<i32>,

    /// Transition days for noncurrent versions
    #[arg(long)]
    pub noncurrent_transition_days: Option<i32>,

    /// Storage class for noncurrent version transition
    #[arg(long)]
    pub noncurrent_transition_storage_class: Option<String>,

    /// Key prefix filter
    #[arg(long)]
    pub prefix: Option<String>,

    /// Remove expired delete markers
    #[arg(long)]
    pub expired_object_delete_marker: Option<bool>,

    /// Maximum number of noncurrent versions to retain
    #[arg(long)]
    pub newer_noncurrent_versions: Option<i32>,

    /// Set the rule to disabled state
    #[arg(long)]
    pub disable: Option<bool>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RemoveRuleArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// ID of the rule to remove
    #[arg(long)]
    pub id: Option<String>,

    /// Remove all lifecycle rules
    #[arg(long)]
    pub all: bool,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ImportRuleArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Path to the JSON file containing lifecycle rules
    pub file: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct RuleListOutput {
    bucket: String,
    rules: Vec<LifecycleRule>,
}

#[derive(Debug, Serialize)]
struct RuleOperationOutput {
    bucket: String,
    rule_id: String,
    action: String,
}

/// Execute a rule subcommand
pub async fn execute(cmd: RuleCommands, output_config: OutputConfig) -> ExitCode {
    match cmd {
        RuleCommands::Add(args) => execute_add(args, output_config).await,
        RuleCommands::Edit(args) => execute_edit(args, output_config).await,
        RuleCommands::List(args) => execute_list(args, output_config).await,
        RuleCommands::Remove(args) => execute_remove(args, output_config).await,
        RuleCommands::Export(args) => execute_export(args, output_config).await,
        RuleCommands::Import(args) => execute_import(args, output_config).await,
    }
}

async fn execute_add(args: AddRuleArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if let Err(error) = validate_expired_delete_marker_inputs(
        args.expired_object_delete_marker,
        args.expiry_days.is_some() || args.expiry_date.is_some(),
        false,
    ) {
        formatter.error(&error);
        return ExitCode::UsageError;
    }

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    // Get existing rules
    let mut rules = match client.get_bucket_lifecycle(&bucket).await {
        Ok(rules) => rules,
        Err(error) => {
            formatter.error(&format!("Failed to get lifecycle rules: {error}"));
            return ExitCode::GeneralError;
        }
    };

    // Generate rule ID
    let rule_id = generate_rule_id();

    let status = if args.disable {
        LifecycleRuleStatus::Disabled
    } else {
        LifecycleRuleStatus::Enabled
    };

    // Build expiration
    let expiration = if args.expiry_days.is_some() || args.expiry_date.is_some() {
        Some(LifecycleExpiration {
            days: args.expiry_days,
            date: args.expiry_date,
        })
    } else {
        None
    };

    // Build transition
    let transition = match (
        &args.transition_days,
        &args.transition_date,
        &args.storage_class,
    ) {
        (Some(_), _, Some(sc)) | (_, Some(_), Some(sc)) => Some(LifecycleTransition {
            days: args.transition_days,
            date: args.transition_date.clone(),
            storage_class: sc.clone(),
        }),
        (Some(_), _, None) | (_, Some(_), None) => {
            formatter.error(
                "--storage-class is required when using --transition-days or --transition-date",
            );
            return ExitCode::UsageError;
        }
        _ => None,
    };

    // Build noncurrent version expiration
    let noncurrent_version_expiration =
        args.noncurrent_expiry_days
            .map(|days| NoncurrentVersionExpiration {
                noncurrent_days: days,
                newer_noncurrent_versions: args.newer_noncurrent_versions,
            });

    // Build noncurrent version transition
    let noncurrent_version_transition = match (
        args.noncurrent_transition_days,
        &args.noncurrent_transition_storage_class,
    ) {
        (Some(days), Some(sc)) => Some(NoncurrentVersionTransition {
            noncurrent_days: days,
            storage_class: sc.clone(),
        }),
        (Some(_), None) => {
            formatter.error("--noncurrent-transition-storage-class is required when using --noncurrent-transition-days");
            return ExitCode::UsageError;
        }
        _ => None,
    };

    let expired_object_delete_marker = if args.expired_object_delete_marker {
        Some(true)
    } else {
        None
    };

    let new_rule = LifecycleRule {
        id: rule_id.clone(),
        status,
        prefix: args.prefix,
        tags: None,
        expiration,
        transition,
        noncurrent_version_expiration,
        noncurrent_version_transition,
        abort_incomplete_multipart_upload_days: None,
        expired_object_delete_marker,
    };

    rules.push(new_rule);

    match client.set_bucket_lifecycle(&bucket, rules).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&RuleOperationOutput {
                    bucket,
                    rule_id,
                    action: "added".to_string(),
                });
            } else {
                formatter.success(&format!("Lifecycle rule '{rule_id}' added successfully."));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set lifecycle rules: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_edit(args: EditRuleArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let mut rules = match client.get_bucket_lifecycle(&bucket).await {
        Ok(rules) => rules,
        Err(error) => {
            formatter.error(&format!("Failed to get lifecycle rules: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let rule = match rules.iter_mut().find(|r| r.id == args.id) {
        Some(rule) => rule,
        None => {
            formatter.error(&format!("Rule '{}' not found", args.id));
            return ExitCode::NotFound;
        }
    };

    // Update status if requested
    if let Some(disable) = args.disable {
        rule.status = if disable {
            LifecycleRuleStatus::Disabled
        } else {
            LifecycleRuleStatus::Enabled
        };
    }

    // Update prefix if provided
    if args.prefix.is_some() {
        rule.prefix = args.prefix;
    }

    // Update expiration if provided
    if args.expiry_days.is_some() || args.expiry_date.is_some() {
        rule.expiration = Some(LifecycleExpiration {
            days: args
                .expiry_days
                .or_else(|| rule.expiration.as_ref().and_then(|e| e.days)),
            date: args
                .expiry_date
                .or_else(|| rule.expiration.as_ref().and_then(|e| e.date.clone())),
        });
    }

    // Update transition if provided
    if args.transition_days.is_some()
        || args.transition_date.is_some()
        || args.storage_class.is_some()
    {
        let current_sc = rule
            .transition
            .as_ref()
            .map(|t| t.storage_class.clone())
            .unwrap_or_default();
        let sc = args.storage_class.unwrap_or(current_sc);
        if sc.is_empty() {
            formatter.error("--storage-class is required for transition");
            return ExitCode::UsageError;
        }
        rule.transition = Some(LifecycleTransition {
            days: args
                .transition_days
                .or_else(|| rule.transition.as_ref().and_then(|t| t.days)),
            date: args
                .transition_date
                .or_else(|| rule.transition.as_ref().and_then(|t| t.date.clone())),
            storage_class: sc,
        });
    }

    // Update noncurrent version expiration if provided
    if args.noncurrent_expiry_days.is_some() || args.newer_noncurrent_versions.is_some() {
        let current = rule.noncurrent_version_expiration.as_ref();
        rule.noncurrent_version_expiration = Some(NoncurrentVersionExpiration {
            noncurrent_days: args
                .noncurrent_expiry_days
                .unwrap_or_else(|| current.map(|c| c.noncurrent_days).unwrap_or(0)),
            newer_noncurrent_versions: args
                .newer_noncurrent_versions
                .or_else(|| current.and_then(|c| c.newer_noncurrent_versions)),
        });
    }

    // Update noncurrent version transition if provided
    if args.noncurrent_transition_days.is_some()
        || args.noncurrent_transition_storage_class.is_some()
    {
        let current = rule.noncurrent_version_transition.as_ref();
        let sc = args
            .noncurrent_transition_storage_class
            .or_else(|| current.map(|c| c.storage_class.clone()))
            .unwrap_or_default();
        if sc.is_empty() {
            formatter.error(
                "--noncurrent-transition-storage-class is required for noncurrent transition",
            );
            return ExitCode::UsageError;
        }
        rule.noncurrent_version_transition = Some(NoncurrentVersionTransition {
            noncurrent_days: args
                .noncurrent_transition_days
                .unwrap_or_else(|| current.map(|c| c.noncurrent_days).unwrap_or(0)),
            storage_class: sc,
        });
    }

    // Update expired object delete marker if provided
    if let Some(val) = args.expired_object_delete_marker {
        rule.expired_object_delete_marker = Some(val);
    }

    if let Err(error) = validate_expired_delete_marker_rule(rule) {
        formatter.error(&error);
        return ExitCode::UsageError;
    }

    match client.set_bucket_lifecycle(&bucket, rules).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&RuleOperationOutput {
                    bucket,
                    rule_id: args.id,
                    action: "edited".to_string(),
                });
            } else {
                formatter.success("Lifecycle rule updated successfully.");
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set lifecycle rules: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_list(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let rules = client
        .get_bucket_lifecycle(&bucket)
        .await
        .unwrap_or_default();

    if formatter.is_json() {
        formatter.json(&RuleListOutput { bucket, rules });
        return ExitCode::Success;
    }

    if rules.is_empty() {
        formatter.println("No lifecycle rules found.");
        return ExitCode::Success;
    }

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        "ID",
        "Status",
        "Prefix",
        "Expiry",
        "Transition",
        "Storage Class",
    ]);

    for rule in &rules {
        let prefix = rule.prefix.as_deref().unwrap_or("-");
        let expiry = format_expiry(rule);
        let transition = format_transition(rule);
        let storage_class = rule
            .transition
            .as_ref()
            .map(|t| t.storage_class.as_str())
            .unwrap_or("-");

        table.add_row(vec![
            &rule.id,
            &rule.status.to_string(),
            prefix,
            &expiry,
            &transition,
            storage_class,
        ]);
    }

    formatter.println(&table.to_string());
    ExitCode::Success
}

async fn execute_remove(args: RemoveRuleArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    if args.id.is_none() && !args.all {
        formatter.error("Either --id or --all is required");
        return ExitCode::UsageError;
    }

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    if args.all {
        match client.delete_bucket_lifecycle(&bucket).await {
            Ok(()) => {
                if formatter.is_json() {
                    formatter.json(&RuleOperationOutput {
                        bucket,
                        rule_id: "*".to_string(),
                        action: "removed_all".to_string(),
                    });
                } else {
                    formatter.success("All lifecycle rules removed.");
                }
                return ExitCode::Success;
            }
            Err(error) => {
                formatter.error(&format!(
                    "Failed to delete lifecycle configuration: {error}"
                ));
                return ExitCode::GeneralError;
            }
        }
    }

    // Remove by ID
    let rule_id = args.id.as_deref().unwrap_or("");
    let mut rules = match client.get_bucket_lifecycle(&bucket).await {
        Ok(rules) => rules,
        Err(error) => {
            formatter.error(&format!("Failed to get lifecycle rules: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let before = rules.len();
    rules.retain(|r| r.id != rule_id);
    if rules.len() == before {
        formatter.error(&format!("Rule '{rule_id}' not found"));
        return ExitCode::NotFound;
    }

    let result = if rules.is_empty() {
        client.delete_bucket_lifecycle(&bucket).await
    } else {
        client.set_bucket_lifecycle(&bucket, rules).await
    };

    match result {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&RuleOperationOutput {
                    bucket,
                    rule_id: rule_id.to_string(),
                    action: "removed".to_string(),
                });
            } else {
                formatter.success(&format!("Lifecycle rule '{rule_id}' removed."));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to update lifecycle rules: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_export(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let rules = client
        .get_bucket_lifecycle(&bucket)
        .await
        .unwrap_or_default();

    let config = LifecycleConfiguration { rules };
    formatter.json(&config);
    ExitCode::Success
}

async fn execute_import(args: ImportRuleArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let file_contents = match std::fs::read_to_string(&args.file) {
        Ok(contents) => contents,
        Err(error) => {
            formatter.error(&format!("Failed to read file '{}': {error}", args.file));
            return ExitCode::GeneralError;
        }
    };

    let config: LifecycleConfiguration = match serde_json::from_str(&file_contents) {
        Ok(config) => config,
        Err(error) => {
            formatter.error(&format!("Failed to parse lifecycle configuration: {error}"));
            return ExitCode::UsageError;
        }
    };

    if let Some(error) = config
        .rules
        .iter()
        .find_map(|rule| validate_expired_delete_marker_rule(rule).err())
    {
        formatter.error(&error);
        return ExitCode::UsageError;
    }

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let rule_count = config.rules.len();
    match client.set_bucket_lifecycle(&bucket, config.rules).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&serde_json::json!({
                    "bucket": bucket,
                    "rulesImported": rule_count,
                    "action": "imported",
                }));
            } else {
                formatter.success(&format!(
                    "Imported {rule_count} lifecycle rule(s) to bucket '{bucket}'."
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set lifecycle rules: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Helpers ====================

fn validate_expired_delete_marker_inputs(
    cleanup_enabled: bool,
    has_current_expiration: bool,
    has_tags: bool,
) -> std::result::Result<(), String> {
    if !cleanup_enabled {
        return Ok(());
    }
    if has_current_expiration {
        return Err(
            "expired delete-marker cleanup cannot be combined with current expiration days or date"
                .to_string(),
        );
    }
    if has_tags {
        return Err(
            "expired delete-marker cleanup cannot be combined with tag filters".to_string(),
        );
    }
    Ok(())
}

fn validate_expired_delete_marker_rule(rule: &LifecycleRule) -> std::result::Result<(), String> {
    validate_expired_delete_marker_inputs(
        rule.expired_object_delete_marker == Some(true),
        rule.expiration
            .as_ref()
            .is_some_and(|expiration| expiration.days.is_some() || expiration.date.is_some()),
        rule.tags.as_ref().is_some_and(|tags| !tags.is_empty()),
    )
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
            formatter.error(&format!("Failed to load aliases: {error}"));
            return Err(ExitCode::GeneralError);
        }
    };

    let alias = match alias_manager.get(alias_name) {
        Ok(alias) => alias,
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            return Err(ExitCode::NotFound);
        }
    };

    let client = match S3Client::new(alias).await {
        Ok(client) => client,
        Err(error) => {
            formatter.error(&format!("Failed to create S3 client: {error}"));
            return Err(ExitCode::NetworkError);
        }
    };

    let caps = match client.capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            if force {
                rc_core::Capabilities::default()
            } else {
                formatter.error(&format!("Failed to detect capabilities: {error}"));
                return Err(ExitCode::NetworkError);
            }
        }
    };

    if !force && !caps.lifecycle {
        formatter.error("Backend does not support lifecycle. Use --force to attempt anyway.");
        return Err(ExitCode::UnsupportedFeature);
    }

    match client.bucket_exists(bucket).await {
        Ok(true) => {}
        Ok(false) => {
            formatter.error(&format!("Bucket '{bucket}' does not exist"));
            return Err(ExitCode::NotFound);
        }
        Err(error) => {
            formatter.error(&format!("Failed to check bucket: {error}"));
            return Err(ExitCode::NetworkError);
        }
    }

    Ok(client)
}

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    if path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err("Bucket path must be in format alias/bucket".to_string());
    }

    if parts.iter().skip(2).any(|part| !part.is_empty()) {
        return Err("Bucket path must not include an object key".to_string());
    }

    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn generate_rule_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Use lower 32 bits of nanosecond timestamp as a simple unique suffix
    let suffix = format!("{:08x}", (nanos & 0xFFFF_FFFF) as u32);
    format!("rule-{suffix}")
}

fn format_expiry(rule: &LifecycleRule) -> String {
    if let Some(exp) = &rule.expiration {
        if let Some(days) = exp.days {
            return format!("{days} day(s)");
        }
        if let Some(date) = &exp.date {
            return date.clone();
        }
    }
    "-".to_string()
}

fn format_transition(rule: &LifecycleRule) -> String {
    if let Some(tr) = &rule.transition {
        if let Some(days) = tr.days {
            return format!("{days} day(s)");
        }
        if let Some(date) = &tr.date {
            return date.clone();
        }
    }
    "-".to_string()
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
        assert!(parse_bucket_path("local/my-bucket/key.txt").is_err());
        assert!(parse_bucket_path("local/my-bucket/path/to/key.txt").is_err());
    }

    #[test]
    fn test_generate_rule_id_format() {
        let id = generate_rule_id();
        assert!(id.starts_with("rule-"));
        assert_eq!(id.len(), 13); // "rule-" (5) + 8 hex chars
    }

    #[test]
    fn test_format_expiry_days() {
        let rule = LifecycleRule {
            id: "test".to_string(),
            status: LifecycleRuleStatus::Enabled,
            prefix: None,
            tags: None,
            expiration: Some(LifecycleExpiration {
                days: Some(30),
                date: None,
            }),
            transition: None,
            noncurrent_version_expiration: None,
            noncurrent_version_transition: None,
            abort_incomplete_multipart_upload_days: None,
            expired_object_delete_marker: None,
        };
        assert_eq!(format_expiry(&rule), "30 day(s)");
    }

    #[test]
    fn test_format_transition_none() {
        let rule = LifecycleRule {
            id: "test".to_string(),
            status: LifecycleRuleStatus::Enabled,
            prefix: None,
            tags: None,
            expiration: None,
            transition: None,
            noncurrent_version_expiration: None,
            noncurrent_version_transition: None,
            abort_incomplete_multipart_upload_days: None,
            expired_object_delete_marker: None,
        };
        assert_eq!(format_transition(&rule), "-");
    }

    #[tokio::test]
    async fn test_execute_add_invalid_path_returns_usage_error() {
        let args = AddRuleArgs {
            path: "invalid-path".to_string(),
            expiry_days: Some(30),
            expiry_date: None,
            transition_days: None,
            transition_date: None,
            storage_class: None,
            noncurrent_expiry_days: None,
            noncurrent_transition_days: None,
            noncurrent_transition_storage_class: None,
            prefix: None,
            expired_object_delete_marker: false,
            newer_noncurrent_versions: None,
            disable: false,
            force: false,
        };

        let code = execute_add(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn test_execute_add_rejects_current_expiration_with_marker_cleanup() {
        let args = AddRuleArgs {
            path: "local/my-bucket".to_string(),
            expiry_days: Some(30),
            expiry_date: None,
            transition_days: None,
            transition_date: None,
            storage_class: None,
            noncurrent_expiry_days: None,
            noncurrent_transition_days: None,
            noncurrent_transition_storage_class: None,
            prefix: None,
            expired_object_delete_marker: true,
            newer_noncurrent_versions: None,
            disable: false,
            force: false,
        };

        let code = execute_add(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn test_marker_cleanup_validation_rejects_tags() {
        assert!(validate_expired_delete_marker_inputs(true, false, true).is_err());
        assert!(validate_expired_delete_marker_inputs(true, false, false).is_ok());
    }

    #[tokio::test]
    async fn test_execute_remove_no_id_or_all_returns_usage_error() {
        let args = RemoveRuleArgs {
            path: "local/my-bucket".to_string(),
            id: None,
            all: false,
            force: false,
        };

        let code = execute_remove(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
