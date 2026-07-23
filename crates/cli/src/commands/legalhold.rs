//! Object legal-hold commands with `mc legalhold` compatible entry points.

use clap::{Args, Subcommand};
use rc_core::{LegalHoldStatus, ObjectLockOptions, ObjectStore as _, parse_object_path};

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

use super::lock::{
    LockStateOutput, LocksData, emit_lock_output, fail_core_lock, fail_lock, setup_client,
};

const LEGAL_HOLD_CAPABILITY: &str = "object_legal_hold";

/// Manage legal hold for an object version.
#[derive(Args, Debug)]
pub struct LegalHoldArgs {
    #[command(subcommand)]
    pub command: LegalHoldCommands,
}

#[derive(Subcommand, Debug)]
pub enum LegalHoldCommands {
    /// Show legal-hold status.
    Info(LegalHoldObjectArg),
    /// Set legal hold to ON.
    Set(LegalHoldObjectArg),
    /// Set legal hold to OFF.
    Clear(LegalHoldObjectArg),
}

#[derive(Args, Debug)]
pub struct LegalHoldObjectArg {
    /// Object path in alias/bucket/key form.
    pub path: String,
    /// Read or mutate one exact object version.
    #[arg(long, alias = "vid")]
    pub version_id: Option<String>,
}

pub async fn execute(args: LegalHoldArgs, output_config: OutputConfig) -> ExitCode {
    match args.command {
        LegalHoldCommands::Info(args) => execute_info(args, output_config).await,
        LegalHoldCommands::Set(args) => {
            execute_mutation(args, LegalHoldStatus::On, "legal_hold_set", output_config).await
        }
        LegalHoldCommands::Clear(args) => {
            execute_mutation(
                args,
                LegalHoldStatus::Off,
                "legal_hold_clear",
                output_config,
            )
            .await
        }
    }
}

async fn execute_info(args: LegalHoldObjectArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (path, options) = match parse_target(args, &formatter) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let client = match setup_client(&path.alias, &formatter, LEGAL_HOLD_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client.get_object_legal_hold(&path, &options).await {
        Ok(status) => emit_lock_output(
            &formatter,
            LocksData::one(
                "legal_hold_info",
                false,
                LockStateOutput::legal_hold(&path, options.version_id, status.is_on()),
            ),
        ),
        Err(error) => fail_core_lock(
            &formatter,
            &error,
            "Failed to get object legal hold",
            LEGAL_HOLD_CAPABILITY,
        ),
    }
}

async fn execute_mutation(
    args: LegalHoldObjectArg,
    status: LegalHoldStatus,
    operation: &'static str,
    output_config: OutputConfig,
) -> ExitCode {
    let formatter = Formatter::new(output_config);
    let (path, options) = match parse_target(args, &formatter) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let client = match setup_client(&path.alias, &formatter, LEGAL_HOLD_CAPABILITY).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    if let Err(error) = client.put_object_legal_hold(&path, status, &options).await {
        return fail_core_lock(
            &formatter,
            &error,
            "Failed to update object legal hold",
            LEGAL_HOLD_CAPABILITY,
        );
    }
    emit_lock_output(
        &formatter,
        LocksData::one(
            operation,
            true,
            LockStateOutput::legal_hold(&path, options.version_id, status.is_on()),
        ),
    )
}

fn parse_target(
    args: LegalHoldObjectArg,
    formatter: &Formatter,
) -> Result<(rc_core::RemotePath, ObjectLockOptions), ExitCode> {
    let path = parse_object_path(&args.path).map_err(|error| {
        fail_lock(
            formatter,
            ExitCode::UsageError,
            &error.to_string(),
            LEGAL_HOLD_CAPABILITY,
        )
    })?;
    let options = ObjectLockOptions::new(args.version_id, false).map_err(|error| {
        fail_lock(
            formatter,
            ExitCode::UsageError,
            &error.to_string(),
            LEGAL_HOLD_CAPABILITY,
        )
    })?;
    Ok((path, options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_hold_target_requires_object_and_non_empty_version() {
        let formatter = Formatter::default();
        assert!(
            parse_target(
                LegalHoldObjectArg {
                    path: "local/bucket".to_string(),
                    version_id: None,
                },
                &formatter,
            )
            .is_err()
        );
        assert!(
            parse_target(
                LegalHoldObjectArg {
                    path: "local/bucket/key".to_string(),
                    version_id: Some(String::new()),
                },
                &formatter,
            )
            .is_err()
        );
    }
}
