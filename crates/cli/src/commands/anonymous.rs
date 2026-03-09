//! anonymous command - Manage anonymous access to buckets and objects
//!
//! Supports anonymous access operations compatible with `mc anonymous`:
//! - set: set access level to private/public/download/upload
//! - set-json: set access using a policy JSON file
//! - get: get permission level
//! - get-json: get raw policy JSON
//! - list: list access rules
//! - links: list public object links, optionally recursively

use std::collections::{HashMap, HashSet};
use std::fmt;

use clap::{Args, Subcommand};
use rc_core::{Alias, AliasManager, ListOptions, ObjectStore as _, RemotePath};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

#[derive(Args, Debug)]
pub struct AnonymousArgs {
    #[command(subcommand)]
    pub command: AnonymousCommands,
}

#[derive(Subcommand, Debug)]
pub enum AnonymousCommands {
    /// Set anonymous access permission
    Set(SetArgs),

    /// Set anonymous access from a policy JSON file
    SetJson(SetJsonArgs),

    /// Get anonymous access permission
    Get(AnonymousPathArg),

    /// Get anonymous policy JSON
    GetJson(AnonymousPathArg),

    /// List anonymous policies
    List(AnonymousPathArg),

    /// List public links for anonymous readable prefixes
    Links(LinksArgs),
}

#[derive(Args, Debug)]
pub struct SetArgs {
    /// Permission: private, public, download, upload
    pub permission: String,

    /// Target path (alias/bucket or alias/bucket/prefix)
    pub path: String,
}

#[derive(Args, Debug)]
pub struct SetJsonArgs {
    /// Path to policy JSON file
    pub file: String,

    /// Target path (alias/bucket or alias/bucket/prefix)
    pub path: String,
}

#[derive(Args, Debug)]
pub struct AnonymousPathArg {
    /// Target path (alias/bucket or alias/bucket/prefix)
    pub path: String,
}

#[derive(Args, Debug)]
pub struct LinksArgs {
    /// Target path (alias/bucket or alias/bucket/prefix)
    pub path: String,

    /// Recursively list links
    #[arg(short, long)]
    pub recursive: bool,
}

#[derive(Debug, Serialize)]
struct PermissionOutput {
    path: String,
    permission: String,
}

#[derive(Debug, Serialize)]
struct RuleOutput {
    resource: String,
    permission: String,
}

#[derive(Debug, Serialize)]
struct LinkOutput {
    url: String,
    status: String,
}

#[derive(Debug)]
struct AccessTarget {
    alias: String,
    bucket: String,
    prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessLevel {
    Private,
    Download,
    Upload,
    Public,
    Custom,
}

impl AccessLevel {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_lowercase().as_str() {
            "private" => Ok(Self::Private),
            "public" => Ok(Self::Public),
            "download" => Ok(Self::Download),
            "upload" => Ok(Self::Upload),
            _ => Err(format!(
                "Invalid permission '{value}'. Allowed: private, public, download, upload"
            )),
        }
    }

    fn is_read(self) -> bool {
        matches!(self, Self::Download | Self::Public)
    }

    fn is_write(self) -> bool {
        matches!(self, Self::Upload | Self::Public)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Download => "download",
            Self::Upload => "upload",
            Self::Public => "public",
            Self::Custom => "custom",
        }
    }
}

impl fmt::Display for AccessLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, Default, Debug)]
struct PermissionFlags {
    read: bool,
    write: bool,
    custom: bool,
}

#[derive(Debug)]
struct AccessRule {
    resource: String,
    flags: PermissionFlags,
}

impl PermissionFlags {
    fn add(&mut self, other: PermissionFlags) {
        self.read |= other.read;
        self.write |= other.write;
        self.custom |= other.custom;
    }

    fn level(self) -> AccessLevel {
        if self.custom {
            AccessLevel::Custom
        } else if self.read && self.write {
            AccessLevel::Public
        } else if self.read {
            AccessLevel::Download
        } else if self.write {
            AccessLevel::Upload
        } else {
            AccessLevel::Private
        }
    }
}

