//! Site replication commands (`rc admin replicate`).
//!
//! Mirrors `mc admin replicate` against the RustFS native admin API
//! (`/rustfs/admin/v3/site-replication/*`). Peer sites are given as
//! configured alias names; their endpoints and credentials are resolved
//! from the local alias store.

use clap::Subcommand;

use super::get_admin_client;
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::AliasManager;
use rc_core::admin::{AdminApi, PeerSiteSpec, SiteRemoveSpec, SiteStatusOptions};

/// Site replication subcommands
#[derive(Subcommand, Debug)]
pub enum ReplicateCommands {
    /// Add sites (given as alias names) to a site replication cluster
    Add(AddArgs),

    /// Show site replication configuration
    Info(InfoArgs),

    /// Show site replication status
    Status(StatusArgs),

    /// Remove sites from the site replication cluster
    #[command(alias = "rm")]
    Remove(RemoveArgs),
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Alias names of the sites to link (first is the request target)
    #[arg(required = true, num_args = 2..)]
    pub aliases: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct InfoArgs {
    /// Alias name of the server
    pub alias: String,
}

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Alias name of the server
    pub alias: String,

    /// Include bucket replication status
    #[arg(long)]
    pub buckets: bool,

    /// Include user replication status
    #[arg(long)]
    pub users: bool,

    /// Include group replication status
    #[arg(long)]
    pub groups: bool,

    /// Include policy replication status
    #[arg(long)]
    pub policies: bool,

    /// Include replication metrics
    #[arg(long)]
    pub metrics: bool,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Alias name of the server to send the request to
    pub alias: String,

    /// Site names to remove from the cluster
    #[arg(long = "site", conflicts_with = "all")]
    pub sites: Vec<String>,

    /// Remove all sites (dissolve the cluster)
    #[arg(long)]
    pub all: bool,
}

/// Execute a site replication subcommand
pub async fn execute(cmd: ReplicateCommands, formatter: &Formatter) -> ExitCode {
    match cmd {
        ReplicateCommands::Add(args) => execute_add(args, formatter).await,
        ReplicateCommands::Info(args) => execute_info(args, formatter).await,
        ReplicateCommands::Status(args) => execute_status(args, formatter).await,
        ReplicateCommands::Remove(args) => execute_remove(args, formatter).await,
    }
}

fn resolve_peer_sites(
    aliases: &[String],
    formatter: &Formatter,
) -> Result<Vec<PeerSiteSpec>, ExitCode> {
    let alias_manager = match AliasManager::new() {
        Ok(am) => am,
        Err(e) => {
            formatter.error(&format!("Failed to load aliases: {e}"));
            return Err(ExitCode::GeneralError);
        }
    };

    let mut sites = Vec::with_capacity(aliases.len());
    for name in aliases {
        let alias = match alias_manager.get(name) {
            Ok(a) => a,
            Err(e) => {
                formatter.error(&format!("Unknown alias `{name}`: {e}"));
                return Err(ExitCode::GeneralError);
            }
        };
        if alias.anonymous || alias.access_key.is_empty() {
            formatter.error(&format!(
                "Alias `{name}` has no credentials; site replication requires credentialed aliases"
            ));
            return Err(ExitCode::GeneralError);
        }
        sites.push(PeerSiteSpec {
            name: alias.name.clone(),
            endpoint: alias.endpoint.clone(),
            access_key: alias.access_key.clone(),
            secret_key: alias.secret_key.clone(),
            skip_tls_verify: alias.insecure,
            ca_cert_pem: String::new(),
        });
    }
    Ok(sites)
}

