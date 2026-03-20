//! replicate command - Manage bucket replication configuration
//!
//! Add, update, list, status, remove, export, or import bucket replication rules.
//! This command orchestrates both the S3 replication API and the Admin remote-target API.

use clap::{Args, Subcommand};
use comfy_table::{Cell, Table};
use rc_core::admin::AdminApi;
use rc_core::replication::{
    BucketTarget, BucketTargetCredentials, ReplicationConfiguration, ReplicationDestination,
    ReplicationRule, ReplicationRuleStatus,
};
use rc_core::{AliasManager, ObjectStore as _};
use rc_s3::{AdminClient, S3Client};
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const DEFAULT_REMOTE_TARGET_PATH: &str = "auto";
const DEFAULT_REMOTE_TARGET_API: &str = "s3v4";
const DEFAULT_REPLICATION_STORAGE_CLASS: &str = "STANDARD";

/// Manage bucket replication
#[derive(Args, Debug)]
pub struct ReplicateArgs {
    #[command(subcommand)]
    pub command: ReplicateCommands,
}

#[derive(Subcommand, Debug)]
pub enum ReplicateCommands {
    /// Add a new replication rule
    Add(AddArgs),

    /// Update an existing replication rule
    Update(UpdateArgs),

    /// List replication rules for a bucket
    List(BucketArg),

    /// Show replication status/metrics for a bucket
    Status(BucketArg),

    /// Remove replication rules from a bucket
    Remove(RemoveArgs),

    /// Export replication configuration as JSON
    Export(BucketArg),

    /// Import replication configuration from a JSON file
    Import(ImportArgs),
}

#[derive(Args, Debug)]
pub struct BucketArg {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Remote target bucket (TARGET_ALIAS/BUCKET)
    #[arg(long, value_name = "TARGET_ALIAS/BUCKET")]
    pub remote_bucket: String,

    /// Replication flags (comma-separated: delete,delete-marker,existing-objects)
    #[arg(long, value_name = "FLAGS")]
    pub replicate: Option<String>,

    /// Rule priority (higher = more important)
    #[arg(long, default_value = "1")]
    pub priority: i32,

    /// Storage class override at destination
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Bandwidth limit in bytes/sec (0 = unlimited)
    #[arg(long, default_value = "0")]
    pub bandwidth: i64,

    /// Enable synchronous replication
    #[arg(long)]
    pub sync: bool,

    /// Key prefix filter
    #[arg(long)]
    pub prefix: Option<String>,

    /// Rule identifier (auto-generated if not specified)
    #[arg(long)]
    pub id: Option<String>,

    /// Health check interval in seconds
    #[arg(long, value_name = "SECONDS", default_value = "60")]
    pub healthcheck_seconds: u64,

    /// Disable replication proxy
    #[arg(long)]
    pub disable_proxy: bool,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Rule ID to update
    #[arg(long)]
    pub id: String,

    /// Replication flags (comma-separated: delete,delete-marker,existing-objects)
    #[arg(long, value_name = "FLAGS")]
    pub replicate: Option<String>,

    /// Rule priority (higher = more important)
    #[arg(long)]
    pub priority: Option<i32>,

    /// Storage class override at destination
    #[arg(long)]
    pub storage_class: Option<String>,

    /// Bandwidth limit in bytes/sec (0 = unlimited)
    #[arg(long)]
    pub bandwidth: Option<i64>,

    /// Enable or disable synchronous replication
    #[arg(long)]
    pub sync: Option<bool>,

    /// Key prefix filter
    #[arg(long)]
    pub prefix: Option<String>,

    /// Health check interval in seconds
    #[arg(long, value_name = "SECONDS")]
    pub healthcheck_seconds: Option<u64>,

    /// Disable replication proxy
    #[arg(long)]
    pub disable_proxy: Option<bool>,