pub async fn execute(args: AnonymousArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        AnonymousCommands::Set(args) => execute_set(args, output_config).await,
        AnonymousCommands::SetJson(args) => execute_set_json(args, output_config).await,
        AnonymousCommands::Get(args) => execute_get(args, output_config).await,
        AnonymousCommands::GetJson(args) => execute_get_json(args, output_config).await,
        AnonymousCommands::List(args) => execute_list(args, output_config).await,
        AnonymousCommands::Links(args) => execute_links(args, output_config).await,
    }
}

async fn execute_set(args: SetArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let permission = match AccessLevel::parse(&args.permission) {
        Ok(permission) => permission,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let target = match parse_anonymous_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let (_alias, client) = match setup_anonymous_client(&target.alias, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let result = if permission == AccessLevel::Private {
        client.delete_bucket_policy(&target.bucket).await
    } else {
        let policy = build_policy(&permission, &target.bucket, &target.prefix);
        client.set_bucket_policy(&target.bucket, &policy).await
    };

    match result {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&PermissionOutput {
                    path: target.path_with_bucket_prefix(),
                    permission: permission.as_str().to_string(),
                });
            } else {
                formatter.println(&format!(
                    "Set anonymous permission for '{}' to '{}'",
                    target.path_with_bucket_prefix(),
                    permission
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!(
                "Failed to set anonymous access for '{}': {error}",
                target.path_with_bucket_prefix()
            ));
            exit_code_from_error(&error)
        }
    }
}

async fn execute_set_json(args: SetJsonArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let target = match parse_anonymous_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let policy = match tokio::fs::read_to_string(&args.file).await {
        Ok(content) => content,
        Err(error) => {
            formatter.error(&format!("Failed to read policy file '{}': {error}", args.file));
            return ExitCode::GeneralError;
        }
    };

    if serde_json::from_str::<serde_json::Value>(&policy).is_err() {
        formatter.error("Provided policy file is not valid JSON");
        return ExitCode::UsageError;
    }

    let (_alias, client) = match setup_anonymous_client(&target.alias, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.set_bucket_policy(&target.bucket, &policy).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&PermissionOutput {
                    path: target.path_with_bucket_prefix(),
                    permission: "custom".to_string(),
                });
            } else {
                formatter.println(&format!(
                    "Set anonymous policy from file for '{}' successfully",
                    target.path_with_bucket_prefix()
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!(
                "Failed to set anonymous policy for '{}': {error}",
                target.path_with_bucket_prefix()
            ));
            exit_code_from_error(&error)
        }
    }
}

