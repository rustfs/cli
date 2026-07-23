//! Object retention commands with `mc retention` compatible entry points.

use std::time::SystemTime;

use clap::{Args, Subcommand};
use jiff::{Span, Timestamp, tz::TimeZone};
use rc_core::{
    ObjectLockOptions, ObjectRetention, ObjectStore as _, RetentionDuration, RetentionDurationUnit,
    RetentionMode, parse_object_path,
};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

use super::lock::{
    self, BucketLockArg, LockArgs, LockCommands, LockStateOutput, LocksData, RetentionModeArg,
    SetBucketLockArgs, emit_lock_output, fail_core_lock, fail_lock, setup_client,
};

const RETENTION_CAPABILITY: &str = "object_retention";

/// Manage object retention or a bucket's default retention rule.
#[derive(Args, Debug)]
pub struct RetentionArgs {
    #[command(subcommand)]
    pub command: RetentionCommands,
}

#[derive(Subcommand, Debug)]
pub enum RetentionCommands {
    /// Show retention for an object version or a bucket default.
    Info(InfoRetentionArgs),
    /// Set retention for an object version or a bucket default.
    Set(SetRetentionArgs),
    /// Clear retention for an object version or a bucket default.
    Clear(ClearRetentionArgs),
}

#[derive(Args, Debug)]
pub struct InfoRetentionArgs {
    /// Object path, or a bucket path when --default is used.
    pub path: String,
    /// Read one exact object version.
    #[arg(long, alias = "vid")]
    pub version_id: Option<String>,
    /// Read bucket default retention instead of object retention.
    #[arg(long)]
    pub default: bool,
}

#[derive(Args, Debug)]
pub struct SetRetentionArgs {
    /// Retention mode.
    pub mode: RetentionModeArg,
    /// Positive validity (for example 30d or 2y) or an absolute UTC RFC3339 timestamp.
    pub validity: String,
    /// Object path, or a bucket path when --default is used.
    pub path: String,
    /// Mutate one exact object version.
    #[arg(long, alias = "vid")]
    pub version_id: Option<String>,
    /// Explicitly request governance retention bypass.
    #[arg(long)]
    pub bypass: bool,
    /// Set bucket default retention instead of object retention.
    #[arg(long)]
    pub default: bool,
}

#[derive(Args, Debug)]
pub struct ClearRetentionArgs {
    /// Object path, or a bucket path when --default is used.
    pub path: String,
    /// Mutate one exact object version.
    #[arg(long, alias = "vid")]
    pub version_id: Option<String>,
    /// Explicitly request governance retention bypass.
    #[arg(long)]
    pub bypass: bool,
    /// Clear bucket default retention instead of object retention.
    #[arg(long)]
    pub default: bool,
}

pub async fn execute(args: RetentionArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        RetentionCommands::Info(args) => execute_info(args, output_config).await,
        RetentionCommands::Set(args) => execute_set(args, output_config).await,
        RetentionCommands::Clear(args) => execute_clear(args, output_config).await,
    }
}

async fn execute_info(args: InfoRetentionArgs, output_config: OutputConfig) -> ExitCode {
    if args.default {
        if args.version_id.is_some() {
            let formatter = Formatter::new(output_config);
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                "--default cannot be combined with --version-id",
                RETENTION_CAPABILITY,
            );
        }
        return lock::execute(
            LockArgs {
                command: LockCommands::Info(BucketLockArg { path: args.path }),
            },
            output_config,
        )
        .await;
    }

    let formatter = Formatter::new(output_config);
    let path = match parse_object_path(&args.path) {
        Ok(path) => path,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error.to_string(),
                RETENTION_CAPABILITY,
            );
        }
    };
    let options = match ObjectLockOptions::new(args.version_id, false) {
        Ok(options) => options,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error.to_string(),
                RETENTION_CAPABILITY,
            );
        }
    };
    let client = match setup_client(&path.alias, &formatter, RETENTION_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.get_object_retention(&path, &options).await {
        Ok(retention) => emit_lock_output(
            &formatter,
            LocksData::one(
                "retention_info",
                false,
                LockStateOutput::retention(&path, options.version_id, retention),
            ),
        ),
        Err(error) => fail_core_lock(
            &formatter,
            &error,
            "Failed to get object retention",
            RETENTION_CAPABILITY,
        ),
    }
}