    /// Enable or disable the rule
    #[arg(long)]
    pub status: Option<ReplicationRuleStatus>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RemoveArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Rule ID to remove (omit for --all)
    #[arg(long)]
    pub id: Option<String>,

    /// Remove all replication rules
    #[arg(long)]
    pub all: bool,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Source bucket path (ALIAS/BUCKET)
    pub path: String,

    /// Path to JSON file containing replication configuration
    pub file: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

// ==================== Output types ====================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplicateListOutput {
    bucket: String,
    rules: Vec<ReplicationRule>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplicateOperationOutput {
    bucket: String,
    rule_id: String,
    action: String,
}

// ==================== execute ====================

/// Execute the replicate command
pub async fn execute(args: ReplicateArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        ReplicateCommands::Add(args) => execute_add(args, output_config).await,
        ReplicateCommands::Update(args) => execute_update(args, output_config).await,
        ReplicateCommands::List(args) => execute_list(args, output_config).await,
        ReplicateCommands::Status(args) => execute_status(args, output_config).await,
        ReplicateCommands::Remove(args) => execute_remove(args, output_config).await,
        ReplicateCommands::Export(args) => execute_export(args, output_config).await,
        ReplicateCommands::Import(args) => execute_import(args, output_config).await,
    }
}

// ==================== Add ====================