async fn execute_get(args: AnonymousPathArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let target = match parse_anonymous_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let (_alias, client) = match setup_anonymous_client(&target.alias, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let permission = match client.get_bucket_policy(&target.bucket).await {
        Ok(Some(policy)) => match parse_permission_from_policy(&policy, &target.bucket, &target.prefix) {
            Ok(permission) => permission,
            Err(_) => AccessLevel::Custom,
        },
        Ok(None) => AccessLevel::Private,
        Err(error) => {
            formatter.error(&format!(
                "Failed to get anonymous policy for '{}': {error}",
                target.path_with_bucket_prefix()
            ));
            return exit_code_from_error(&error);
        }
    };

    if formatter.is_json() {
        formatter.json(&PermissionOutput {
            path: target.path_with_bucket_prefix(),
            permission: permission.as_str().to_string(),
        });
    } else {
        formatter.println(&format!(
            "Anonymous access permission for '{}' is '{}'",
            target.path_with_bucket_prefix(),
            permission
        ));
    }

    ExitCode::Success
}

async fn execute_get_json(args: AnonymousPathArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let target = match parse_anonymous_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let (_alias, client) = match setup_anonymous_client(&target.alias, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let policy = match client.get_bucket_policy(&target.bucket).await {
        Ok(Some(policy)) => policy,
        Ok(None) => "{}".to_string(),
        Err(error) => {
            formatter.error(&format!(
                "Failed to get anonymous policy for '{}': {error}",
                target.path_with_bucket_prefix()
            ));
            return exit_code_from_error(&error);
        }
    };

    match serde_json::from_str::<serde_json::Value>(&policy) {
        Ok(json) => formatter.json(&json),
        Err(_) => formatter.json(&policy),
    }

    ExitCode::Success
}

async fn execute_list(args: AnonymousPathArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let target = match parse_anonymous_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let (_alias, client) = match setup_anonymous_client(&target.alias, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let policy = match client.get_bucket_policy(&target.bucket).await {
        Ok(Some(policy)) => policy,
        Ok(None) => {
            if formatter.is_json() {
                formatter.json(&Vec::<RuleOutput>::new());
            } else {
                formatter.println("No anonymous policies found.");
            }
            return ExitCode::Success;
        }
        Err(error) => {
            formatter.error(&format!(
                "Failed to list anonymous policies for '{}': {error}",
                target.path_with_bucket_prefix()
            ));
            return exit_code_from_error(&error);
        }
    };

    let rules = parse_access_rules(&policy, &target.bucket).unwrap_or_else(|_| {
        vec![AccessRule {
            resource: String::new(),
            flags: PermissionFlags {
                read: false,
                write: false,
                custom: true,
            },
        }]
    });

    if rules.is_empty() {
        if formatter.is_json() {
            formatter.json(&Vec::<RuleOutput>::new());
        } else {
            formatter.println("No anonymous policies found.");
        }
        return ExitCode::Success;
    }

    // For compatibility with `mc anonymous list`, always show all known anonymous rules for
    // the bucket. This avoids false negatives for bucket-level and inherited policies.
    if rules.is_empty() {
        if formatter.is_json() {
            formatter.json(&Vec::<RuleOutput>::new());
        } else {
            formatter.println("No anonymous policies found for target.");
        }
        return ExitCode::Success;
    }

    let mut merged: HashMap<String, PermissionFlags> = HashMap::new();
    for rule in rules {
        merged
            .entry(rule.resource)
            .and_modify(|state| state.add(rule.flags))
            .or_insert(rule.flags);
    }

    let mut rules: Vec<AccessRule> = merged
        .into_iter()
        .map(|(resource, flags)| AccessRule { resource, flags })
        .collect();

    // Sort for deterministic output.
    rules.sort_by(|a, b| a.resource.cmp(&b.resource));

    if formatter.is_json() {
        let output = rules
            .iter()
            .map(|r| RuleOutput {
                resource: r.resource.clone(),
                permission: r.flags.level().as_str().to_string(),
            })
            .collect::<Vec<_>>();
        formatter.json(&output);
    } else {
        formatter.println("Anonymous policies:");
        for rule in rules {
            formatter.println(&format!(
                "  {} => {}",
                rule.resource,
                rule.flags.level()
            ));
        }
    }

    ExitCode::Success
}

async fn execute_links(args: LinksArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let target = match parse_anonymous_path(&args.path) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let (alias, client) = match setup_anonymous_client(&target.alias, &formatter).await {
        Ok(v) => v,
        Err(code) => return code,
    };

    let policy = match client.get_bucket_policy(&target.bucket).await {
        Ok(Some(policy)) => policy,
        Ok(None) => {
            formatter.println("No anonymous policies found for target.");
            return ExitCode::Success;
        }
        Err(error) => {
            formatter.error(&format!(
                "Failed to resolve anonymous links for '{}': {error}",
                target.path_with_bucket_prefix()
            ));
            return exit_code_from_error(&error);
        }
    };

    let rules = match parse_access_rules(&policy, &target.bucket) {
        Ok(rules) => rules,
        Err(_) => {
            formatter.println("Policy does not contain recognizable anonymous rules.");
            return ExitCode::Success;
        }
    };

    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let target_prefix = target.prefix.trim_end_matches('/').to_string();

    for rule in rules {
        if !rule.is_read() {
            continue;
        }

        if !prefixes_overlap(&rule.resource, &target_prefix) {
            continue;
        }

        let list_prefix = if target_prefix.is_empty() {
            rule.resource.clone()
        } else if rule.resource.is_empty() {
            target_prefix.clone()
        } else {
            rule.resource.clone()
        };

        for key in list_keys_for_prefix(
            &client,
            &target.bucket,
            &list_prefix,
            args.recursive,
        )
        .await
        .unwrap_or_default()
        {
            if !target_prefix.is_empty() && !key_under_prefix(&key, &target_prefix) {
                continue;
            }
            let url = build_public_url(&alias.endpoint, &target.bucket, &key);
            if seen.insert(url.clone()) {
                urls.push(url);
            }
        }
    }

    urls.sort_unstable();
    if urls.is_empty() {
        formatter.println("No public links found for target.");
        return ExitCode::Success;
    }

    if formatter.is_json() {
        let output: Vec<LinkOutput> = urls
            .iter()
            .map(|u| LinkOutput {
                url: u.clone(),
                status: "success".to_string(),
            })
            .collect();
        formatter.json(&output);
    } else {
        for url in urls {
            formatter.println(&format!("  {url}"));
        }
    }

    ExitCode::Success
}

fn parse_anonymous_path(path: &str) -> Result<AccessTarget, String> {
    if path.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let mut parts = path.splitn(3, '/');
    let alias = parts
        .next()
        .ok_or_else(|| "Path must include alias".to_string())?;
    if alias.is_empty() {
        return Err("Alias name is required (alias/bucket[/prefix])".to_string());
    }

    let bucket = parts
        .next()
        .ok_or_else(|| "Bucket is required (alias/bucket[/prefix])".to_string())?;
    if bucket.is_empty() {
        return Err("Bucket is required (alias/bucket[/prefix])".to_string());
    }

    let prefix = parts.next().map(|v| v.trim_end_matches('/')).unwrap_or("");
    if alias.is_empty() || bucket.is_empty() {
        return Err("Path must be in format alias/bucket[/prefix]".to_string());
    }

    Ok(AccessTarget {
        alias: alias.to_string(),
        bucket: bucket.to_string(),
        prefix: prefix.to_string(),
    })
}

fn build_policy(level: &AccessLevel, bucket: &str, prefix: &str) -> String {
    let object_resource = build_object_resource(bucket, prefix);
    let bucket_resource = format!("arn:aws:s3:::{}", bucket);
    let mut statements = Vec::new();
    let mut sid = 1u8;

    if level.is_read() {
        statements.push(serde_json::json!({
            "Sid": format!("AnonymousRead{sid}"),
            "Effect": "Allow",
            "Principal": "*",
            "Action": ["s3:GetObject"],
            "Resource": object_resource,
        }));
        sid += 1;

        let mut condition = serde_json::json!({});
        if !prefix.is_empty() {
            let sanitized = prefix.trim_end_matches('/');
            condition = serde_json::json!({
                "StringLike": {
                    "s3:prefix": [format!("{sanitized}"), format!("{sanitized}/*"), format!("{sanitized}*")]
                }
            });
        }
        if prefix.is_empty() {
            statements.push(serde_json::json!({
                "Sid": format!("AnonymousList{sid}"),
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:ListBucket",
                "Resource": bucket_resource,
            }));
        } else {
            statements.push(serde_json::json!({
                "Sid": format!("AnonymousList{sid}"),
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:ListBucket",
                "Resource": bucket_resource,
                "Condition": condition,
            }));
        }
        sid += 1;
    }

    if level.is_write() {
        statements.push(serde_json::json!({
            "Sid": format!("AnonymousWrite{sid}"),
            "Effect": "Allow",
            "Principal": "*",
            "Action": ["s3:PutObject"],
            "Resource": build_object_resource(bucket, prefix),
        }));
    }

    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": statements,
    })
    .to_string()
}

fn build_object_resource(bucket: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        format!("arn:aws:s3:::{}/*", bucket)
    } else {
        format!(
            "arn:aws:s3:::{}/*",
            format!("{}/{}", bucket, prefix.trim_end_matches('/'))
        )
    }
}

