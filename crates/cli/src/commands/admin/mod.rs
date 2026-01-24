//! Admin commands for IAM and cluster management
//!
//! This module provides commands for managing users, policies, groups,
//! service accounts, and cluster operations on RustFS/MinIO-compatible servers.

mod group;
mod heal;
mod info;
mod policy;
mod service_account;
mod user;

use clap::Subcommand;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};
use rc_core::AliasManager;
use rc_s3::AdminClient;

/// Admin subcommands for IAM and cluster management
#[derive(Subcommand, Debug)]
pub enum AdminCommands {
    /// Display cluster information (servers, disks, usage)
    #[command(subcommand)]
    Info(info::InfoCommands),

    /// Manage cluster healing operations
    #[command(subcommand)]
    Heal(heal::HealCommands),

    /// Manage IAM users
    #[command(subcommand)]
    User(user::UserCommands),

    /// Manage IAM policies
    #[command(subcommand)]
    Policy(policy::PolicyCommands),

    /// Manage IAM groups
    #[command(subcommand)]
    Group(group::GroupCommands),

    /// Manage service accounts
    #[command(name = "service-account", subcommand)]
    ServiceAccount(service_account::ServiceAccountCommands),
}

/// Execute an admin subcommand
pub async fn execute(cmd: AdminCommands, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    match cmd {
        AdminCommands::Info(info_cmd) => info::execute(info_cmd, &formatter).await,
        AdminCommands::Heal(heal_cmd) => heal::execute(heal_cmd, &formatter).await,
        AdminCommands::User(user_cmd) => user::execute(user_cmd, &formatter).await,
        AdminCommands::Policy(policy_cmd) => policy::execute(policy_cmd, &formatter).await,
        AdminCommands::Group(group_cmd) => group::execute(group_cmd, &formatter).await,
        AdminCommands::ServiceAccount(sa_cmd) => service_account::execute(sa_cmd, &formatter).await,
    }
}

/// Helper to get AdminClient from an alias name
pub fn get_admin_client(alias_name: &str, formatter: &Formatter) -> Result<AdminClient, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return Err(ExitCode::GeneralError);
        }
    };

    let alias = match alias_manager.get(alias_name) {
        Ok(a) => a,
        Err(rc_core::Error::AliasNotFound(_)) => {
            formatter.error(&format!("Alias '{}' not found", alias_name));
            return Err(ExitCode::NotFound);
        }
        Err(e) => {
            formatter.error(&format!("Failed to get alias: {e}"));
            return Err(ExitCode::GeneralError);
        }
    };

    match AdminClient::new(&alias) {
        Ok(client) => Ok(client),
        Err(e) => {
            formatter.error(&format!("Failed to create admin client: {e}"));
            Err(ExitCode::GeneralError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: AdminCommands,
    }

    #[test]
    fn test_parse_admin_info_disk_options() {
        let cli = TestCli::parse_from(["rc", "info", "disk", "local", "--offline", "--healing"]);

        match cli.command {
            AdminCommands::Info(info::InfoCommands::Disk(args)) => {
                assert_eq!(args.alias, "local");
                assert!(args.offline);
                assert!(args.healing);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_heal_start_options() {
        let cli = TestCli::parse_from([
            "rc",
            "heal",
            "start",
            "local",
            "--bucket",
            "mybucket",
            "--prefix",
            "logs/",
            "--scan-mode",
            "deep",
            "--remove",
            "--recreate",
            "--dry-run",
        ]);

        match cli.command {
            AdminCommands::Heal(heal::HealCommands::Start(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.bucket.as_deref(), Some("mybucket"));
                assert_eq!(args.prefix.as_deref(), Some("logs/"));
                assert_eq!(args.scan_mode, "deep");
                assert!(args.remove);
                assert!(args.recreate);
                assert!(args.dry_run);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }
}