async fn execute_add(args: AddArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (source_alias, source_bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let (target_alias, target_bucket) = match parse_bucket_path(&args.remote_bucket) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&format!("Invalid --remote-bucket: {error}"));
            return ExitCode::UsageError;
        }
    };

    // Create S3 client for source
    let s3_client =
        match setup_s3_client(&source_alias, &source_bucket, args.force, &formatter).await {
            Ok(client) => client,
            Err(code) => return code,
        };

    // Create admin client for source (to register remote target)
    let admin_client = match setup_admin_client(&source_alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    // Resolve target alias to get endpoint + credentials
    let target_alias_info = match resolve_alias(&target_alias, &formatter) {
        Ok(alias) => alias,
        Err(code) => return code,
    };

    let (target_endpoint, secure) =
        remote_target_endpoint(&target_alias_info.endpoint, target_alias_info.insecure);

    let storage_class = args
        .storage_class
        .clone()
        .unwrap_or_else(|| DEFAULT_REPLICATION_STORAGE_CLASS.to_string());

    // Build BucketTarget
    let target = BucketTarget {
        source_bucket: source_bucket.clone(),
        endpoint: target_endpoint,
        credentials: Some(BucketTargetCredentials {
            access_key: target_alias_info.access_key.clone(),
            secret_key: target_alias_info.secret_key.clone(),
        }),
        target_bucket: target_bucket.clone(),
        secure,
        path: DEFAULT_REMOTE_TARGET_PATH.to_string(),
        api: DEFAULT_REMOTE_TARGET_API.to_string(),
        target_type: "replication".to_string(),
        region: target_alias_info.region.clone(),
        bandwidth_limit: args.bandwidth,
        replication_sync: args.sync,
        storage_class: storage_class.clone(),
        health_check_duration: args.healthcheck_seconds,
        disable_proxy: args.disable_proxy,
        ..Default::default()
    };

    // Register remote target via admin API → get ARN
    let arn = match admin_client
        .set_remote_target(&source_bucket, target, false)
        .await
    {
        Ok(arn) => arn,
        Err(error) => {
            formatter.error(&format!("Failed to set remote target: {error}"));
            return ExitCode::GeneralError;
        }
    };

    // Parse replication flags
    let (delete_replication, delete_marker_replication, existing_object_replication) =
        parse_replicate_flags(args.replicate.as_deref());

    let rule_id = args
        .id
        .unwrap_or_else(|| format!("rule-{}", &arn[arn.len().saturating_sub(8)..]));

    let destination_storage_class = Some(storage_class);

    let new_rule = ReplicationRule {
        id: rule_id.clone(),
        priority: args.priority,
        status: ReplicationRuleStatus::Enabled,
        prefix: args.prefix,
        tags: None,
        destination: ReplicationDestination {
            bucket_arn: arn,
            storage_class: destination_storage_class,
        },
        delete_marker_replication: Some(delete_marker_replication),
        existing_object_replication: Some(existing_object_replication),
        delete_replication: Some(delete_replication),
    };

    // Get existing config or create new
    let mut config = match s3_client.get_bucket_replication(&source_bucket).await {
        Ok(Some(config)) => config,
        Ok(None) => ReplicationConfiguration {
            role: default_replication_role(&new_rule.destination.bucket_arn),
            rules: Vec::new(),
        },
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            return ExitCode::GeneralError;
        }
    };

    if config.role.is_empty() {
        config.role = default_replication_role(&new_rule.destination.bucket_arn);
    }

    config.rules.push(new_rule);

    match s3_client
        .set_bucket_replication(&source_bucket, config)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&ReplicateOperationOutput {
                    bucket: source_bucket,
                    rule_id,
                    action: "added".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Replication rule '{}' added for '{}/{}'",
                    rule_id, source_alias, source_bucket
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Update ====================

async fn execute_update(args: UpdateArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (source_alias, source_bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let s3_client =
        match setup_s3_client(&source_alias, &source_bucket, args.force, &formatter).await {
            Ok(client) => client,
            Err(code) => return code,
        };

    let mut config = match s3_client.get_bucket_replication(&source_bucket).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            formatter.error("No replication configuration found on this bucket");
            return ExitCode::NotFound;
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let rule = match config.rules.iter_mut().find(|r| r.id == args.id) {
        Some(rule) => rule,
        None => {
            formatter.error(&format!("Rule '{}' not found", args.id));
            return ExitCode::NotFound;
        }
    };

    // Apply updates
    if let Some(priority) = args.priority {
        rule.priority = priority;
    }
    if let Some(status) = args.status {
        rule.status = status;
    }
    if let Some(ref prefix) = args.prefix {
        rule.prefix = Some(prefix.clone());
    }
    if let Some(ref storage_class) = args.storage_class {
        rule.destination.storage_class = Some(storage_class.clone());
    }
    if let Some(ref flags) = args.replicate {
        let (delete, delete_marker, existing) = parse_replicate_flags(Some(flags));
        rule.delete_replication = Some(delete);
        rule.delete_marker_replication = Some(delete_marker);
        rule.existing_object_replication = Some(existing);
    }

    let rule_id = args.id.clone();

    match s3_client
        .set_bucket_replication(&source_bucket, config)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&ReplicateOperationOutput {
                    bucket: source_bucket,
                    rule_id,
                    action: "updated".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Replication rule '{}' updated for '{}/{}'",
                    rule_id, source_alias, source_bucket
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to update replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== List ====================

async fn execute_list(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_replication(&bucket).await {
        Ok(Some(config)) => {
            if formatter.is_json() {
                formatter.json(&ReplicateListOutput {
                    bucket,
                    rules: config.rules,
                });
            } else if config.rules.is_empty() {
                formatter.println("No replication rules found.");
            } else {
                let mut table = Table::new();
                table.set_header(vec![
                    Cell::new("ID"),
                    Cell::new("Priority"),
                    Cell::new("Status"),
                    Cell::new("Prefix"),
                    Cell::new("Flags"),
                    Cell::new("Destination"),
                    Cell::new("Storage Class"),
                ]);

                for rule in &config.rules {
                    table.add_row(vec![
                        Cell::new(&rule.id),
                        Cell::new(rule.priority),
                        Cell::new(rule.status),
                        Cell::new(rule.prefix.as_deref().unwrap_or("-")),
                        Cell::new(format_replication_flags(rule)),
                        Cell::new(&rule.destination.bucket_arn),
                        Cell::new(rule.destination.storage_class.as_deref().unwrap_or("-")),
                    ]);
                }

                formatter.println(&table.to_string());
            }
            ExitCode::Success
        }
        Ok(None) => {
            if formatter.is_json() {
                formatter.json(&ReplicateListOutput {
                    bucket,
                    rules: Vec::new(),
                });
            } else {
                formatter.println("No replication configuration found.");
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Status ====================

async fn execute_status(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let admin_client = match setup_admin_client(&alias_name, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match admin_client.replication_metrics(&bucket).await {
        Ok(metrics) => {
            if formatter.is_json() {
                formatter.json(&metrics);
            } else {
                formatter.println(&format!("Replication metrics for '{alias_name}/{bucket}':"));
                match serde_json::to_string_pretty(&metrics) {
                    Ok(pretty) => formatter.println(&pretty),
                    Err(error) => {
                        formatter.error(&format!("Failed to format metrics: {error}"));
                        return ExitCode::GeneralError;
                    }
                }
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication metrics: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Remove ====================

async fn execute_remove(args: RemoveArgs, output_config: OutputConfig) -> ExitCode {
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

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    if args.all {
        match client.delete_bucket_replication(&bucket).await {
            Ok(()) => {
                if formatter.is_json() {
                    formatter.json(&ReplicateOperationOutput {
                        bucket,
                        rule_id: "*".to_string(),
                        action: "removed".to_string(),
                    });
                } else {
                    formatter.success("All replication rules removed.");
                }
                return ExitCode::Success;
            }
            Err(error) => {
                formatter.error(&format!("Failed to remove replication config: {error}"));
                return ExitCode::GeneralError;
            }
        }
    }

    // Remove specific rule by ID
    let rule_id = args.id.as_deref().unwrap_or_default();

    let mut config = match client.get_bucket_replication(&bucket).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            formatter.error("No replication configuration found on this bucket");
            return ExitCode::NotFound;
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let before = config.rules.len();
    config.rules.retain(|r| r.id != rule_id);

    if config.rules.len() == before {
        formatter.error(&format!("Rule '{}' not found", rule_id));
        return ExitCode::NotFound;
    }

    if config.rules.is_empty() {
        match client.delete_bucket_replication(&bucket).await {
            Ok(()) => {}
            Err(error) => {
                formatter.error(&format!("Failed to remove replication config: {error}"));
                return ExitCode::GeneralError;
            }
        }
    } else {
        match client.set_bucket_replication(&bucket, config).await {
            Ok(()) => {}
            Err(error) => {
                formatter.error(&format!("Failed to update replication config: {error}"));
                return ExitCode::GeneralError;
            }
        }
    }

    if formatter.is_json() {
        formatter.json(&ReplicateOperationOutput {
            bucket,
            rule_id: rule_id.to_string(),
            action: "removed".to_string(),
        });
    } else {
        formatter.success(&format!("Replication rule '{}' removed.", rule_id));
    }
    ExitCode::Success
}

// ==================== Export ====================

async fn execute_export(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_replication(&bucket).await {
        Ok(Some(config)) => {
            formatter.json(&config);
            ExitCode::Success
        }
        Ok(None) => {
            formatter.error("No replication configuration found on this bucket");
            ExitCode::NotFound
        }
        Err(error) => {
            formatter.error(&format!("Failed to get replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Import ====================

async fn execute_import(args: ImportArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let data = match std::fs::read_to_string(&args.file) {
        Ok(data) => data,
        Err(error) => {
            formatter.error(&format!("Failed to read file '{}': {error}", args.file));
            return ExitCode::GeneralError;
        }
    };

    let config: ReplicationConfiguration = match serde_json::from_str(&data) {
        Ok(config) => config,
        Err(error) => {
            formatter.error(&format!("Invalid JSON in '{}': {error}", args.file));
            return ExitCode::UsageError;
        }
    };

    let client = match setup_s3_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.set_bucket_replication(&bucket, config).await {
        Ok(()) => {
            if formatter.is_json() {
                let output = serde_json::json!({
                    "bucket": bucket,
                    "action": "imported",
                    "file": args.file,
                });
                formatter.json(&output);
            } else {
                formatter.success(&format!(
                    "Replication configuration imported from '{}'",
                    args.file
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to set replication config: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Helpers ====================

fn parse_bucket_path(path: &str) -> Result<(String, String), String> {
    if path.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let parts: Vec<&str> = path.splitn(2, '/').collect();

    if parts.len() < 2 || parts[0].is_empty() {
        return Err("Alias name is required (ALIAS/BUCKET)".to_string());
    }

    let bucket = parts[1].trim_end_matches('/');
    if bucket.is_empty() {
        return Err("Bucket name is required (ALIAS/BUCKET)".to_string());
    }

    Ok((parts[0].to_string(), bucket.to_string()))
}

fn resolve_alias(alias_name: &str, formatter: &Formatter) -> Result<rc_core::Alias, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            formatter.error(&format!("Failed to load aliases: {error}"));
            return Err(ExitCode::GeneralError);
        }
    };

    match alias_manager.get(alias_name) {
        Ok(alias) => Ok(alias),
        Err(_) => {
            formatter.error(&format!("Alias '{alias_name}' not found"));
            Err(ExitCode::NotFound)
        }
    }
}

async fn setup_s3_client(
    alias_name: &str,
    bucket: &str,
    force: bool,
    formatter: &Formatter,
) -> Result<S3Client, ExitCode> {
    let alias = match resolve_alias(alias_name, formatter) {
        Ok(alias) => alias,
        Err(code) => return Err(code),
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

    if !force && !caps.replication {
        formatter.error("Backend does not support replication. Use --force to attempt anyway.");
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

fn setup_admin_client(alias_name: &str, formatter: &Formatter) -> Result<AdminClient, ExitCode> {
    let alias = resolve_alias(alias_name, formatter)?;

    match AdminClient::new(&alias) {
        Ok(client) => Ok(client),
        Err(error) => {
            formatter.error(&format!("Failed to create admin client: {error}"));
            Err(ExitCode::GeneralError)
        }
    }
}

/// Parse --replicate flag value into (delete, delete_marker, existing_objects) booleans.
fn parse_replicate_flags(flags: Option<&str>) -> (bool, bool, bool) {
    let mut delete = false;
    let mut delete_marker = false;
    let mut existing_objects = false;

    if let Some(flags_str) = flags {
        for flag in flags_str.split(',').map(str::trim) {
            match flag.to_lowercase().as_str() {
                "delete" => delete = true,
                "delete-marker" => delete_marker = true,
                "existing-objects" => existing_objects = true,
                _ => {}
            }
        }
    }

    (delete, delete_marker, existing_objects)
}

fn default_replication_role(bucket_arn: &str) -> String {
    bucket_arn.to_string()
}

fn format_replication_flags(rule: &ReplicationRule) -> String {
    let mut flags = Vec::new();

    if rule.delete_replication == Some(true) {
        flags.push("delete");
    }
    if rule.delete_marker_replication == Some(true) {
        flags.push("delete-marker");
    }
    if rule.existing_object_replication == Some(true) {
        flags.push("existing-objects");
    }

    if flags.is_empty() {
        "-".to_string()
    } else {
        flags.join(",")
    }
}

fn remote_target_endpoint(endpoint: &str, insecure: bool) -> (String, bool) {
    let trimmed = endpoint.trim().trim_end_matches('/');

    if let Some(rest) = trimmed.strip_prefix("https://") {
        return (strip_endpoint_path(rest), true);
    }

    if let Some(rest) = trimmed.strip_prefix("http://") {
        return (strip_endpoint_path(rest), false);
    }

    (strip_endpoint_path(trimmed), !insecure)
}

fn strip_endpoint_path(endpoint: &str) -> String {
    endpoint.split('/').next().unwrap_or(endpoint).to_string()
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
    fn test_parse_bucket_path_errors() {
        assert!(parse_bucket_path("").is_err());
        assert!(parse_bucket_path("local").is_err());
        assert!(parse_bucket_path("/bucket").is_err());
        assert!(parse_bucket_path("local/").is_err());
    }

    #[test]
    fn test_parse_replicate_flags_none() {
        let (d, dm, eo) = parse_replicate_flags(None);
        assert!(!d);
        assert!(!dm);
        assert!(!eo);
    }

    #[test]
    fn test_parse_replicate_flags_all() {
        let (d, dm, eo) = parse_replicate_flags(Some("delete,delete-marker,existing-objects"));
        assert!(d);
        assert!(dm);
        assert!(eo);
    }

    #[test]
    fn test_parse_replicate_flags_partial() {
        let (d, dm, eo) = parse_replicate_flags(Some("delete-marker"));
        assert!(!d);
        assert!(dm);
        assert!(!eo);
    }

    #[test]
    fn test_parse_replicate_flags_case_insensitive() {
        let (d, _, _) = parse_replicate_flags(Some("DELETE"));
        assert!(d);
    }

    #[test]
    fn test_default_replication_role_uses_destination_arn() {
        let arn = "arn:rustfs:replication:us-east-1:123:test";
        assert_eq!(default_replication_role(arn), arn);
    }

    #[test]
    fn test_format_replication_flags_includes_delete_replication() {
        let rule = ReplicationRule {
            id: "rule-1".to_string(),
            priority: 1,
            status: ReplicationRuleStatus::Enabled,
            prefix: None,
            tags: None,
            destination: ReplicationDestination {
                bucket_arn: "arn:rustfs:replication:us-east-1:123:test".to_string(),
                storage_class: Some("STANDARD".to_string()),
            },
            delete_marker_replication: Some(true),
            existing_object_replication: Some(true),
            delete_replication: Some(true),
        };

        assert_eq!(
            format_replication_flags(&rule),
            "delete,delete-marker,existing-objects"
        );
    }

    #[test]
    fn test_remote_target_endpoint_strips_scheme_and_path() {
        let (endpoint, secure) = remote_target_endpoint("https://localhost:9005/path/", false);
        assert_eq!(endpoint, "localhost:9005");
        assert!(secure);
    }

    #[test]
    fn test_remote_target_endpoint_supports_plain_host_port() {
        let (endpoint, secure) = remote_target_endpoint("localhost:9005", true);
        assert_eq!(endpoint, "localhost:9005");
        assert!(!secure);
    }

    #[test]
    fn test_add_defaults_destination_storage_class_to_standard() {
        let rule = ReplicationRule {
            id: "rule-1".to_string(),
            priority: 1,
            status: ReplicationRuleStatus::Enabled,
            prefix: None,
            tags: None,
            destination: ReplicationDestination {
                bucket_arn: "arn:rustfs:replication:us-east-1:123:test".to_string(),
                storage_class: Some("STANDARD".to_string()),
            },
            delete_marker_replication: Some(false),
            existing_object_replication: Some(false),
            delete_replication: Some(false),
        };

        assert_eq!(rule.destination.storage_class.as_deref(), Some("STANDARD"));
    }

    #[tokio::test]
    async fn test_execute_add_invalid_path_returns_usage_error() {
        let args = ReplicateArgs {
            command: ReplicateCommands::Add(AddArgs {
                path: "no-slash".to_string(),
                remote_bucket: "target/bucket".to_string(),
                replicate: None,
                priority: 1,
                storage_class: None,
                bandwidth: 0,
                sync: false,
                prefix: None,
                id: None,
                healthcheck_seconds: 60,
                disable_proxy: false,
                force: false,
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn test_execute_remove_requires_id_or_all() {
        let args = ReplicateArgs {
            command: ReplicateCommands::Remove(RemoveArgs {
                path: "local/bucket".to_string(),
                id: None,
                all: false,
                force: false,
            }),
        };

        let code = execute(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