async fn list_keys_for_prefix(
    client: &S3Client,
    bucket: &str,
    prefix: &str,
    recursive: bool,
) -> Result<Vec<String>, rc_core::Error> {
    let mut options = ListOptions {
        recursive,
        max_keys: Some(1000),
        prefix: if prefix.is_empty() {
            None
        } else {
            Some(prefix.to_string())
        },
        ..Default::default()
    };

    let mut continuation: Option<String> = None;
    let mut keys = Vec::new();

    loop {
        options.continuation_token = continuation.clone();
        let path = RemotePath::new("anonymous", bucket, prefix);
        let result = client.list_objects(&path, options.clone()).await?;
        for item in result.items {
            if !item.is_dir {
                keys.push(item.key);
            }
        }

        if !result.truncated {
            break;
        }
        continuation = result.continuation_token;
    }

    Ok(keys)
}

fn parse_access_rules(policy: &str, bucket: &str) -> Result<Vec<AccessRule>, String> {
    let policy: serde_json::Value = serde_json::from_str(policy).map_err(|e| e.to_string())?;
    let statements = match policy.get("Statement") {
        Some(statement) => parse_statement_list(statement)?,
        None => return Err("Policy has no statements".to_string()),
    };

    let mut rules = Vec::new();

    for statement in statements {
        let effect = statement
            .get("Effect")
            .and_then(|value| value.as_str())
            .unwrap_or("Allow");
        if !effect.eq_ignore_ascii_case("allow") {
            continue;
        }

        if !is_public_principal(statement.get("Principal")) {
            continue;
        }

        let actions = parse_string_or_array(statement.get("Action"));
        let resources = parse_string_or_array(statement.get("Resource"));
        if actions.is_empty() || resources.is_empty() {
            continue;
        }

        let mut flags = PermissionFlags::default();
        for action in actions {
            let parsed = parse_action_flags(&action);
            flags.read |= parsed.read;
            flags.write |= parsed.write;
            flags.custom |= parsed.custom;
        }
        if !flags.read && !flags.write && !flags.custom {
            continue;
        }

        for resource in resources {
            if let Some(resource_path) = normalize_bucket_resource(bucket, &resource) {
                rules.push(AccessRule {
                    resource: resource_path,
                    flags,
                });
            }
        }
    }
    Ok(rules)
}