async fn execute_add(args: AddArgs, formatter: &Formatter) -> ExitCode {
    let sites = match resolve_peer_sites(&args.aliases, formatter) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let client = match get_admin_client(&args.aliases[0], formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.site_replication_add(&sites).await {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&result);
            } else {
                formatter.success(&format!(
                    "Site replication configured across: {}",
                    args.aliases.join(", ")
                ));
                if let Some(status) = result.get("status").and_then(|v| v.as_str()) {
                    formatter.println(&format!("  Status: {status}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to add site replication: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_info(args: InfoArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    match client.site_replication_info().await {
        Ok(info) => {
            if formatter.is_json() {
                formatter.json(&info);
            } else {
                let enabled = info
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !enabled {
                    formatter.println("Site replication is not configured.");
                } else {
                    if let Some(name) = info.get("name").and_then(|v| v.as_str()) {
                        formatter.println(&format!("Cluster: {name}"));
                    }
                    if let Some(sites) = info.get("sites").and_then(|v| v.as_array()) {
                        formatter.println("Sites:");
                        for site in sites {
                            let name = site.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                            let endpoint =
                                site.get("endpoint").and_then(|v| v.as_str()).unwrap_or("-");
                            formatter.println(&format!("  {name}  {endpoint}"));
                        }
                    }
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get site replication info: {e}"));
            ExitCode::GeneralError
        }
    }
}

async fn execute_status(args: StatusArgs, formatter: &Formatter) -> ExitCode {
    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    // Default to the summary views when no specific flag is requested,
    // matching `mc admin replicate status` behavior.
    let none_requested =
        !(args.buckets || args.users || args.groups || args.policies || args.metrics);
    let options = SiteStatusOptions {
        buckets: args.buckets || none_requested,
        users: args.users || none_requested,
        groups: args.groups || none_requested,
        policies: args.policies || none_requested,
        metrics: args.metrics,
        peer_state: false,
        ilm_expiry_rules: false,
    };

    match client.site_replication_status(&options).await {
        Ok(status) => {
            if formatter.is_json() {
                formatter.json(&status);
            } else {
                print_replication_status(&status, formatter);
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to get site replication status: {e}"));
            ExitCode::GeneralError
        }
    }
}

fn print_replication_status(status: &serde_json::Value, formatter: &Formatter) {
    let enabled = status
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        formatter.println("Site replication is not configured.");
        return;
    }

    if let Some(sites) = status.get("Sites").and_then(|v| v.as_object()) {
        formatter.println("Sites:");
        for site in sites.values() {
            let name = site.get("name").and_then(|v| v.as_str()).unwrap_or("-");
            let endpoint = site.get("endpoint").and_then(|v| v.as_str()).unwrap_or("-");
            formatter.println(&format!("  {name}  {endpoint}"));
        }
    }

    let count = |key: &str| status.get(key).and_then(|v| v.as_u64()).unwrap_or_default();
    formatter.println(&format!(
        "Replicated entities: buckets={} users={} groups={} policies={}",
        count("MaxBuckets"),
        count("MaxUsers"),
        count("MaxGroups"),
        count("MaxPolicies"),
    ));

    if let Some(errors) = status.get("PeerErrors").and_then(|v| v.as_object())
        && !errors.is_empty()
    {
        formatter.println(&format!(
            "Peer errors reported by {} site(s):",
            errors.len()
        ));
        for (site, error) in errors {
            formatter.println(&format!("  {site}: {error}"));
        }
    }
}

async fn execute_remove(args: RemoveArgs, formatter: &Formatter) -> ExitCode {
    if !args.all && args.sites.is_empty() {
        formatter.error("Specify --site <name> (repeatable) or --all");
        return ExitCode::UsageError;
    }

    let client = match get_admin_client(&args.alias, formatter) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let spec = SiteRemoveSpec {
        site_names: args.sites.clone(),
        remove_all: args.all,
    };

    match client.site_replication_remove(&spec).await {
        Ok(result) => {
            if formatter.is_json() {
                formatter.json(&result);
            } else {
                formatter.success("Site replication removal requested.");
                if let Some(status) = result.get("status").and_then(|v| v.as_str()) {
                    formatter.println(&format!("  Status: {status}"));
                }
            }
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&format!("Failed to remove site replication: {e}"));
            ExitCode::GeneralError
        }
    }
}
