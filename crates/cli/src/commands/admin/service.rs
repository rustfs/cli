//! Service control commands (restart, stop, freeze, unfreeze).
//!
//! These map to `POST /rustfs/admin/v3/service?action=<action>` on the
//! server. `restart` and `stop` both trigger a graceful shutdown; a process
//! manager (systemd, k8s) is responsible for relaunching after `restart`.

use clap::Subcommand;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::AdminApi;

/// Service control subcommands
#[derive(Subcommand, Debug)]
pub enum ServiceCommands {
    /// Restart the server (graceful shutdown; process manager relaunches)
    Restart(ActionArgs),

    /// Stop the server gracefully
    Stop(ActionArgs),

    /// Set the service freeze flag (advisory)
    Freeze(ActionArgs),

    /// Clear the service freeze flag
    Unfreeze(ActionArgs),
}

#[derive(clap::Args, Debug)]
pub struct ActionArgs {
    /// Alias name of the server
    pub alias: String,
}

/// Execute a service subcommand
pub async fn execute(cmd: ServiceCommands, formatter: &Formatter) -> ExitCode {
    let (action, args) = match cmd {
        ServiceCommands::Restart(args) => ("restart", args),
        ServiceCommands::Stop(args) => ("stop", args),
        ServiceCommands::Freeze(args) => ("freeze", args),
        ServiceCommands::Unfreeze(args) => ("unfreeze", args),
    };

    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.service_action(action).await {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&result);
            } else if result.accepted {
                formatter.success(&format!(
                    "Service {action} accepted on `{}`: {}",
                    args.alias, result.message
                ));
            } else {
                formatter.error(&format!(
                    "Service {action} not accepted on `{}`: {}",
                    args.alias, result.message
                ));
                return ExitCode::GeneralError;
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!(
                "Failed to {action} service on `{}`: {e}",
                args.alias
            ));
            ExitCode::GeneralError
        }
    }
}
