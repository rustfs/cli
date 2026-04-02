//! event command - Manage bucket notification configuration
//!
//! Add, list, or remove bucket event notification rules.

use clap::{Args, Subcommand};
use rc_core::{AliasManager, BucketNotification, NotificationTarget, ObjectStore as _};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

const EVENT_AFTER_HELP: &str = "\
Examples:
  rc bucket event list local/my-bucket
  rc bucket event add local/my-bucket arn:aws:sqs:us-east-1:123456789012:jobs --event put
  rc event remove local/my-bucket arn:aws:sns:us-east-1:123456789012:alerts";

const EVENT_ADD_AFTER_HELP: &str = "\
Examples:
  rc bucket event add local/my-bucket arn:aws:sqs:us-east-1:123456789012:jobs --event put
  rc event add local/my-bucket arn:aws:sns:us-east-1:123456789012:alerts --event delete
  rc bucket event add local/my-bucket arn:aws:lambda:us-east-1:123456789012:function:thumbnail --event put,delete";

/// Manage bucket event notifications
#[derive(Args, Debug)]
#[command(after_help = EVENT_AFTER_HELP)]
pub struct EventArgs {
    #[command(subcommand)]
    pub command: EventCommands,
}

#[derive(Subcommand, Debug)]
pub enum EventCommands {
    /// Add or replace a bucket notification rule for an ARN
    Add(AddArgs),

    /// List bucket notification rules
    List(BucketArg),

    /// Remove bucket notification rules by target ARN
    Remove(RemoveArgs),
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
#[command(after_help = EVENT_ADD_AFTER_HELP)]
pub struct AddArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Target ARN (SQS/SNS/Lambda)
    pub arn: String,

    /// Event pattern (repeatable; comma-separated values are also accepted)
    #[arg(long = "event", value_name = "EVENT", num_args = 1..)]
    pub events: Vec<String>,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RemoveArgs {
    /// Path to the bucket (alias/bucket)
    pub path: String,

    /// Target ARN to remove
    pub arn: String,

    /// Force operation even if capability detection fails
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct NotificationListOutput {
    bucket: String,
    notifications: Vec<BucketNotification>,
}

#[derive(Debug, Serialize)]
struct NotificationOperationOutput {
    bucket: String,
    arn: String,
    events: Vec<String>,
    action: String,
}

/// Execute the event command
pub async fn execute(args: EventArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        EventCommands::Add(args) => execute_add(args, output_config).await,
        EventCommands::List(args) => execute_list(args, output_config).await,
        EventCommands::Remove(args) => execute_remove(args, output_config).await,
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
                "Use a bucket path in the form alias/bucket before retrying the event command.",
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.get_bucket_notifications(&bucket).await {
        Ok(notifications) => {
            if formatter.is_json() {
                formatter.json(&NotificationListOutput {
                    bucket,
                    notifications,
                });
            } else if notifications.is_empty() {
                formatter.println("No notification rules found.");
            } else {
                formatter.println("Bucket notification rules:");
                for item in notifications {
                    let events = item.events.join(", ");
                    formatter.println(&format!(
                        "  [{}] {} -> {}",
                        item.target_string(),
                        item.arn,
                        events
                    ));
                }
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to list notifications: {error}"),
        ),
    }
}

async fn execute_add(args: AddArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the event command.",
            );
        }
    };

    let target = match infer_target_from_arn(&args.arn) {
        Ok(target) => target,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use an SQS, SNS, or Lambda ARN when adding a bucket notification target.",
            );
        }
    };

    let events = parse_event_list(&args.events);

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let mut notifications = match client.get_bucket_notifications(&bucket).await {
        Ok(items) => items,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read current notifications: {error}"),
            );
        }
    };

    upsert_notification(
        &mut notifications,
        BucketNotification {
            id: None,
            target,
            arn: args.arn.clone(),
            events: events.clone(),
            prefix: None,
            suffix: None,
        },
    );

    match client
        .set_bucket_notifications(&bucket, notifications)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&NotificationOperationOutput {
                    bucket,
                    arn: args.arn,
                    events,
                    action: "added".to_string(),
                });
            } else {
                formatter.success("Notification rule added successfully.");
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to set notification: {error}"),
        ),
    }
}