async fn execute_set(args: SetRetentionArgs, output_config: OutputConfig) -> ExitCode {
    if args.default {
        return execute_set_default(args, output_config).await;
    }

    let formatter = Formatter::new(output_config);
    let now = match current_timestamp() {
        Ok(now) => now,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::GeneralError,
                &error,
                RETENTION_CAPABILITY,
            );
        }
    };
    let retain_until = match parse_object_validity(&args.validity, now) {
        Ok(retain_until) => retain_until,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error,
                RETENTION_CAPABILITY,
            );
        }
    };
    let path = match parse_object_path(&args.path) {
        Ok(path) => path,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error.to_string(),
                RETENTION_CAPABILITY,
            );
        }
    };
    let options = match ObjectLockOptions::new(args.version_id, args.bypass) {
        Ok(options) => options,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error.to_string(),
                RETENTION_CAPABILITY,
            );
        }
    };
    let requested = ObjectRetention {
        mode: args.mode.into(),
        retain_until,
    };
    let client = match setup_client(&path.alias, &formatter, RETENTION_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let existing = match client.get_object_retention(&path, &options).await {
        Ok(existing) => existing,
        Err(error) => {
            return fail_core_lock(
                &formatter,
                &error,
                "Failed to inspect existing object retention",
                RETENTION_CAPABILITY,
            );
        }
    };
    if let Err(error) =
        validate_retention_change(existing.as_ref(), Some(&requested), &options, now)
    {
        return fail_lock(&formatter, ExitCode::Conflict, &error, RETENTION_CAPABILITY);
    }
    if let Err(error) = client
        .put_object_retention(&path, Some(requested.clone()), &options)
        .await
    {
        return fail_core_lock(
            &formatter,
            &error,
            "Failed to set object retention",
            RETENTION_CAPABILITY,
        );
    }
    emit_lock_output(
        &formatter,
        LocksData::one(
            "retention_set",
            true,
            LockStateOutput::retention(&path, options.version_id, Some(requested)),
        ),
    )
}

async fn execute_set_default(args: SetRetentionArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config.clone());
    if args.version_id.is_some() || args.bypass {
        return fail_lock(
            &formatter,
            ExitCode::UsageError,
            "--default cannot be combined with --version-id or --bypass",
            RETENTION_CAPABILITY,
        );
    }
    let duration = match parse_default_validity(&args.validity) {
        Ok(duration) => duration,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error,
                RETENTION_CAPABILITY,
            );
        }
    };
    let (days, years) = match duration.unit {
        RetentionDurationUnit::Days => (Some(duration.value), None),
        RetentionDurationUnit::Years => (None, Some(duration.value)),
    };
    lock::execute(
        LockArgs {
            command: LockCommands::Set(SetBucketLockArgs {
                path: args.path,
                mode: args.mode,
                days,
                years,
            }),
        },
        output_config,
    )
    .await
}

async fn execute_clear(args: ClearRetentionArgs, output_config: OutputConfig) -> ExitCode {
    if args.default {
        let formatter = Formatter::new(output_config.clone());
        if args.version_id.is_some() || args.bypass {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                "--default cannot be combined with --version-id or --bypass",
                RETENTION_CAPABILITY,
            );
        }
        return lock::execute(
            LockArgs {
                command: LockCommands::Clear(BucketLockArg { path: args.path }),
            },
            output_config,
        )
        .await;
    }

    let formatter = Formatter::new(output_config);
    let now = match current_timestamp() {
        Ok(now) => now,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::GeneralError,
                &error,
                RETENTION_CAPABILITY,
            );
        }
    };
    let path = match parse_object_path(&args.path) {
        Ok(path) => path,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error.to_string(),
                RETENTION_CAPABILITY,
            );
        }
    };
    let options = match ObjectLockOptions::new(args.version_id, args.bypass) {
        Ok(options) => options,
        Err(error) => {
            return fail_lock(
                &formatter,
                ExitCode::UsageError,
                &error.to_string(),
                RETENTION_CAPABILITY,
            );
        }
    };
    let client = match setup_client(&path.alias, &formatter, RETENTION_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let existing = match client.get_object_retention(&path, &options).await {
        Ok(existing) => existing,
        Err(error) => {
            return fail_core_lock(
                &formatter,
                &error,
                "Failed to inspect existing object retention",
                RETENTION_CAPABILITY,
            );
        }
    };
    if let Err(error) = validate_retention_change(existing.as_ref(), None, &options, now) {
        return fail_lock(&formatter, ExitCode::Conflict, &error, RETENTION_CAPABILITY);
    }
    if let Err(error) = client.put_object_retention(&path, None, &options).await {
        return fail_core_lock(
            &formatter,
            &error,
            "Failed to clear object retention",
            RETENTION_CAPABILITY,
        );
    }
    emit_lock_output(
        &formatter,
        LocksData::one(
            "retention_clear",
            true,
            LockStateOutput::retention(&path, options.version_id, None),
        ),
    )
}

fn current_timestamp() -> Result<Timestamp, String> {
    Timestamp::try_from(SystemTime::now())
        .map_err(|error| format!("System clock is outside the supported UTC range: {error}"))
}

fn parse_object_validity(value: &str, now: Timestamp) -> Result<Timestamp, String> {
    if let Some(duration) = parse_duration_syntax(value)? {
        let span = match duration.unit {
            RetentionDurationUnit::Days => {
                Span::new().try_days(duration.value).map_err(|error| {
                    format!("Retention duration is outside the supported range: {error}")
                })?
            }
            RetentionDurationUnit::Years => {
                Span::new().try_years(duration.value).map_err(|error| {
                    format!("Retention duration is outside the supported range: {error}")
                })?
            }
        };
        return now
            .to_zoned(TimeZone::UTC)
            .checked_add(span)
            .map(|value| value.timestamp())
            .map_err(|error| format!("Retention date overflows the supported UTC range: {error}"));
    }

    if !value.ends_with('Z') {
        return Err(
            "Retention validity must be a positive Nd/Ny duration or an RFC3339 UTC timestamp ending in 'Z'"
                .to_string(),
        );
    }
    let timestamp: Timestamp = value
        .parse()
        .map_err(|error| format!("Invalid retention date '{value}': {error}"))?;
    if timestamp <= now {
        return Err("Retention date must be in the future".to_string());
    }
    Ok(timestamp)
}

