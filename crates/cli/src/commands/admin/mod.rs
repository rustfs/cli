//! Admin commands for IAM and cluster management
//!
//! This module provides commands for managing users, policies, groups,
//! service accounts, and cluster operations on RustFS/MinIO-compatible servers.

mod access_key;
mod decommission;
mod expand;
mod group;
mod heal;
mod info;
mod policy;
mod pool;
mod rebalance;
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

    /// Manage server pools and expansion status
    #[command(subcommand)]
    Pool(pool::PoolCommands),

    /// Manage post-expansion data rebalancing
    #[command(alias = "scale", subcommand)]
    Expand(expand::ExpandCommands),

    /// Manage server pool decommissioning
    #[command(alias = "decom", subcommand)]
    Decommission(decommission::DecommissionCommands),

    /// Manage post-expansion rebalancing
    #[command(subcommand)]
    Rebalance(rebalance::RebalanceCommands),

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

    /// Inspect access key identities
    #[command(name = "access-key", subcommand)]
    AccessKey(access_key::AccessKeyCommands),
}

/// Execute an admin subcommand
pub async fn execute(cmd: AdminCommands, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    match cmd {
        AdminCommands::Info(info_cmd) => info::execute(info_cmd, &formatter).await,
        AdminCommands::Heal(heal_cmd) => heal::execute(heal_cmd, &formatter).await,
        AdminCommands::Pool(pool_cmd) => pool::execute(pool_cmd, &formatter).await,
        AdminCommands::Expand(expand_cmd) => expand::execute(expand_cmd, &formatter).await,
        AdminCommands::Decommission(decommission_cmd) => {
            decommission::execute(decommission_cmd, &formatter).await
        }
        AdminCommands::Rebalance(rebalance_cmd) => {
            rebalance::execute(rebalance_cmd, &formatter).await
        }
        AdminCommands::User(user_cmd) => user::execute(user_cmd, &formatter).await,
        AdminCommands::Policy(policy_cmd) => policy::execute(policy_cmd, &formatter).await,
        AdminCommands::Group(group_cmd) => group::execute(group_cmd, &formatter).await,
        AdminCommands::ServiceAccount(sa_cmd) => service_account::execute(sa_cmd, &formatter).await,
        AdminCommands::AccessKey(access_key_cmd) => {
            access_key::execute(access_key_cmd, &formatter).await
        }
    }
}

fn normalize_admin_alias(alias_name: &str) -> &str {
    let normalized_alias = alias_name.trim_end_matches('/');
    if normalized_alias.is_empty() {
        alias_name
    } else {
        normalized_alias
    }
}

/// Helper to get AdminClient from an alias name
pub fn get_admin_client(alias_name: &str, formatter: &Formatter) -> Result<AdminClient, ExitCode> {
    let alias_lookup_name = normalize_admin_alias(alias_name);

    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return Err(ExitCode::GeneralError);
        }
    };

    let alias = match alias_manager.get(alias_lookup_name) {
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
    fn test_parse_admin_access_key_info() {
        let cli =
            TestCli::parse_from(["rc", "access-key", "info", "local", "AKIAIOSFODNN7EXAMPLE"]);

        match cli.command {
            AdminCommands::AccessKey(access_key::AccessKeyCommands::Info(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.access_key, "AKIAIOSFODNN7EXAMPLE");
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

    #[test]
    fn test_parse_admin_pool_status_by_id() {
        let cli = TestCli::parse_from(["rc", "pool", "status", "local", "1", "--by-id"]);

        match cli.command {
            AdminCommands::Pool(pool::PoolCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.pool.as_deref(), Some("1"));
                assert!(args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_pool_list_and_status_without_pool() {
        let cli = TestCli::parse_from(["rc", "pool", "list", "local"]);

        match cli.command {
            AdminCommands::Pool(pool::PoolCommands::List(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }

        let cli = TestCli::parse_from(["rc", "pool", "status", "local"]);

        match cli.command {
            AdminCommands::Pool(pool::PoolCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
                assert!(args.pool.is_none());
                assert!(!args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_expand_commands() {
        let cli = TestCli::parse_from(["rc", "expand", "start", "local"]);

        match cli.command {
            AdminCommands::Expand(expand::ExpandCommands::Start(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }

        let cli = TestCli::parse_from(["rc", "expand", "status", "local"]);

        match cli.command {
            AdminCommands::Expand(expand::ExpandCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }

        let cli = TestCli::parse_from(["rc", "expand", "stop", "local"]);

        match cli.command {
            AdminCommands::Expand(expand::ExpandCommands::Stop(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_expand_alias() {
        let cli = TestCli::parse_from(["rc", "scale", "status", "local"]);

        match cli.command {
            AdminCommands::Expand(expand::ExpandCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_decommission_start_by_id() {
        let cli = TestCli::parse_from(["rc", "decommission", "start", "local", "1", "--by-id"]);

        match cli.command {
            AdminCommands::Decommission(decommission::DecommissionCommands::Start(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.pool, "1");
                assert!(args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_decommission_status_variants() {
        let cli = TestCli::parse_from(["rc", "decommission", "status", "local"]);

        match cli.command {
            AdminCommands::Decommission(decommission::DecommissionCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
                assert!(args.pool.is_none());
                assert!(!args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }

        let cli = TestCli::parse_from(["rc", "decommission", "status", "local", "1", "--by-id"]);

        match cli.command {
            AdminCommands::Decommission(decommission::DecommissionCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.pool.as_deref(), Some("1"));
                assert!(args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_decommission_alias() {
        let cli = TestCli::parse_from(["rc", "decom", "cancel", "local", "1", "--by-id"]);

        match cli.command {
            AdminCommands::Decommission(decommission::DecommissionCommands::Cancel(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.pool, "1");
                assert!(args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_decommission_clear_by_id() {
        let cli = TestCli::parse_from(["rc", "decommission", "clear", "local", "3", "--by-id"]);

        match cli.command {
            AdminCommands::Decommission(decommission::DecommissionCommands::Clear(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.pool, "3");
                assert!(args.by_id);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_rebalance_start() {
        let cli = TestCli::parse_from(["rc", "rebalance", "start", "local"]);

        match cli.command {
            AdminCommands::Rebalance(rebalance::RebalanceCommands::Start(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_rebalance_status_and_stop() {
        let cli = TestCli::parse_from(["rc", "rebalance", "status", "local"]);

        match cli.command {
            AdminCommands::Rebalance(rebalance::RebalanceCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }

        let cli = TestCli::parse_from(["rc", "rebalance", "stop", "local"]);

        match cli.command {
            AdminCommands::Rebalance(rebalance::RebalanceCommands::Stop(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_normalize_admin_alias_trailing_slash() {
        assert_eq!(normalize_admin_alias("local/"), "local");
    }

    #[test]
    fn test_normalize_admin_alias_without_trailing_slash() {
        assert_eq!(normalize_admin_alias("local"), "local");
    }

    #[test]
    fn test_normalize_admin_alias_only_slash() {
        assert_eq!(normalize_admin_alias("/"), "/");
    }
}