async fn execute_remove(args: RemoveArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let (alias_name, bucket) = match parse_bucket_path(&args.path) {
        Ok(parts) => parts,
        Err(error) => {
            return formatter.fail_with_suggestion(
                ExitCode::UsageError,
                &error,
                "Use a bucket path in the form alias/bucket before retrying the event command.",
            );
        }
    };

    let client = match setup_client(&alias_name, &bucket, args.force, &formatter).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let mut notifications = match client.get_bucket_notifications(&bucket).await {
        Ok(items) => items,
        Err(error) => {
            return formatter.fail(
                ExitCode::GeneralError,
                &format!("Failed to read current notifications: {error}"),
            );
        }
    };

    let removed = remove_notifications_by_arn(&mut notifications, &args.arn);
    if removed == 0 {
        return formatter.fail_with_suggestion(
            ExitCode::NotFound,
            &format!("Notification target '{}' not found", args.arn),
            "Run `rc bucket event list <alias/bucket>` to inspect the configured notification targets.",
        );
    }

    match client
        .set_bucket_notifications(&bucket, notifications)
        .await
    {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&NotificationOperationOutput {
                    bucket,
                    arn: args.arn,
                    events: Vec::new(),
                    action: "removed".to_string(),
                });
            } else {
                formatter.success("Notification rule removed successfully.");
            }
            ExitCode::Success
        }
        Err(error) => formatter.fail(
            ExitCode::GeneralError,
            &format!("Failed to update notifications: {error}"),
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
            formatter.error(&format!("Failed to load aliases: {error}"));
            return Err(ExitCode::GeneralError);
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

    if !force && !caps.notifications {
        return Err(formatter.fail_with_suggestion(
            ExitCode::UnsupportedFeature,
            "Backend does not support notifications. Use --force to attempt anyway.",
            "Retry with --force only if you know the backend supports bucket notifications.",
        ));
    }

    match client.bucket_exists(bucket).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(formatter.fail_with_suggestion(
                ExitCode::NotFound,
                &format!("Bucket '{bucket}' does not exist"),
                "Check the bucket path and retry the event command.",
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

    Ok((
        parts[0].to_string(),
        parts[1].trim_end_matches('/').to_string(),
    ))
}

fn parse_event_list(values: &[String]) -> Vec<String> {
    let mut events: Vec<String> = values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_event_name)
        .collect();

    if events.is_empty() {
        events.push("s3:ObjectCreated:*".to_string());
    }

    events.sort();
    events.dedup();
    events
}

fn normalize_event_name(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "put" => "s3:ObjectCreated:*".to_string(),
        "get" => "s3:ObjectAccessed:*".to_string(),
        "delete" => "s3:ObjectRemoved:*".to_string(),
        "replica" => "s3:Replication:*".to_string(),
        "ilm" => "s3:ObjectTransition:*".to_string(),
        _ => value.to_string(),
    }
}

fn infer_target_from_arn(arn: &str) -> Result<NotificationTarget, String> {
    if arn.contains(":sqs:") {
        Ok(NotificationTarget::Queue)
    } else if arn.contains(":sns:") {
        Ok(NotificationTarget::Topic)
    } else if arn.contains(":lambda:") {
        Ok(NotificationTarget::Lambda)
    } else {
        Err(format!(
            "Unsupported ARN '{}'. Expected SQS/SNS/Lambda ARN",
            arn
        ))
    }
}

fn upsert_notification(notifications: &mut Vec<BucketNotification>, new_rule: BucketNotification) {
    notifications.retain(|item| item.arn != new_rule.arn);
    notifications.push(new_rule);
}

fn remove_notifications_by_arn(notifications: &mut Vec<BucketNotification>, arn: &str) -> usize {
    let before = notifications.len();
    notifications.retain(|item| item.arn != arn);
    before.saturating_sub(notifications.len())
}

trait NotificationTargetDisplay {
    fn target_string(&self) -> &'static str;
}