fn parse_default_validity(value: &str) -> Result<RetentionDuration, String> {
    parse_duration_syntax(value)?.ok_or_else(|| {
        "Bucket default retention validity must use an explicit day or year suffix (for example 30d or 2y)".to_string()
    })
}

fn parse_duration_syntax(value: &str) -> Result<Option<RetentionDuration>, String> {
    let Some(unit) = value.chars().last() else {
        return Err("Retention validity cannot be empty".to_string());
    };
    if !matches!(unit, 'd' | 'y') {
        return Ok(None);
    }
    let number = &value[..value.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Retention validity must be a positive integer followed by d or y".to_string());
    }
    let value: i32 = number
        .parse()
        .map_err(|_| "Retention duration is outside the supported range".to_string())?;
    let duration = match unit {
        'd' => RetentionDuration::days(value),
        'y' => RetentionDuration::years(value),
        _ => unreachable!("duration suffix is checked above"),
    }
    .map_err(|error| error.to_string())?;
    Ok(Some(duration))
}

fn validate_retention_change(
    existing: Option<&ObjectRetention>,
    requested: Option<&ObjectRetention>,
    options: &ObjectLockOptions,
    now: Timestamp,
) -> Result<(), String> {
    let Some(existing) = existing.filter(|retention| retention.retain_until > now) else {
        return Ok(());
    };

    match existing.mode {
        RetentionMode::Compliance => {
            let may_extend = requested.is_some_and(|retention| {
                retention.mode == RetentionMode::Compliance
                    && retention.retain_until >= existing.retain_until
            });
            if may_extend {
                Ok(())
            } else {
                Err(format!(
                    "Active compliance retention cannot be cleared, shortened, or changed before {}",
                    existing.retain_until
                ))
            }
        }
        RetentionMode::Governance => {
            let weakens = requested.is_none_or(|retention| {
                retention.mode != RetentionMode::Governance
                    || retention.retain_until < existing.retain_until
            });
            if weakens && !options.bypass_governance {
                Err("Shortening, clearing, or changing active governance retention requires the explicit --bypass flag".to_string())
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(value: &str) -> Timestamp {
        value.parse().expect("valid test timestamp")
    }

    #[test]
    fn validity_supports_mc_durations_and_utc_dates() {
        let now = timestamp("2026-07-21T00:00:00Z");
        assert_eq!(
            parse_object_validity("1d", now)
                .expect("one day")
                .to_string(),
            "2026-07-22T00:00:00Z"
        );
        assert_eq!(
            parse_object_validity("1y", now)
                .expect("one year")
                .to_string(),
            "2027-07-21T00:00:00Z"
        );
        assert!(parse_object_validity("2020-01-01T00:00:00Z", now).is_err());
        assert!(parse_object_validity("2027-01-01T00:00:00+08:00", now).is_err());
        assert!(parse_object_validity("999999999999999999y", now).is_err());
    }

    #[test]
    fn bucket_default_validity_requires_explicit_days_or_years() {
        assert_eq!(
            parse_default_validity("30d")
                .expect("valid day default")
                .unit,
            RetentionDurationUnit::Days
        );
        assert_eq!(
            parse_default_validity("2y")
                .expect("valid year default")
                .unit,
            RetentionDurationUnit::Years
        );
        assert!(parse_default_validity("1m").is_err());
        assert!(parse_default_validity("0d").is_err());
        assert!(parse_default_validity("2099-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn compliance_cannot_be_weakened_even_with_bypass() {
        let now = timestamp("2026-07-21T00:00:00Z");
        let existing = ObjectRetention {
            mode: RetentionMode::Compliance,
            retain_until: timestamp("2027-07-21T00:00:00Z"),
        };
        let shorter = ObjectRetention {
            mode: RetentionMode::Compliance,
            retain_until: timestamp("2027-07-20T00:00:00Z"),
        };
        let options = ObjectLockOptions::new(None, true).expect("valid options");
        assert!(validate_retention_change(Some(&existing), Some(&shorter), &options, now).is_err());
        assert!(validate_retention_change(Some(&existing), None, &options, now).is_err());
    }

    #[test]
    fn governance_weakening_requires_explicit_bypass() {
        let now = timestamp("2026-07-21T00:00:00Z");
        let existing = ObjectRetention {
            mode: RetentionMode::Governance,
            retain_until: timestamp("2027-07-21T00:00:00Z"),
        };
        let implicit = ObjectLockOptions::new(None, false).expect("valid options");
        let explicit = ObjectLockOptions::new(None, true).expect("valid options");
        assert!(validate_retention_change(Some(&existing), None, &implicit, now).is_err());
        assert!(validate_retention_change(Some(&existing), None, &explicit, now).is_ok());
    }
}
