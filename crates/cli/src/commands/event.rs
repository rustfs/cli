//! event command - Manage bucket event notifications
//!
//! Add, list, and remove bucket notification rules.

use clap::{Args, Subcommand};
use rc_core::{AliasManager, NotificationRule, NotificationTargetType, ObjectStore as _};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const DEFAULT_EVENT: &str = "s3:ObjectCreated:*";

/// Manage bucket event notifications
#[derive(Args, Debug)]
pub struct EventArgs {
    #[command(subcommand)]
    pub command: EventCommands,
}

#[derive(Subcommand, Debug)]
pub enum EventCommands {
    /// Add a notification rule
    Add(AddEventArgs),

    /// List notification rules
    List(BucketArg),

    /// Remove notification rules by ARN
    Remove(RemoveEventArgs),
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
pub struct AddEventArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Destination ARN (SNS/SQS/Lambda)
    pub arn: String,

    /// Event(s), can be repeated. Defaults to s3:ObjectCreated:*
    #[arg(long = "event", value_name = "EVENT", num_args = 1..)]
    pub events: Vec<String>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RemoveEventArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Destination ARN to remove
    pub arn: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct EventRuleOutput {
    arn: String,
    target: String,
    events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<String>,
}

#[derive(Debug, Serialize)]
struct EventListOutput {
    bucket: String,
    count: usize,
    rules: Vec<EventRuleOutput>,
}

/// Execute the event command
pub async fn execute(args: EventArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        EventCommands::Add(add) => execute_add(add, output_config).await,
        EventCommands::List(list) => execute_list(list, output_config).await,
        EventCommands::Remove(remove) => execute_remove(remove, output_config).await,
    }
}

