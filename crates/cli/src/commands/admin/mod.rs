//! Admin commands for IAM and cluster management
//!
//! This module provides commands for managing users, policies, groups,
//! service accounts, and cluster operations through the RustFS Admin API.

mod access_key;
mod capabilities;
mod config;
mod decommission;
mod diagnostics;
mod expand;
mod group;
mod heal;
mod idp;
mod ilm;
mod info;
mod kms;
mod metrics;
mod policy;
mod pool;
mod rebalance;
mod replicate;
mod scanner;
mod service;
mod service_account;
mod user;

use clap::Subcommand;
use rc_core::Error;
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};
use rc_core::{Alias, AliasManager};
use rc_s3::AdminClient;

/// Admin subcommands for IAM and cluster management
#[derive(Subcommand, Debug)]
pub enum AdminCommands {
    /// Discover effective RustFS runtime capabilities
    Capabilities(capabilities::CapabilitiesArgs),

    /// Read bounded snapshots or run explicitly confirmed RustFS diagnostic probes
    #[command(subcommand)]
    Diagnostics(diagnostics::DiagnosticsCommands),

    /// Manage RustFS server configuration
    #[command(subcommand)]
    Config(config::ConfigCommands),

    /// Query bounded RustFS realtime metrics
    Metrics(metrics::MetricsArgs),

    /// Inspect KMS state and manage key lifecycle operations
    #[command(subcommand)]
    Kms(kms::KmsCommands),

    /// Inspect scanner health and freshness
    #[command(subcommand)]
    Scanner(scanner::ScannerCommands),

    /// Manage lifecycle transition operations
    #[command(subcommand)]
    Ilm(ilm::IlmCommands),

    /// Inspect, validate, and manage identity-provider configuration
    #[command(subcommand)]
    Idp(idp::IdpCommands),

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

    /// Control the server process (restart, stop, freeze, unfreeze)
    #[command(subcommand)]
    Service(service::ServiceCommands),

    /// Manage site replication across clusters
    #[command(subcommand)]
    Replicate(replicate::ReplicateCommands),
}

/// Execute an admin subcommand
pub async fn execute(cmd: AdminCommands, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    match cmd {
        AdminCommands::Capabilities(args) => capabilities::execute(args, &formatter).await,
        AdminCommands::Diagnostics(command) => diagnostics::execute(command, &formatter).await,
        AdminCommands::Config(config_cmd) => config::execute(config_cmd, &formatter).await,
        AdminCommands::Metrics(args) => metrics::execute(args, &formatter).await,
        AdminCommands::Kms(kms_cmd) => kms::execute(kms_cmd, &formatter).await,
        AdminCommands::Scanner(scanner_cmd) => scanner::execute(scanner_cmd, &formatter).await,
        AdminCommands::Ilm(ilm_cmd) => ilm::execute(ilm_cmd, &formatter).await,
        AdminCommands::Idp(idp_cmd) => idp::execute(idp_cmd, &formatter).await,
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
        AdminCommands::Service(service_cmd) => service::execute(service_cmd, &formatter).await,
        AdminCommands::Replicate(replicate_cmd) => {
            replicate::execute(replicate_cmd, &formatter).await
        }
    }
}

#[derive(Debug, Serialize)]
struct AdminV3ErrorEnvelope<'a> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'a str,
    status: &'static str,
    error: AdminV3Error<'a>,
}

#[derive(Debug, Serialize)]
struct AdminV3Error<'a> {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<&'a str>,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

fn emit_observability_error(
    output_type: &str,
    capability: &str,
    context: &str,
    error: &Error,
    formatter: &Formatter,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    let message = format!("{context}: {error}");
    if formatter.is_json() {
        let unsupported = matches!(error, Error::UnsupportedFeature(_));
        formatter.json_error(&AdminV3ErrorEnvelope {
            schema_version: 3,
            output_type,
            status: "error",
            error: AdminV3Error {
                error_type: observability_error_type(error),
                message,
                retryable: matches!(error, Error::Network(_)),
                capability: unsupported.then_some(capability),
                server: None,
                suggestion: observability_error_suggestion(error),
            },
        });
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn observability_error_type(error: &Error) -> &'static str {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => "usage_error",
        Error::Network(_) => "network_error",
        Error::Auth(_) => "auth_error",
        Error::NotFound(_) | Error::AliasNotFound(_) => "not_found",
        Error::Conflict(_) | Error::AliasExists(_) => "conflict",
        Error::UnsupportedFeature(_) => "unsupported_feature",
        Error::Interrupted(_) => "interrupted",
        _ => "general_error",
    }
}

