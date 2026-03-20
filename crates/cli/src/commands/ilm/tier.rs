//! ilm tier command - Manage remote storage tiers
//!
//! Add, edit, list, inspect, and remove remote storage tiers used by lifecycle transition rules.

use clap::{Args, Subcommand};
use comfy_table::{ContentArrangement, Table};
use rc_core::admin::{AdminApi, TierConfig, TierCreds, TierType};
use serde::Serialize;

use crate::exit_code::ExitCode;
use crate::output::{Formatter, OutputConfig};

use super::super::admin::get_admin_client;

#[derive(Subcommand, Debug)]
pub enum TierCommands {
    /// Add a new remote storage tier
    Add(AddTierArgs),

    /// Edit tier credentials
    Edit(EditTierArgs),

    /// List configured storage tiers
    List(AliasArg),

    /// Show tier statistics
    Info(TierNameArg),

    /// Remove a storage tier
    Remove(RemoveTierArgs),
}

#[derive(Args, Debug)]
pub struct AliasArg {
    /// Alias name
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct AddTierArgs {
    /// Tier type (s3, rustfs, minio, aliyun, tencent, huaweicloud, azure, gcs, r2)
    pub tier_type: String,

    /// Tier name (uppercase identifier, e.g. WARM, COLD)
    pub tier_name: String,

    /// Alias name
    pub alias: String,

    /// Remote endpoint URL
    #[arg(long)]
    pub endpoint: String,

    /// Access key for the remote backend
    #[arg(long)]
    pub access_key: String,

    /// Secret key for the remote backend
    #[arg(long)]
    pub secret_key: String,

    /// Target bucket on the remote backend
    #[arg(long)]
    pub bucket: String,

    /// Object key prefix on the remote bucket
    #[arg(long, default_value = "")]
    pub prefix: String,

    /// Region of the remote backend
    #[arg(long, default_value = "")]
    pub region: String,

    /// Storage class on the remote backend (for S3/Azure/GCS tiers)
    #[arg(long, default_value = "")]
    pub storage_class: String,
}

#[derive(Args, Debug)]
pub struct EditTierArgs {
    /// Tier name to update
    pub tier_name: String,

    /// Alias name
    pub alias: String,

    /// New access key
    #[arg(long)]
    pub access_key: String,

    /// New secret key
    #[arg(long)]
    pub secret_key: String,
}

#[derive(Args, Debug)]
pub struct TierNameArg {
    /// Tier name to inspect
    pub tier_name: String,

    /// Alias name
    pub alias: String,
}

#[derive(Args, Debug)]
pub struct RemoveTierArgs {
    /// Tier name to remove
    pub tier_name: String,

    /// Alias name
    pub alias: String,