async fn execute_add(args: AddEventArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parsed) => parsed,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let mut rules = match client.get_bucket_notifications(&bucket).await {
        Ok(rules) => rules,
        Err(error) => {
            formatter.error(&format!("Failed to read current notifications: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let events = normalize_events(args.events);
    let target = match infer_target_type(&args.arn) {
        Ok(target) => target,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    rules.push(NotificationRule {
        id: None,
        arn: args.arn.clone(),
        target,
        events: events.clone(),
        prefix: None,
        suffix: None,
    });

    match client.set_bucket_notifications(&bucket, rules).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&serde_json::json!({
                    "bucket": bucket,
                    "arn": args.arn,
                    "events": events,
                    "status": "added"
                }));
            } else {
                formatter.println(&format!("Added event notification to '{}'", args.path));
                formatter.println(&format!("  ARN: {}", args.arn));
                formatter.println(&format!("  Events: {}", events.join(", ")));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to add notification: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_list(args: BucketArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parsed) => parsed,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_notifications(&bucket).await {
        Ok(rules) => {
            if formatter.is_json() {
                let output = EventListOutput {
                    bucket,
                    count: rules.len(),
                    rules: rules.iter().map(to_output_rule).collect(),
                };
                formatter.json(&output);
            } else if rules.is_empty() {
                formatter.println("No event notifications found.");
            } else {
                formatter.println(&format!("Event notifications for '{}':", args.path));
                for rule in &rules {
                    formatter.println(&format!(
                        "  - [{}] {} => {}",
                        target_type_name(&rule.target),
                        rule.events.join(", "),
                        rule.arn
                    ));
                    if let Some(prefix) = &rule.prefix {
                        formatter.println(&format!("      prefix: {prefix}"));
                    }
                    if let Some(suffix) = &rule.suffix {
                        formatter.println(&format!("      suffix: {suffix}"));
                    }
                }
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to list notifications: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_remove(args: RemoveEventArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parsed) => parsed,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match setup_client(&alias_name, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let rules = match client.get_bucket_notifications(&bucket).await {
        Ok(rules) => rules,
        Err(error) => {
            formatter.error(&format!("Failed to read current notifications: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let (remaining, removed_count) = remove_rules_by_arn(rules, &args.arn);
    if removed_count == 0 {
        formatter.error(&format!(
            "No notification rule found for ARN '{}'",
            args.arn
        ));
        return ExitCode::NotFound;
    }

    match client.set_bucket_notifications(&bucket, remaining).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&serde_json::json!({
                    "bucket": bucket,
                    "arn": args.arn,
                    "removed": removed_count,
                    "status": "removed"
                }));
            } else {
                formatter.println(&format!(
                    "Removed {} notification rule(s) from '{}'",
                    removed_count, args.path
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to remove notification: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn setup_client(
    alias_name: &str,
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

    if !force {
        match client.capabilities().await {
            Ok(caps) => {
                if !caps.notifications {
                    formatter.error(
                        "Backend does not support notifications. Use --force to attempt anyway.",
                    );
                    return Err(ExitCode::UnsupportedFeature);
                }
            }
            Err(error) => {
                formatter.error(&format!("Failed to detect capabilities: {error}"));
                return Err(ExitCode::NetworkError);
            }
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

fn normalize_events(events: Vec<String>) -> Vec<String> {
    if events.is_empty() {
        return vec![DEFAULT_EVENT.to_string()];
    }

    let mut normalized = Vec::new();
    for event in events {
        let trimmed = event.trim();
        if !trimmed.is_empty() {
            normalized.push(trimmed.to_string());
        }
    }

    if normalized.is_empty() {
        vec![DEFAULT_EVENT.to_string()]
    } else {
        normalized
    }
}

fn infer_target_type(arn: &str) -> Result<NotificationTargetType, String> {
    if arn.contains(":sns:") {
        return Ok(NotificationTargetType::Topic);
    }
    if arn.contains(":sqs:") {
        return Ok(NotificationTargetType::Queue);
    }
    if arn.contains(":lambda:") {
        return Ok(NotificationTargetType::Lambda);
    }

    Err(format!(
        "Unsupported ARN type '{}'. Expected SNS, SQS, or Lambda ARN",
        arn
    ))
}

fn remove_rules_by_arn(rules: Vec<NotificationRule>, arn: &str) -> (Vec<NotificationRule>, usize) {
    let original_len = rules.len();
    let remaining = rules
        .into_iter()
        .filter(|rule| rule.arn != arn)
        .collect::<Vec<_>>();
    let removed = original_len.saturating_sub(remaining.len());
    (remaining, removed)
}

fn target_type_name(target: &NotificationTargetType) -> &'static str {
    match target {
        NotificationTargetType::Topic => "topic",
        NotificationTargetType::Queue => "queue",
        NotificationTargetType::Lambda => "lambda",
    }
}

fn to_output_rule(rule: &NotificationRule) -> EventRuleOutput {
    EventRuleOutput {
        id: rule.id.clone(),
        arn: rule.arn.clone(),
        target: target_type_name(&rule.target).to_string(),
        events: rule.events.clone(),
        prefix: rule.prefix.clone(),
        suffix: rule.suffix.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bucket_path() {
        let (alias, bucket) = parse_bucket_path("local/mybucket").expect("valid path");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "mybucket");

        let (alias, bucket) = parse_bucket_path("local/mybucket/").expect("valid path");
        assert_eq!(alias, "local");
        assert_eq!(bucket, "mybucket");
    }

    #[test]
    fn test_parse_bucket_path_errors() {
        assert!(parse_bucket_path("").is_err());
        assert!(parse_bucket_path("local").is_err());
        assert!(parse_bucket_path("/bucket").is_err());
        assert!(parse_bucket_path("local/").is_err());
    }

    #[test]
    fn test_normalize_events() {
        assert_eq!(
            normalize_events(Vec::new()),
            vec![DEFAULT_EVENT.to_string()]
        );
        assert_eq!(
            normalize_events(vec!["".to_string(), "  ".to_string()]),
            vec![DEFAULT_EVENT.to_string()]
        );
        assert_eq!(
            normalize_events(vec!["s3:ObjectRemoved:*".to_string()]),
            vec!["s3:ObjectRemoved:*".to_string()]
        );
    }

    #[test]
    fn test_infer_target_type() {
        assert_eq!(
            infer_target_type("arn:aws:sns:us-east-1:123456789012:my-topic").unwrap(),
            NotificationTargetType::Topic
        );
        assert_eq!(
            infer_target_type("arn:aws:sqs:us-east-1:123456789012:my-queue").unwrap(),
            NotificationTargetType::Queue
        );
        assert_eq!(
            infer_target_type("arn:aws:lambda:us-east-1:123456789012:function:fn").unwrap(),
            NotificationTargetType::Lambda
        );
        assert!(infer_target_type("arn:aws:iam::123456789012:user/demo").is_err());
    }

    #[test]
    fn test_remove_rules_by_arn() {
        let rules = vec![
            NotificationRule {
                id: None,
                arn: "arn:aws:sns:us-east-1:1:a".to_string(),
                target: NotificationTargetType::Topic,
                events: vec![DEFAULT_EVENT.to_string()],
                prefix: None,
                suffix: None,
            },
            NotificationRule {
                id: None,
                arn: "arn:aws:sqs:us-east-1:1:b".to_string(),
                target: NotificationTargetType::Queue,
                events: vec![DEFAULT_EVENT.to_string()],
                prefix: None,
                suffix: None,
            },
            NotificationRule {
                id: Some("id-2".to_string()),
                arn: "arn:aws:sns:us-east-1:1:a".to_string(),
                target: NotificationTargetType::Topic,
                events: vec!["s3:ObjectRemoved:*".to_string()],
                prefix: None,
                suffix: None,
            },
        ];

        let (remaining, removed) = remove_rules_by_arn(rules, "arn:aws:sns:us-east-1:1:a");
        assert_eq!(removed, 2);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].arn, "arn:aws:sqs:us-east-1:1:b");
    }
}