fn observability_error_suggestion(error: &Error) -> Option<&'static str> {
    match error {
        Error::InvalidPath(_) | Error::Config(_) => Some("Review the command arguments and retry."),
        Error::Network(_) => Some("Verify the endpoint and network connectivity, then retry."),
        Error::Auth(_) => Some("Verify credentials and required admin permissions, then retry."),
        Error::UnsupportedFeature(_) => Some("Upgrade RustFS to beta.10 or later."),
        Error::Interrupted(_) => Some("Retry with --yes if deletion is still intended."),
        _ => None,
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
    let alias = get_admin_alias(alias_name, formatter)?;

    match AdminClient::new(&alias) {
        Ok(client) => Ok(client),
        Err(e) => {
            formatter.error(&format!("Failed to create admin client: {e}"));
            Err(ExitCode::GeneralError)
        }
    }
}

/// Resolve an admin alias for commands that need both Admin and S3 adapters.
pub fn get_admin_alias(alias_name: &str, formatter: &Formatter) -> Result<Alias, ExitCode> {
    let alias_lookup_name = normalize_admin_alias(alias_name);

    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return Err(ExitCode::GeneralError);
        }
    };

    match alias_manager.get(alias_lookup_name) {
        Ok(a) => Ok(a),
        Err(rc_core::Error::AliasNotFound(_)) => {
            formatter.error(&format!("Alias '{}' not found", alias_name));
            Err(ExitCode::NotFound)
        }
        Err(e) => {
            formatter.error(&format!("Failed to get alias: {e}"));
            Err(ExitCode::GeneralError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
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
    fn test_parse_admin_capabilities_refresh() {
        let cli = TestCli::parse_from(["rc", "capabilities", "local", "--refresh"]);

        match cli.command {
            AdminCommands::Capabilities(args) => {
                assert_eq!(args.alias, "local");
                assert!(args.refresh);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_ilm_transition_run() {
        let cli = TestCli::parse_from([
            "rc",
            "ilm",
            "transition",
            "run",
            "local",
            "photos",
            "--prefix",
            "logs/",
            "--tier",
            "COLDTIER",
            "--dry-run",
            "--max-objects",
            "25",
            "--max-duration-seconds",
            "30",
        ]);

        match cli.command {
            AdminCommands::Ilm(ilm::IlmCommands::Transition(ilm::TransitionCommands::Run(
                args,
            ))) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.bucket, "photos");
                assert_eq!(args.prefix, "logs/");
                assert_eq!(args.tier.as_deref(), Some("COLDTIER"));
                assert!(args.dry_run);
                assert_eq!(args.max_objects, 25);
                assert_eq!(args.max_duration_seconds, Some(30));
            }
            _ => panic!("Unexpected ILM transition run command"),
        }
    }

    #[test]
    fn test_parse_admin_kms_commands() {
        let status = TestCli::parse_from(["rc", "kms", "status", "local"]);
        match status.command {
            AdminCommands::Kms(kms::KmsCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
            }
            _ => panic!("Unexpected KMS status command"),
        }

        let roundtrip = TestCli::parse_from([
            "rc",
            "kms",
            "roundtrip",
            "local",
            "diagnostic-bucket",
            "--key-id",
            "archive-key",
            "--yes",
        ]);
        match roundtrip.command {
            AdminCommands::Kms(kms::KmsCommands::Roundtrip(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.bucket, "diagnostic-bucket");
                assert_eq!(args.key_id.as_deref(), Some("archive-key"));
                assert!(args.yes);
            }
            _ => panic!("Unexpected KMS roundtrip command"),
        }

        let list = TestCli::parse_from([
            "rc", "kms", "key", "list", "local", "--limit", "25", "--marker", "next/key",
        ]);
        match list.command {
            AdminCommands::Kms(kms::KmsCommands::Key(kms::KmsKeyCommands::List(args))) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.limit, 25);
                assert_eq!(args.marker.as_deref(), Some("next/key"));
            }
            _ => panic!("Unexpected KMS key list command"),
        }

        let key_status =
            TestCli::parse_from(["rc", "kms", "key", "status", "local", "archive/key"]);
        match key_status.command {
            AdminCommands::Kms(kms::KmsCommands::Key(kms::KmsKeyCommands::Status(args))) => {
                assert_eq!(args.key_id.as_deref(), Some("archive/key"));
            }
            _ => panic!("Unexpected KMS key status command"),
        }

        let create = TestCli::parse_from([
            "rc",
            "kms",
            "key",
            "create",
            "local",
            "--name",
            "archive",
            "--description",
            "Archive key",
            "--tag",
            "environment=prod",
        ]);
        match create.command {
            AdminCommands::Kms(kms::KmsCommands::Key(kms::KmsKeyCommands::Create(args))) => {
                assert_eq!(args.name.as_deref(), Some("archive"));
                assert_eq!(args.tags, vec!["environment=prod"]);
            }
            _ => panic!("Unexpected KMS key create command"),
        }

        let delete = TestCli::parse_from([
            "rc",
            "kms",
            "key",
            "delete",
            "local",
            "archive/key",
            "--immediate",
            "--yes",
            "--confirm-immediate",
        ]);
        match delete.command {
            AdminCommands::Kms(kms::KmsCommands::Key(kms::KmsKeyCommands::Delete(args))) => {
                assert!(args.immediate);
                assert!(args.yes);
                assert!(args.confirm_immediate);
            }
            _ => panic!("Unexpected KMS key delete command"),
        }

        let cancel = TestCli::parse_from([
            "rc",
            "kms",
            "key",
            "cancel-deletion",
            "local",
            "archive/key",
        ]);
        match cancel.command {
            AdminCommands::Kms(kms::KmsCommands::Key(kms::KmsKeyCommands::CancelDeletion(
                args,
            ))) => {
                assert_eq!(args.key_id, "archive/key");
            }
            _ => panic!("Unexpected KMS key cancel-deletion command"),
        }

        let configure = TestCli::parse_from([
            "rc",
            "kms",
            "configure",
            "local",
            "--config-file",
            "/secure/kms.json",
        ]);
        match configure.command {
            AdminCommands::Kms(kms::KmsCommands::Configure(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(
                    args.config_file.as_deref(),
                    Some(std::path::Path::new("/secure/kms.json"))
                );
                assert!(!args.stdin);
            }
            _ => panic!("Unexpected KMS configure command"),
        }

        let reconfigure = TestCli::parse_from(["rc", "kms", "reconfigure", "local", "--stdin"]);
        match reconfigure.command {
            AdminCommands::Kms(kms::KmsCommands::Reconfigure(args)) => {
                assert!(args.stdin);
                assert!(args.config_file.is_none());
            }
            _ => panic!("Unexpected KMS reconfigure command"),
        }

        let restart = TestCli::parse_from(["rc", "kms", "restart", "local", "--yes"]);
        match restart.command {
            AdminCommands::Kms(kms::KmsCommands::Restart(args)) => {
                assert!(args.yes);
            }
            _ => panic!("Unexpected KMS restart command"),
        }
    }

    #[test]
    fn test_parse_admin_diagnostics_commands() {
        for (name, expected) in [
            ("health", "health"),
            ("cluster", "cluster"),
            ("extensions", "extensions"),
        ] {
            let cli = TestCli::parse_from(["rc", "diagnostics", name, "local"]);
            match cli.command {
                AdminCommands::Diagnostics(command) => {
                    assert_eq!(command.name(), expected);
                    assert_eq!(command.alias(), "local");
                }
                _ => panic!("Unexpected command parsing result"),
            }
        }
    }

    #[test]
    fn test_parse_admin_diagnostics_client_devnull_options() {
        let cli = TestCli::parse_from([
            "rc",
            "diagnostics",
            "client-devnull",
            "local",
            "--size",
            "16MiB",
            "--timeout",
            "45s",
            "--concurrency",
            "2",
            "--yes",
        ]);

        match cli.command {
            AdminCommands::Diagnostics(diagnostics::DiagnosticsCommands::ClientDevnull(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.size, "16MiB");
                assert_eq!(args.timeout, "45s");
                assert_eq!(args.concurrency, 2);
                assert!(args.yes);
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_diagnostics_client_devnull_requires_confirmation() {
        let error = TestCli::try_parse_from(["rc", "diagnostics", "client-devnull", "local"])
            .expect_err("client devnull must require --yes");

        assert!(error.to_string().contains("--yes"));
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
    fn test_parse_admin_heal_status_task_options() {
        let cli = TestCli::parse_from([
            "rc",
            "heal",
            "status",
            "local",
            "--bucket",
            "mybucket",
            "--prefix",
            "logs/",
            "--client-token",
            "heal-token-123",
        ]);

        match cli.command {
            AdminCommands::Heal(heal::HealCommands::Status(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.bucket.as_deref(), Some("mybucket"));
                assert_eq!(args.prefix.as_deref(), Some("logs/"));
                assert_eq!(args.client_token.as_deref(), Some("heal-token-123"));
            }
            _ => panic!("Unexpected command parsing result"),
        }
    }

    #[test]
    fn test_parse_admin_heal_stop_task_options() {
        let cli = TestCli::parse_from([
            "rc",
            "heal",
            "stop",
            "local",
            "--bucket",
            "mybucket",
            "--client-token",
            "heal-token-123",
        ]);

        match cli.command {
            AdminCommands::Heal(heal::HealCommands::Stop(args)) => {
                assert_eq!(args.alias, "local");
                assert_eq!(args.bucket.as_deref(), Some("mybucket"));
                assert_eq!(args.prefix.as_deref(), None);
                assert_eq!(args.client_token.as_deref(), Some("heal-token-123"));
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
