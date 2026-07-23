//! Bucket Object Lock commands and shared WORM command output helpers.

use clap::{Args, Subcommand, ValueEnum};
use comfy_table::Table;
use rc_core::{
    AliasManager, BucketObjectLockConfiguration, DefaultRetention, ObjectRetention,
    ObjectStore as _, ParsedPath, RemotePath, RetentionDuration, RetentionMode, parse_path,
};
use rc_s3::S3Client;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig, V3ErrorEnvelope, V3SuccessEnvelope};

const LOCK_CAPABILITY: &str = "object_lock";

/// Manage bucket Object Lock configuration.
#[derive(Args, Debug)]
pub struct LockArgs {
    #[command(subcommand)]
    pub command: LockCommands,
}

#[derive(Subcommand, Debug)]
pub enum LockCommands {
    /// Show bucket Object Lock and default retention configuration.
    Info(BucketLockArg),
    /// Set a bucket default retention rule.
    Set(SetBucketLockArgs),
    /// Clear the bucket default retention rule without disabling Object Lock.
    Clear(BucketLockArg),
}

#[derive(Args, Debug)]
pub struct BucketLockArg {
    /// Bucket path in alias/bucket form.
    pub path: String,
}

#[derive(Args, Debug)]
pub struct SetBucketLockArgs {
    /// Bucket path in alias/bucket form.
    pub path: String,
    /// Default retention mode.
    #[arg(long)]
    pub mode: RetentionModeArg,
    /// Positive default retention duration in days.
    #[arg(long, conflicts_with = "years", required_unless_present = "years")]
    pub days: Option<i32>,
    /// Positive default retention duration in years.
    #[arg(long, conflicts_with = "days", required_unless_present = "days")]
    pub years: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RetentionModeArg {
    Governance,
    Compliance,
}

impl From<RetentionModeArg> for RetentionMode {
    fn from(value: RetentionModeArg) -> Self {
        match value {
            RetentionModeArg::Governance => Self::Governance,
            RetentionModeArg::Compliance => Self::Compliance,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct LocksData {
    operation: &'static str,
    changed: bool,
    items: Vec<LockStateOutput>,
}

impl LocksData {
    pub(crate) fn one(operation: &'static str, changed: bool, item: LockStateOutput) -> Self {
        Self {
            operation,
            changed,
            items: vec![item],
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct LockStateOutput {
    bucket: String,
    key: String,
    version_id: Option<String>,
    object_lock_enabled: bool,
    retention: Option<ObjectRetention>,
    legal_hold: Option<bool>,
    default_retention: Option<DefaultRetention>,
}

impl LockStateOutput {
    pub(crate) fn bucket(
        bucket: String,
        configuration: Option<BucketObjectLockConfiguration>,
    ) -> Self {
        let configuration = configuration.unwrap_or(BucketObjectLockConfiguration {
            enabled: false,
            default_retention: None,
        });
        Self {
            bucket,
            key: String::new(),
            version_id: None,
            object_lock_enabled: configuration.enabled,
            retention: None,
            legal_hold: None,
            default_retention: configuration.default_retention,
        }
    }

    pub(crate) fn retention(
        path: &RemotePath,
        version_id: Option<String>,
        retention: Option<ObjectRetention>,
    ) -> Self {
        Self {
            bucket: path.bucket.clone(),
            key: path.key.clone(),
            version_id,
            object_lock_enabled: true,
            retention,
            legal_hold: None,
            default_retention: None,
        }
    }

    pub(crate) fn legal_hold(path: &RemotePath, version_id: Option<String>, enabled: bool) -> Self {
        Self {
            bucket: path.bucket.clone(),
            key: path.key.clone(),
            version_id,
            object_lock_enabled: true,
            retention: None,
            legal_hold: Some(enabled),
            default_retention: None,
        }
    }
}

pub async fn execute(args: LockArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        LockCommands::Info(args) => execute_info(args, output_config).await,
        LockCommands::Set(args) => execute_set(args, output_config).await,
        LockCommands::Clear(args) => execute_clear(args, output_config).await,
    }
}

async fn execute_info(args: BucketLockArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let path = match parse_bucket_path(&args.path) {
        Ok(path) => path,
        Err(error) => return fail_lock(&formatter, ExitCode::UsageError, &error, LOCK_CAPABILITY),
    };
    let client = match setup_client(&path.alias, &formatter, LOCK_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client
        .get_bucket_object_lock_configuration(&path.bucket)
        .await
    {
        Ok(configuration) => emit_lock_output(
            &formatter,
            LocksData {
                operation: "bucket_lock_info",
                changed: false,
                items: vec![LockStateOutput::bucket(path.bucket, configuration)],
            },
        ),
        Err(error) => fail_core_lock(
            &formatter,
            &error,
            "Failed to get bucket Object Lock configuration",
            LOCK_CAPABILITY,
        ),
    }
}

async fn execute_set(args: SetBucketLockArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let path = match parse_bucket_path(&args.path) {
        Ok(path) => path,
        Err(error) => return fail_lock(&formatter, ExitCode::UsageError, &error, LOCK_CAPABILITY),
    };
    let duration = match parse_bucket_duration(args.days, args.years) {
        Ok(duration) => duration,
        Err(error) => return fail_lock(&formatter, ExitCode::UsageError, &error, LOCK_CAPABILITY),
    };
    let client = match setup_client(&path.alias, &formatter, LOCK_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let existing = match client
        .get_bucket_object_lock_configuration(&path.bucket)
        .await
    {
        Ok(Some(configuration)) if configuration.enabled => configuration,
        Ok(_) => {
            return fail_lock(
                &formatter,
                ExitCode::Conflict,
                "Object Lock must be enabled when the bucket is created before its default retention can be updated",
                LOCK_CAPABILITY,
            );
        }
        Err(error) => {
            return fail_core_lock(
                &formatter,
                &error,
                "Failed to inspect bucket Object Lock configuration",
                LOCK_CAPABILITY,
            );
        }
    };
    let requested = BucketObjectLockConfiguration {
        enabled: existing.enabled,
        default_retention: Some(DefaultRetention {
            mode: args.mode.into(),
            duration,
        }),
    };
    if let Err(error) = client
        .put_bucket_object_lock_configuration(&path.bucket, requested)
        .await
    {
        return fail_core_lock(
            &formatter,
            &error,
            "Failed to set bucket Object Lock configuration",
            LOCK_CAPABILITY,
        );
    }
    emit_bucket_round_trip(&client, &formatter, path.bucket, "bucket_lock_set").await
}

async fn execute_clear(args: BucketLockArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let path = match parse_bucket_path(&args.path) {
        Ok(path) => path,
        Err(error) => return fail_lock(&formatter, ExitCode::UsageError, &error, LOCK_CAPABILITY),
    };
    let client = match setup_client(&path.alias, &formatter, LOCK_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let existing = match client
        .get_bucket_object_lock_configuration(&path.bucket)
        .await
    {
        Ok(Some(configuration)) if configuration.enabled => configuration,
        Ok(_) => {
            return fail_lock(
                &formatter,
                ExitCode::Conflict,
                "Object Lock is not enabled for this bucket",
                LOCK_CAPABILITY,
            );
        }
        Err(error) => {
            return fail_core_lock(
                &formatter,
                &error,
                "Failed to inspect bucket Object Lock configuration",
                LOCK_CAPABILITY,
            );
        }
    };
    let requested = BucketObjectLockConfiguration {
        enabled: existing.enabled,
        default_retention: None,
    };
    if let Err(error) = client
        .put_bucket_object_lock_configuration(&path.bucket, requested)
        .await
    {
        return fail_core_lock(
            &formatter,
            &error,
            "Failed to clear bucket default retention",
            LOCK_CAPABILITY,
        );
    }
    emit_bucket_round_trip(&client, &formatter, path.bucket, "bucket_lock_clear").await
}

async fn emit_bucket_round_trip(
    client: &S3Client,
    formatter: &Formatter,
    bucket: String,
    operation: &'static str,
) -> ExitCode {
    match client.get_bucket_object_lock_configuration(&bucket).await {
        Ok(configuration) => emit_lock_output(
            formatter,
            LocksData {
                operation,
                changed: true,
                items: vec![LockStateOutput::bucket(bucket, configuration)],
            },
        ),
        Err(error) => fail_core_lock(
            formatter,
            &error,
            "Bucket Object Lock changed, but the updated configuration could not be read back",
            LOCK_CAPABILITY,
        ),
    }
}

fn parse_bucket_duration(
    days: Option<i32>,
    years: Option<i32>,
) -> Result<RetentionDuration, String> {
    match (days, years) {
        (Some(days), None) => RetentionDuration::days(days).map_err(|error| error.to_string()),
        (None, Some(years)) => RetentionDuration::years(years).map_err(|error| error.to_string()),
        (Some(_), Some(_)) => Err("Specify only one of --days or --years".to_string()),
        (None, None) => Err("Specify exactly one of --days or --years".to_string()),
    }
}

pub(crate) fn parse_bucket_path(value: &str) -> Result<RemotePath, String> {
    match parse_path(value).map_err(|error| error.to_string())? {
        ParsedPath::Remote(path) if path.key.is_empty() => Ok(path),
        _ => Err("Bucket path must use the form alias/bucket".to_string()),
    }
}

pub(crate) async fn setup_client(
    alias_name: &str,
    formatter: &Formatter,
    capability: &str,
) -> Result<S3Client, ExitCode> {
    let manager = AliasManager::new()
        .map_err(|error| fail_core_lock(formatter, &error, "Failed to load aliases", capability))?;
    let alias = manager.get(alias_name).map_err(|error| {
        fail_core_lock(formatter, &error, "Failed to resolve alias", capability)
    })?;
    S3Client::new(alias).await.map_err(|error| {
        fail_core_lock(formatter, &error, "Failed to create S3 client", capability)
    })
}

pub(crate) fn emit_lock_output(formatter: &Formatter, data: LocksData) -> ExitCode {
    if formatter.is_json() {
        formatter.json(&V3SuccessEnvelope::locks(&data));
    } else {
        formatter.println(&render_lock_table(&data.items, formatter));
    }
    ExitCode::Success
}

fn render_lock_table(items: &[LockStateOutput], formatter: &Formatter) -> String {
    let mut table = Table::new();
    table.set_header([
        "BUCKET",
        "OBJECT",
        "VERSION",
        "LOCK",
        "RETENTION",
        "RETAIN UNTIL",
        "LEGAL HOLD",
        "DEFAULT",
    ]);
    for item in items {
        let bucket = formatter.sanitize_text(&item.bucket);
        let key = formatter.sanitize_text(&item.key);
        let version_id = item
            .version_id
            .as_deref()
            .map(|value| formatter.sanitize_text(value));
        let (mode, retain_until) = item
            .retention
            .as_ref()
            .map(|retention| {
                (
                    retention.mode.to_string().to_ascii_uppercase(),
                    retention.retain_until.to_string(),
                )
            })
            .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
        let legal_hold = match item.legal_hold {
            Some(true) => "ON",
            Some(false) => "OFF",
            None => "-",
        };
        let default = item
            .default_retention
            .as_ref()
            .map(|retention| {
                format!(
                    "{} {} {}",
                    retention.mode.to_string().to_ascii_uppercase(),
                    retention.duration.value,
                    retention.duration.unit
                )
            })
            .unwrap_or_else(|| "-".to_string());
        table.add_row([
            bucket.as_str(),
            if key.is_empty() { "-" } else { key.as_str() },
            version_id.as_deref().unwrap_or("-"),
            if item.object_lock_enabled {
                "ENABLED"
            } else {
                "DISABLED"
            },
            mode.as_str(),
            retain_until.as_str(),
            legal_hold,
            default.as_str(),
        ]);
    }
    table.to_string()
}

pub(crate) fn fail_core_lock(
    formatter: &Formatter,
    error: &rc_core::Error,
    context: &str,
    capability: &str,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    fail_lock(formatter, code, &format!("{context}: {error}"), capability)
}

pub(crate) fn fail_lock(
    formatter: &Formatter,
    code: ExitCode,
    message: &str,
    capability: &str,
) -> ExitCode {
    if formatter.is_json() {
        formatter.json_error(&V3ErrorEnvelope::locks(code, message, Some(capability)));
        code
    } else {
        formatter.fail(code, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_duration_requires_one_positive_unit() {
        assert_eq!(
            parse_bucket_duration(Some(30), None)
                .expect("valid days")
                .unit,
            rc_core::RetentionDurationUnit::Days
        );
        assert!(parse_bucket_duration(Some(1), Some(1)).is_err());
        assert!(parse_bucket_duration(None, None).is_err());
        assert!(parse_bucket_duration(Some(0), None).is_err());
    }

    #[test]
    fn bucket_paths_cannot_select_objects() {
        assert!(parse_bucket_path("local/bucket").is_ok());
        assert!(parse_bucket_path("local/bucket/key").is_err());
    }
}