impl NotificationTargetDisplay for BucketNotification {
    fn target_string(&self) -> &'static str {
        match self.target {
            NotificationTarget::Queue => "queue",
            NotificationTarget::Topic => "topic",
            NotificationTarget::Lambda => "lambda",
        }
    }
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
    }

    #[test]
    fn test_parse_event_list_defaults_and_deduplicates() {
        assert_eq!(
            parse_event_list(&[]),
            vec!["s3:ObjectCreated:*".to_string()]
        );

        let events = parse_event_list(&[
            "s3:ObjectCreated:Put,s3:ObjectRemoved:*".to_string(),
            "s3:ObjectCreated:Put".to_string(),
        ]);

        assert_eq!(
            events,
            vec![
                "s3:ObjectCreated:Put".to_string(),
                "s3:ObjectRemoved:*".to_string()
            ]
        );
    }

    #[test]
    fn test_parse_event_list_normalizes_shorthand_names() {
        let events = parse_event_list(&[
            "put,get,delete".to_string(),
            "replica".to_string(),
            "ilm,s3:ObjectCreated:Post".to_string(),
            "PUT".to_string(),
        ]);

        assert_eq!(
            events,
            vec![
                "s3:ObjectAccessed:*".to_string(),
                "s3:ObjectCreated:*".to_string(),
                "s3:ObjectCreated:Post".to_string(),
                "s3:ObjectRemoved:*".to_string(),
                "s3:ObjectTransition:*".to_string(),
                "s3:Replication:*".to_string(),
            ]
        );
    }

    #[test]
    fn test_upsert_notification_replaces_existing_rule_for_same_arn() {
        let mut rules = vec![BucketNotification {
            id: None,
            target: NotificationTarget::Queue,
            arn: "arn:aws:sqs:us-east-1:123456789012:q1".to_string(),
            events: vec!["s3:ObjectRemoved:*".to_string()],
            prefix: None,
            suffix: None,
        }];

        let normalized_events =
            parse_event_list(&["put,s3:ObjectCreated:*".to_string(), "PUT".to_string()]);

        upsert_notification(
            &mut rules,
            BucketNotification {
                id: None,
                target: NotificationTarget::Queue,
                arn: "arn:aws:sqs:us-east-1:123456789012:q1".to_string(),
                events: normalized_events,
                prefix: None,
                suffix: None,
            },
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].events, vec!["s3:ObjectCreated:*".to_string()]);
    }

    #[test]
    fn test_parse_event_list_deduplicates_shorthand_and_canonical_names() {
        let events = parse_event_list(&[
            "put,s3:ObjectCreated:*".to_string(),
            "GET,s3:ObjectAccessed:*".to_string(),
            "delete,s3:ObjectRemoved:*".to_string(),
        ]);

        assert_eq!(
            events,
            vec![
                "s3:ObjectAccessed:*".to_string(),
                "s3:ObjectCreated:*".to_string(),
                "s3:ObjectRemoved:*".to_string(),
            ]
        );
    }

    #[test]
    fn test_normalize_event_name_preserves_non_shorthand_values() {
        assert_eq!(
            normalize_event_name("s3:ObjectCreated:Post"),
            "s3:ObjectCreated:Post"
        );
        assert_eq!(normalize_event_name("custom:event"), "custom:event");
    }

    #[test]
    fn test_infer_target_from_arn() {
        assert_eq!(
            infer_target_from_arn("arn:aws:sqs:us-east-1:123456789012:queue").expect("queue"),
            NotificationTarget::Queue
        );
        assert_eq!(
            infer_target_from_arn("arn:aws:sns:us-east-1:123456789012:topic").expect("topic"),
            NotificationTarget::Topic
        );
        assert_eq!(
            infer_target_from_arn("arn:aws:lambda:us-east-1:123456789012:function:fn")
                .expect("lambda"),
            NotificationTarget::Lambda
        );
        assert!(infer_target_from_arn("arn:aws:iam::123456789012:role/demo").is_err());
    }

    #[test]
    fn test_upsert_and_remove_notification() {
        let mut rules = vec![BucketNotification {
            id: None,
            target: NotificationTarget::Queue,
            arn: "arn:aws:sqs:us-east-1:123456789012:q1".to_string(),
            events: vec!["s3:ObjectCreated:*".to_string()],
            prefix: None,
            suffix: None,
        }];

        upsert_notification(
            &mut rules,
            BucketNotification {
                id: None,
                target: NotificationTarget::Topic,
                arn: "arn:aws:sns:us-east-1:123456789012:t1".to_string(),
                events: vec!["s3:ObjectRemoved:*".to_string()],
                prefix: None,
                suffix: None,
            },
        );
        assert_eq!(rules.len(), 2);

        let removed =
            remove_notifications_by_arn(&mut rules, "arn:aws:sns:us-east-1:123456789012:t1");
        assert_eq!(removed, 1);
        assert_eq!(rules.len(), 1);
    }
}