fn parse_permission_from_policy(
    policy: &str,
    bucket: &str,
    target_prefix: &str,
) -> Result<AccessLevel, String> {
    let rules = parse_access_rules(policy, bucket)?;
    Ok(permission_for_target(&rules, target_prefix))
}

fn permission_for_target(rules: &[AccessRule], target_prefix: &str) -> AccessLevel {
    let target_prefix = target_prefix.trim_end_matches('/').to_string();
    let mut state = PermissionFlags::default();

    for rule in rules {
        if !target_covers_prefix(&rule.resource, &target_prefix) {
            continue;
        }

        state.add(rule.flags);
    }

    state.level()
}

fn target_covers_prefix(rule_prefix: &str, target_prefix: &str) -> bool {
    if target_prefix.is_empty() {
        return rule_prefix.is_empty();
    }
    if rule_prefix.is_empty() {
        return true;
    }

    let rule_prefix = rule_prefix.trim_end_matches('/');
    let target_prefix = target_prefix.trim_end_matches('/');

    target_prefix == rule_prefix
        || target_prefix
            .strip_prefix(rule_prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn normalize_bucket_resource(bucket: &str, resource: &str) -> Option<String> {
    let resource = resource
        .strip_prefix("arn:aws:s3:::")
        .unwrap_or(resource);

    if resource == bucket {
        return Some(String::new());
    }
    if !resource.starts_with(bucket) {
        return None;
    }

    let mut rest = resource.strip_prefix(bucket)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    if !rest.starts_with('/') && rest != "*" && !rest.starts_with('*') {
        return None;
    }

    rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Some(String::new());
    }

    let trimmed = rest
        .trim_end_matches('/')
        .trim_end_matches('*')
        .trim_start_matches('/');
    if trimmed.is_empty() {
        Some(String::new())
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_action_flags(action: &str) -> PermissionFlags {
    let action = action.to_ascii_lowercase();
    if action == "*" || action == "s3:*" {
        return PermissionFlags {
            read: false,
            write: false,
            custom: true,
        };
    }

    if action.contains(":get") {
        PermissionFlags {
            read: true,
            write: false,
            custom: false,
        }
    } else if action.contains(":put") || action.contains(":delete") || action.contains("upload") {
        PermissionFlags {
            read: false,
            write: true,
            custom: false,
        }
    } else {
        PermissionFlags {
            read: false,
            write: false,
            custom: false,
        }
    }
}

fn is_public_principal(principal: Option<&serde_json::Value>) -> bool {
    let Some(principal) = principal else {
        return false;
    };

    match principal {
        serde_json::Value::String(value) => value == "*",
        serde_json::Value::Array(values) => values.iter().any(|value| is_public_principal(Some(value))),
        serde_json::Value::Object(values) => {
            values.values().any(|value| is_public_principal(Some(value)))
        }
        _ => false,
    }
}

fn prefixes_overlap(rule_prefix: &str, target_prefix: &str) -> bool {
    if rule_prefix.is_empty() || target_prefix.is_empty() {
        return true;
    }

    let rule_prefix = rule_prefix.trim_end_matches('/');
    let target_prefix = target_prefix.trim_end_matches('/');
    if rule_prefix == target_prefix {
        return true;
    }

    rule_prefix.starts_with(&format!("{target_prefix}/")) || target_prefix.starts_with(&format!("{rule_prefix}/"))
}

fn key_under_prefix(key: &str, prefix: &str) -> bool {
    key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn parse_string_or_array(value: Option<&serde_json::Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };

    match value {
        serde_json::Value::String(value) => vec![value.to_string()],
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(|v| v.as_str().map(ToString::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_statement_list(value: &serde_json::Value) -> Result<Vec<&serde_json::Value>, String> {
    match value {
        serde_json::Value::Array(values) => Ok(values.iter().collect()),
        serde_json::Value::Object(_) => Ok(vec![value]),
        _ => Err("Policy statement must be object or array".to_string()),
    }
}

fn build_public_url(endpoint: &str, bucket: &str, key: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if key.is_empty() {
        format!("{endpoint}/{bucket}")
    } else {
        format!("{endpoint}/{bucket}/{key}")
    }
}

async fn setup_anonymous_client(
    alias_name: &str,
    formatter: &Formatter,
) -> Result<(Alias, S3Client), ExitCode> {
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

    let client = match S3Client::new(alias.clone()).await {
        Ok(client) => client,
        Err(error) => {
            formatter.error(&format!("Failed to create S3 client: {error}"));
            return Err(ExitCode::NetworkError);
        }
    };

    let capabilities = match client.capabilities().await {
        Ok(caps) => caps,
        Err(error) => {
            formatter.error(&format!("Failed to detect capabilities: {error}"));
            return Err(ExitCode::NetworkError);
        }
    };

    if !capabilities.anonymous {
        formatter.error("Backend does not support anonymous access operations");
        return Err(ExitCode::UnsupportedFeature);
    }

    Ok((alias, client))
}

fn exit_code_from_error(error: &rc_core::Error) -> ExitCode {
    ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError)
}

impl AccessTarget {
    fn path_with_bucket_prefix(&self) -> String {
        if self.prefix.is_empty() {
            format!("{}/{}", self.alias, self.bucket)
        } else {
            format!("{}/{}/{}", self.alias, self.bucket, self.prefix)
        }
    }
}

impl AccessRule {
    fn is_read(&self) -> bool {
        self.flags.read || self.flags.custom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_anonymous_path() {
        let target = parse_anonymous_path("local/mybucket").expect("valid target");
        assert_eq!(target.alias, "local");
        assert_eq!(target.bucket, "mybucket");
        assert!(target.prefix.is_empty());

        let target = parse_anonymous_path("local/mybucket/path/prefix").expect("valid target");
        assert_eq!(target.alias, "local");
        assert_eq!(target.prefix, "path/prefix");
    }

    #[test]
    fn test_parse_anonymous_path_errors() {
        assert!(parse_anonymous_path("").is_err());
        assert!(parse_anonymous_path("local").is_err());
        assert!(parse_anonymous_path("/mybucket").is_err());
        assert!(parse_anonymous_path("local/").is_err());
    }

    #[test]
    fn test_parse_permission() {
        assert_eq!(AccessLevel::parse("private").ok(), Some(AccessLevel::Private));
        assert_eq!(AccessLevel::parse("public").ok(), Some(AccessLevel::Public));
        assert_eq!(AccessLevel::parse("download").ok(), Some(AccessLevel::Download));
        assert_eq!(AccessLevel::parse("upload").ok(), Some(AccessLevel::Upload));
        assert!(AccessLevel::parse("invalid").is_err());
    }

    #[test]
    fn test_build_policy() {
        let policy = build_policy(&AccessLevel::Download, "bucket", "photos");
        let value: serde_json::Value = serde_json::from_str(&policy).expect("valid json");
        assert_eq!(value["Version"], "2012-10-17");
        assert_eq!(value["Statement"].as_array().expect("array").len(), 2);
    }

    #[test]
    fn test_build_public_url() {
        let url = build_public_url("http://localhost:9000/", "bucket", "path/to/object.txt");
        assert_eq!(url, "http://localhost:9000/bucket/path/to/object.txt");
    }
}