    /// Force removal even if tier is in use
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct TierListOutput {
    tiers: Vec<TierConfig>,
}

#[derive(Debug, Serialize)]
struct TierOperationOutput {
    tier_name: String,
    action: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TierInfoOutput {
    tier_name: String,
    config: TierConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<serde_json::Value>,
}

/// Execute a tier subcommand
pub async fn execute(cmd: TierCommands, output_config: OutputConfig) -> ExitCode {
    match cmd {
        TierCommands::Add(args) => execute_add(args, output_config).await,
        TierCommands::Edit(args) => execute_edit(args, output_config).await,
        TierCommands::List(args) => execute_list(args, output_config).await,
        TierCommands::Info(args) => execute_info(args, output_config).await,
        TierCommands::Remove(args) => execute_remove(args, output_config).await,
    }
}

async fn execute_add(args: AddTierArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let tier_type: TierType = match args.tier_type.parse() {
        Ok(tt) => tt,
        Err(error) => {
            formatter.error(&error);
            return ExitCode::UsageError;
        }
    };

    let client = match get_admin_client(&args.alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let config = build_tier_config(&TierConfigParams {
        tier_type,
        name: &args.tier_name,
        endpoint: &args.endpoint,
        access_key: &args.access_key,
        secret_key: &args.secret_key,
        bucket: &args.bucket,
        prefix: &args.prefix,
        region: &args.region,
        storage_class: &args.storage_class,
    });

    match client.add_tier(config).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&TierOperationOutput {
                    tier_name: args.tier_name,
                    action: "added".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Tier '{}' ({}) added successfully.",
                    args.tier_name, tier_type
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to add tier: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_edit(args: EditTierArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let client = match get_admin_client(&args.alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let creds = TierCreds {
        access_key: args.access_key,
        secret_key: args.secret_key,
    };

    match client.edit_tier(&args.tier_name, creds).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&TierOperationOutput {
                    tier_name: args.tier_name,
                    action: "edited".to_string(),
                });
            } else {
                formatter.success(&format!(
                    "Tier '{}' credentials updated successfully.",
                    args.tier_name
                ));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to edit tier: {error}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_list(args: AliasArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let client = match get_admin_client(&args.alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let tiers = match client.list_tiers().await {
        Ok(tiers) => tiers,
        Err(error) => {
            formatter.error(&format!("Failed to list tiers: {error}"));
            return ExitCode::GeneralError;
        }
    };

    if formatter.is_json() {
        formatter.json(&TierListOutput { tiers });
        return ExitCode::Success;
    }

    if tiers.is_empty() {
        formatter.println("No storage tiers configured.");
        return ExitCode::Success;
    }

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        "Name", "Type", "Endpoint", "Bucket", "Prefix", "Region",
    ]);

    for tier in &tiers {
        table.add_row(vec![
            tier.tier_name(),
            &tier.tier_type.to_string(),
            tier.endpoint(),
            tier.bucket(),
            tier.prefix(),
            tier.region(),
        ]);
    }

    formatter.println(&table.to_string());
    ExitCode::Success
}

async fn execute_info(args: TierNameArg, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let client = match get_admin_client(&args.alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    let tiers = match client.list_tiers().await {
        Ok(tiers) => tiers,
        Err(error) => {
            formatter.error(&format!("Failed to list tiers: {error}"));
            return ExitCode::GeneralError;
        }
    };

    let Some(config) = find_tier_config(&tiers, &args.tier_name).cloned() else {
        formatter.error(&format!("Tier '{}' not found", args.tier_name));
        return ExitCode::NotFound;
    };

    let tier_name = config.tier_name().to_string();
    let stats = match client.tier_stats().await {
        Ok(stats) => stats.get(&tier_name).cloned(),
        Err(_) => None,
    };

    if formatter.is_json() {
        formatter.json(&TierInfoOutput {
            tier_name,
            config,
            stats,
        });
    } else {
        formatter.println(&format!("Tier: {}", config.tier_name()));
        formatter.println(&format!("Type: {}", config.tier_type));
        formatter.println(&format!("Endpoint: {}", config.endpoint()));
        formatter.println(&format!("Bucket: {}", config.bucket()));
        formatter.println(&format!("Prefix: {}", display_or_dash(config.prefix())));
        formatter.println(&format!("Region: {}", display_or_dash(config.region())));
        if let Some(stats) = stats {
            formatter.println("Stats:");
            match serde_json::to_string_pretty(&stats) {
                Ok(pretty) => formatter.println(&pretty),
                Err(error) => {
                    formatter.error(&format!("Failed to format tier stats: {error}"));
                    return ExitCode::GeneralError;
                }
            }
        }
    }

    ExitCode::Success
}

fn find_tier_config<'a>(tiers: &'a [TierConfig], tier_name: &str) -> Option<&'a TierConfig> {
    tiers
        .iter()
        .find(|tier| tier.tier_name().eq_ignore_ascii_case(tier_name))
}

fn display_or_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

async fn execute_remove(args: RemoveTierArgs, output_config: OutputConfig) -> ExitCode {
    let formatter = Formatter::new(output_config);

    let client = match get_admin_client(&args.alias, &formatter) {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.remove_tier(&args.tier_name, args.force).await {
        Ok(()) => {
            if formatter.is_json() {
                formatter.json(&TierOperationOutput {
                    tier_name: args.tier_name,
                    action: "removed".to_string(),
                });
            } else {
                formatter.success(&format!("Tier '{}' removed successfully.", args.tier_name));
            }
            ExitCode::Success
        }
        Err(error) => {
            formatter.error(&format!("Failed to remove tier: {error}"));
            ExitCode::GeneralError
        }
    }
}

// ==================== Helpers ====================

struct TierConfigParams<'a> {
    tier_type: TierType,
    name: &'a str,
    endpoint: &'a str,
    access_key: &'a str,
    secret_key: &'a str,
    bucket: &'a str,
    prefix: &'a str,
    region: &'a str,
    storage_class: &'a str,
}

fn build_tier_config(params: &TierConfigParams<'_>) -> TierConfig {
    let mut config = TierConfig {
        tier_type: params.tier_type,
        name: params.name.to_string(),
        ..Default::default()
    };

    match params.tier_type {
        TierType::S3 => {
            config.s3 = Some(rc_core::admin::TierS3 {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
                storage_class: params.storage_class.to_string(),
            });
        }
        TierType::RustFS => {
            config.rustfs = Some(rc_core::admin::TierRustFS {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
                storage_class: params.storage_class.to_string(),
            });
        }
        TierType::MinIO => {
            config.minio = Some(rc_core::admin::TierMinIO {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
            });
        }
        TierType::Aliyun => {
            config.aliyun = Some(rc_core::admin::TierAliyun {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
            });
        }
        TierType::Tencent => {
            config.tencent = Some(rc_core::admin::TierTencent {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
            });
        }
        TierType::Huaweicloud => {
            config.huaweicloud = Some(rc_core::admin::TierHuaweicloud {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
            });
        }
        TierType::Azure => {
            config.azure = Some(rc_core::admin::TierAzure {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
                storage_class: params.storage_class.to_string(),
            });
        }
        TierType::GCS => {
            config.gcs = Some(rc_core::admin::TierGCS {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                creds: String::new(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
                storage_class: params.storage_class.to_string(),
            });
        }
        TierType::R2 => {
            config.r2 = Some(rc_core::admin::TierR2 {
                name: params.name.to_string(),
                endpoint: params.endpoint.to_string(),
                access_key: params.access_key.to_string(),
                secret_key: params.secret_key.to_string(),
                bucket: params.bucket.to_string(),
                prefix: params.prefix.to_string(),
                region: params.region.to_string(),
            });
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_type_parsing() {
        assert_eq!("s3".parse::<TierType>().unwrap(), TierType::S3);
        assert_eq!("rustfs".parse::<TierType>().unwrap(), TierType::RustFS);
        assert_eq!("minio".parse::<TierType>().unwrap(), TierType::MinIO);
        assert_eq!("azure".parse::<TierType>().unwrap(), TierType::Azure);
        assert_eq!("gcs".parse::<TierType>().unwrap(), TierType::GCS);
        assert_eq!("r2".parse::<TierType>().unwrap(), TierType::R2);
        assert!("invalid".parse::<TierType>().is_err());
    }

    #[test]
    fn test_build_tier_config_s3() {
        let config = build_tier_config(&TierConfigParams {
            tier_type: TierType::S3,
            name: "WARM",
            endpoint: "https://s3.amazonaws.com",
            access_key: "AKID",
            secret_key: "SECRET",
            bucket: "warm-bucket",
            prefix: "tier/",
            region: "us-east-1",
            storage_class: "STANDARD_IA",
        });

        assert_eq!(config.tier_type, TierType::S3);
        assert_eq!(config.tier_name(), "WARM");
        assert!(config.s3.is_some());
        let s3 = config.s3.as_ref().unwrap();
        assert_eq!(s3.endpoint, "https://s3.amazonaws.com");
        assert_eq!(s3.bucket, "warm-bucket");
        assert_eq!(s3.prefix, "tier/");
        assert_eq!(s3.region, "us-east-1");
        assert_eq!(s3.storage_class, "STANDARD_IA");
    }

    #[test]
    fn test_build_tier_config_rustfs() {
        let config = build_tier_config(&TierConfigParams {
            tier_type: TierType::RustFS,
            name: "ARCHIVE",
            endpoint: "http://remote:9000",
            access_key: "admin",
            secret_key: "password",
            bucket: "archive-bucket",
            prefix: "",
            region: "",
            storage_class: "",
        });

        assert_eq!(config.tier_type, TierType::RustFS);
        assert_eq!(config.tier_name(), "ARCHIVE");
        assert!(config.rustfs.is_some());
    }

    #[test]
    fn test_build_tier_config_minio() {
        let config = build_tier_config(&TierConfigParams {
            tier_type: TierType::MinIO,
            name: "COLD",
            endpoint: "http://minio:9000",
            access_key: "key",
            secret_key: "secret",
            bucket: "cold-bucket",
            prefix: "data/",
            region: "us-west-2",
            storage_class: "",
        });

        assert_eq!(config.tier_type, TierType::MinIO);
        assert!(config.minio.is_some());
        assert!(config.s3.is_none());
    }

    #[test]
    fn test_find_tier_config_matches_case_insensitively() {
        let warm = build_tier_config(&TierConfigParams {
            tier_type: TierType::RustFS,
            name: "WARM",
            endpoint: "http://remote:9000",
            access_key: "admin",
            secret_key: "password",
            bucket: "archive-bucket",
            prefix: "",
            region: "",
            storage_class: "",
        });
        let cold = build_tier_config(&TierConfigParams {
            tier_type: TierType::MinIO,
            name: "COLD",
            endpoint: "http://minio:9000",
            access_key: "key",
            secret_key: "secret",
            bucket: "cold-bucket",
            prefix: "data/",
            region: "us-west-2",
            storage_class: "",
        });

        let tiers = vec![warm, cold];
        let matched = find_tier_config(&tiers, "warm").expect("tier should exist");
        assert_eq!(matched.tier_name(), "WARM");
    }

    #[test]
    fn test_find_tier_config_returns_none_when_missing() {
        let tiers = vec![build_tier_config(&TierConfigParams {
            tier_type: TierType::RustFS,
            name: "WARM",
            endpoint: "http://remote:9000",
            access_key: "admin",
            secret_key: "password",
            bucket: "archive-bucket",
            prefix: "",
            region: "",
            storage_class: "",
        })];

        assert!(find_tier_config(&tiers, "COLD").is_none());
    }

    #[tokio::test]
    async fn test_execute_add_invalid_tier_type_returns_usage_error() {
        let args = AddTierArgs {
            tier_type: "invalid".to_string(),
            tier_name: "WARM".to_string(),
            alias: "local".to_string(),
            endpoint: "https://example.com".to_string(),
            access_key: "key".to_string(),
            secret_key: "secret".to_string(),
            bucket: "bucket".to_string(),
            prefix: String::new(),
            region: String::new(),
            storage_class: String::new(),
        };

        let code = execute_add(args, OutputConfig::default()).await;
        assert_eq!(code, ExitCode::UsageError);
    }
}
