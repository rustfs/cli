//! Site replication commands (`rc admin replicate`).
//!
//! Mirrors `mc admin replicate` against the RustFS native admin API
//! (`/rustfs/admin/v3/site-replication/*`). Peer sites are given as
//! configured alias names; their endpoints and credentials are resolved
//! from the local alias store.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde::Serialize;
use serde_json::Value;

use super::{get_admin_client, normalize_admin_alias};
use crate::exit_code::ExitCode;
use crate::output::Formatter;
use rc_core::admin::{
    AdminApi, PeerSiteSpec, ReplicateEditStatus, SiteRemoveSpec, SiteReplicationInfo,
    SiteReplicationPeer, SiteReplicationResyncOperation, SiteReplicationResyncStatus,
    SiteStatusOptions, validate_site_replication_ca_bundle,
};
use rc_core::{AliasManager, Error};
use rc_s3::AdminClient;

/// Site replication subcommands
#[derive(Subcommand, Debug)]
pub enum ReplicateCommands {
    /// Add sites (given as alias names) to a site replication cluster
    Add(AddArgs),

    /// Show site replication configuration
    Info(InfoArgs),

    /// Safely edit one exact site replication peer
    Edit(EditArgs),

    /// Manage persisted site resync operation snapshots
    Resync(ResyncArgs),

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
pub struct EditArgs {
    /// Alias name of the server
    pub alias: String,

    /// Exact deployment ID or unique exact site name
    #[arg(long)]
    pub site: String,

    /// Replace the peer endpoint with a safe HTTP(S) origin
    #[arg(long)]
    pub endpoint: Option<String>,

    /// Rename the selected peer
    #[arg(long)]
    pub name: Option<String>,

    /// Disable TLS certificate verification for the peer
    #[arg(long, conflicts_with_all = ["verify_tls", "ca_cert"])]
    pub skip_tls_verify: bool,

    /// Enable TLS certificate verification for the peer
    #[arg(long, conflicts_with = "skip_tls_verify")]
    pub verify_tls: bool,

    /// Replace the peer custom CA with a certificate-only PEM bundle
    #[arg(long, value_name = "FILE", conflicts_with = "clear_ca_cert")]
    pub ca_cert: Option<PathBuf>,

    /// Clear the peer custom CA
    #[arg(long, conflicts_with = "ca_cert")]
    pub clear_ca_cert: bool,

    /// Confirm the read-modify-write edit
    #[arg(long)]
    pub yes: bool,
}

#[derive(clap::Args, Debug)]
pub struct ResyncArgs {
    #[command(subcommand)]
    pub command: ResyncCommands,
}

#[derive(Subcommand, Debug)]
pub enum ResyncCommands {
    /// Start resync for one exact peer and retain the mutation snapshot
    Start(ResyncMutationArgs),

    /// Read the persisted snapshot from the last start or cancel request
    Status(ResyncStatusArgs),

    /// Cancel resync for one exact peer and retain the mutation snapshot
    Cancel(ResyncMutationArgs),
}

#[derive(clap::Args, Debug)]
pub struct ResyncMutationArgs {
    /// Alias name of the server
    pub alias: String,

    /// Exact deployment ID or unique exact site name
    #[arg(long)]
    pub site: String,

    /// Confirm the resync mutation
    #[arg(long)]
    pub yes: bool,
}

#[derive(clap::Args, Debug)]
pub struct ResyncStatusArgs {
    /// Alias name of the server
    pub alias: String,

    /// Exact deployment ID or unique exact site name
    #[arg(long)]
    pub site: String,
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
        ReplicateCommands::Edit(args) => execute_edit(args, formatter).await,
        ReplicateCommands::Resync(args) => execute_resync(args, formatter).await,
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
    let client = match load_admin_client(&args.alias) {
        Ok(c) => c,
        Err(error) => return emit_admin_error(formatter, &error, "site_replication_info", false),
    };

    match client.site_replication_info().await {
        Ok(info) => {
            let safe_info = SafeSiteReplicationInfo::from(&info);
            if formatter.is_json() {
                formatter.json(&admin_success_output(
                    "site_replication_info",
                    info_resource(&info, &args.alias),
                    false,
                    safe_info,
                ));
            } else {
                if !info.enabled {
                    formatter.println("Site replication is not configured.");
                } else {
                    if !info.name.is_empty() {
                        formatter
                            .println(&format!("Cluster: {}", formatter.sanitize_text(&info.name)));
                    }
                    formatter.println("Sites:");
                    for site in &info.sites {
                        let name = formatter.sanitize_text(site.name().unwrap_or("-"));
                        let endpoint = formatter.sanitize_text(site.endpoint().unwrap_or("-"));
                        formatter.println(&format!("  {name}  {endpoint}"));
                    }
                }
            }
            ExitCode::Success
        }
        Err(e) => emit_admin_error(formatter, &e, "site_replication_info", false),
    }
}

async fn execute_edit(args: EditArgs, formatter: &Formatter) -> ExitCode {
    if !args.yes {
        return emit_admin_message_error(
            formatter,
            ExitCode::UsageError,
            "site_replication_edit",
            true,
            "Site replication edit requires --yes confirmation".to_string(),
        );
    }
    if args.endpoint.is_none()
        && args.name.is_none()
        && !args.skip_tls_verify
        && !args.verify_tls
        && args.ca_cert.is_none()
        && !args.clear_ca_cert
    {
        return emit_admin_message_error(
            formatter,
            ExitCode::UsageError,
            "site_replication_edit",
            true,
            "Specify at least one edit option".to_string(),
        );
    }

    let endpoint = match args
        .endpoint
        .as_deref()
        .map(validate_site_endpoint)
        .transpose()
    {
        Ok(endpoint) => endpoint,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    let name = match args.name.as_deref().map(validate_site_name).transpose() {
        Ok(name) => name,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    let ca_cert_pem = match args.ca_cert.as_deref().map(read_ca_certificate).transpose() {
        Ok(ca_cert_pem) => ca_cert_pem,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };

    let client = match load_admin_client(&args.alias) {
        Ok(client) => client,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    let info = match client.site_replication_info().await {
        Ok(info) => info,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    let original = match info.resolve_peer(&args.site) {
        Ok(peer) => peer,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    let original_semantics = match peer_semantics(original) {
        Ok(semantics) => semantics,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    let mut edited = original.clone();
    if let Some(endpoint) = endpoint {
        edited.set_endpoint(endpoint);
    }
    if let Some(name) = name {
        edited.set_name(name);
    }
    if args.skip_tls_verify {
        edited.set_skip_tls_verify(true);
        edited.set_ca_cert_pem(String::new());
    } else if args.verify_tls {
        edited.set_skip_tls_verify(false);
    }
    if let Some(ca_cert_pem) = ca_cert_pem {
        edited.set_skip_tls_verify(false);
        edited.set_ca_cert_pem(ca_cert_pem);
    } else if args.clear_ca_cert {
        edited.set_ca_cert_pem(String::new());
    }

    let edited_semantics = match peer_semantics(&edited) {
        Ok(semantics) => semantics,
        Err(error) => {
            return emit_admin_error(formatter, &error, "site_replication_edit", true);
        }
    };
    if edited_semantics.endpoint.starts_with("http://")
        && (edited_semantics.skip_tls_verify || !edited_semantics.ca_cert_pem.is_empty())
    {
        return emit_admin_message_error(
            formatter,
            ExitCode::UsageError,
            "site_replication_edit",
            true,
            "An HTTP site endpoint cannot retain skipTlsVerify=true or a custom CA".to_string(),
        );
    }

    if edited_semantics == original_semantics {
        return emit_admin_message_error(
            formatter,
            ExitCode::UsageError,
            "site_replication_edit",
            true,
            "The requested options do not change the selected site".to_string(),
        );
    }

    match client.site_replication_edit(&edited).await {
        Ok(status) => {
            let resource = edited
                .deployment_id()
                .unwrap_or(args.site.as_str())
                .to_string();
            let result = SafeSiteReplicationEditResult::new(&edited, &status);
            if formatter.is_json() {
                formatter.json(&admin_success_output(
                    "site_replication_edit",
                    resource,
                    true,
                    result,
                ));
            } else {
                formatter.success(&format!(
                    "Updated site replication peer {}.",
                    formatter.sanitize_text(edited.name().unwrap_or(&args.site))
                ));
            }
            ExitCode::Success
        }
        Err(error) => emit_admin_error(
            formatter,
            &error,
            "site_replication_edit",
            mutation_was_attempted(true, &error),
        ),
    }
}

async fn execute_resync(args: ResyncArgs, formatter: &Formatter) -> ExitCode {
    match args.command {
        ResyncCommands::Start(args) => {
            execute_resync_mutation(args, SiteReplicationResyncOperation::Start, formatter).await
        }
        ResyncCommands::Status(args) => execute_resync_status(args, formatter).await,
        ResyncCommands::Cancel(args) => {
            execute_resync_mutation(args, SiteReplicationResyncOperation::Cancel, formatter).await
        }
    }
}

async fn execute_resync_mutation(
    args: ResyncMutationArgs,
    operation: SiteReplicationResyncOperation,
    formatter: &Formatter,
) -> ExitCode {
    let operation_name = resync_operation_name(operation);
    if !args.yes {
        return emit_admin_message_error(
            formatter,
            ExitCode::UsageError,
            operation_name,
            true,
            format!(
                "Site replication resync {} requires --yes confirmation",
                operation.as_str()
            ),
        );
    }
    execute_resync_request(&args.alias, &args.site, operation, formatter).await
}

async fn execute_resync_status(args: ResyncStatusArgs, formatter: &Formatter) -> ExitCode {
    execute_resync_request(
        &args.alias,
        &args.site,
        SiteReplicationResyncOperation::Status,
        formatter,
    )
    .await
}

async fn execute_resync_request(
    alias_name: &str,
    selector: &str,
    operation: SiteReplicationResyncOperation,
    formatter: &Formatter,
) -> ExitCode {
    let operation_name = resync_operation_name(operation);
    let mutation = operation.is_mutation();
    if selector.trim().is_empty() {
        return emit_admin_message_error(
            formatter,
            ExitCode::UsageError,
            operation_name,
            false,
            "Site selector must not be empty".to_string(),
        );
    }
    let (client, local_endpoint) = match load_admin_client_with_endpoint(alias_name) {
        Ok(context) => context,
        Err(error) => {
            return emit_admin_error(formatter, &error, operation_name, false);
        }
    };
    let info = match client.site_replication_info().await {
        Ok(info) => info,
        Err(error) => {
            return emit_admin_error(formatter, &error, operation_name, false);
        }
    };
    let selected = match info.resolve_peer(selector) {
        Ok(peer) => peer,
        Err(error) => {
            return emit_admin_error(formatter, &error, operation_name, false);
        }
    };
    let endpoint = match selected
        .endpoint()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .map(validate_site_endpoint)
        .transpose()
    {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) | Err(_) => {
            return emit_admin_message_error(
                formatter,
                ExitCode::Conflict,
                operation_name,
                false,
                "Selected site has no valid endpoint".to_string(),
            );
        }
    };
    if mutation && validate_site_endpoint(&local_endpoint).is_ok_and(|local| local == endpoint) {
        return emit_admin_message_error(
            formatter,
            ExitCode::Conflict,
            operation_name,
            false,
            "Cannot resync a site replication peer to itself".to_string(),
        );
    }

    // RustFS expects the complete PeerInfo object. In legacy snapshots the
    // endpoint may be stored as `endpoints`; inserting the singular field lets
    // current servers consume it while all opaque fields remain intact.
    let mut peer = selected.clone();
    peer.set_endpoint(endpoint);
    let resource = selected
        .deployment_id()
        .filter(|deployment_id| !deployment_id.trim().is_empty())
        .unwrap_or_else(|| peer.endpoint().unwrap_or(selector))
        .to_string();
    match client.site_replication_resync(operation, &peer).await {
        Ok(status) => emit_resync_status(formatter, operation, operation_name, resource, status),
        Err(error) => {
            let mutation_attempted = mutation_was_attempted(mutation, &error);
            emit_admin_error(formatter, &error, operation_name, mutation_attempted)
        }
    }
}

fn mutation_was_attempted(mutation: bool, error: &Error) -> bool {
    mutation && !matches!(error, Error::RequestRejected(_))
}

const fn resync_operation_name(operation: SiteReplicationResyncOperation) -> &'static str {
    match operation {
        SiteReplicationResyncOperation::Start => "site_replication_resync_start",
        SiteReplicationResyncOperation::Status => "site_replication_resync_status",
        SiteReplicationResyncOperation::Cancel => "site_replication_resync_cancel",
    }
}

fn emit_resync_status(
    formatter: &Formatter,
    requested: SiteReplicationResyncOperation,
    operation_name: &'static str,
    resource: String,
    status: SiteReplicationResyncStatus,
) -> ExitCode {
    if requested == SiteReplicationResyncOperation::Status && status.is_not_found() {
        return emit_admin_message_error(
            formatter,
            ExitCode::Conflict,
            operation_name,
            false,
            "No persisted resync operation snapshot exists for the selected site".to_string(),
        );
    }

    let failed = status.has_failure();
    let state = if requested == SiteReplicationResyncOperation::Status {
        "unknown"
    } else if failed {
        "failed"
    } else {
        "succeeded"
    };
    let operation_id = (!status.resync_id.is_empty()).then(|| status.resync_id.clone());
    let result = SafeSiteReplicationResyncResult::new(requested, &status);
    if formatter.is_json() {
        formatter.json(&admin_operation_output(
            operation_name,
            resource,
            state,
            operation_id,
            requested.is_mutation(),
            result,
        ));
    } else {
        let snapshot = if requested == SiteReplicationResyncOperation::Status {
            "persisted last-operation snapshot"
        } else {
            "mutation response snapshot"
        };
        formatter.println(&format!(
            "Site resync {snapshot} for {}: operation={}, status={}, lifecycle=unknown",
            formatter.sanitize_text(&resource),
            formatter.sanitize_text(&status.operation),
            formatter.sanitize_text(&status.status)
        ));
        if !status.resync_id.is_empty() {
            formatter.println(&format!(
                "  Operation ID: {}",
                formatter.sanitize_text(&status.resync_id)
            ));
        }
        if !status.error_detail.is_empty() {
            formatter.println(&format!(
                "  Error detail: {}",
                formatter.sanitize_text(&status.error_detail)
            ));
        }
        for bucket in &status.buckets {
            formatter.println(&format!(
                "  {}  {}{}",
                formatter.sanitize_text(&bucket.bucket),
                formatter.sanitize_text(&bucket.status),
                if bucket.error_detail.is_empty() {
                    String::new()
                } else {
                    format!("  {}", formatter.sanitize_text(&bucket.error_detail))
                }
            ));
        }
    }
    if failed {
        ExitCode::GeneralError
    } else {
        ExitCode::Success
    }
}

fn load_admin_client(alias_name: &str) -> Result<AdminClient, Error> {
    let alias_manager = AliasManager::new()?;
    let alias = alias_manager.get(normalize_admin_alias(alias_name))?;
    AdminClient::new(&alias)
}

fn load_admin_client_with_endpoint(alias_name: &str) -> Result<(AdminClient, String), Error> {
    let alias_manager = AliasManager::new()?;
    let alias = alias_manager.get(normalize_admin_alias(alias_name))?;
    let endpoint = alias.endpoint.clone();
    Ok((AdminClient::new(&alias)?, endpoint))
}

fn validate_site_endpoint(endpoint: &str) -> Result<String, Error> {
    let url = url::Url::parse(endpoint)
        .map_err(|_| Error::InvalidPath("Site endpoint must be a valid URL".to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(Error::InvalidPath(
            "Site endpoint must use HTTP or HTTPS and include a host".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::InvalidPath(
            "Site endpoint must not contain user information".to_string(),
        ));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidPath(
            "Site endpoint must be an origin without a path, query, or fragment".to_string(),
        ));
    }
    Ok(url.origin().ascii_serialization())
}

fn validate_site_name(name: &str) -> Result<String, Error> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::InvalidPath(
            "Site name must not be empty".to_string(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::InvalidPath(
            "Site name must not contain control characters".to_string(),
        ));
    }
    Ok(name.to_string())
}

#[derive(PartialEq, Eq)]
struct PeerSemantics {
    endpoint: String,
    name: String,
    deployment_id: String,
    skip_tls_verify: bool,
    ca_cert_pem: String,
}

fn peer_semantics(peer: &SiteReplicationPeer) -> Result<PeerSemantics, Error> {
    let endpoint = peer
        .endpoint()
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or_else(|| Error::General("Selected site has no endpoint".to_string()))?;
    let name = peer
        .name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::General("Selected site has no name".to_string()))?;
    let deployment_id = peer
        .deployment_id()
        .filter(|deployment_id| !deployment_id.is_empty())
        .ok_or_else(|| Error::General("Selected site has no deployment ID".to_string()))?;
    let endpoint = validate_site_endpoint(endpoint)
        .map_err(|_| Error::General("Selected site has an invalid endpoint".to_string()))?;

    Ok(PeerSemantics {
        endpoint,
        name: name.to_string(),
        deployment_id: deployment_id.to_string(),
        skip_tls_verify: peer.skip_tls_verify().unwrap_or(false),
        ca_cert_pem: canonical_ca_for_comparison(peer.ca_cert_pem().unwrap_or_default()),
    })
}

fn canonical_ca_for_comparison(pem: &str) -> String {
    pem.split_ascii_whitespace().collect::<String>()
}

fn read_ca_certificate(path: &Path) -> Result<String, Error> {
    let file = std::fs::File::open(path).map_err(|error| {
        Error::InvalidPath(format!("Failed to open CA certificate file: {error}"))
    })?;
    let mut bytes = Vec::new();
    file.take((rc_core::admin::MAX_SITE_REPLICATION_CA_CERT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            Error::InvalidPath(format!("Failed to read CA certificate file: {error}"))
        })?;
    if bytes.len() > rc_core::admin::MAX_SITE_REPLICATION_CA_CERT_BYTES {
        return Err(Error::InvalidPath(format!(
            "CA certificate bundle exceeds the {} byte limit",
            rc_core::admin::MAX_SITE_REPLICATION_CA_CERT_BYTES
        )));
    }
    validate_ca_certificate_bytes(&bytes)?;
    String::from_utf8(bytes)
        .map_err(|_| Error::InvalidPath("CA certificate bundle must be UTF-8 PEM".to_string()))
}

fn validate_ca_certificate_bytes(bytes: &[u8]) -> Result<(), Error> {
    validate_site_replication_ca_bundle(bytes)
}

#[derive(Serialize)]
struct AdminOperationsSuccess<T: Serialize> {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    data: AdminOperationsData<T>,
}

#[derive(Serialize)]
struct AdminOperationsData<T: Serialize> {
    operations: Vec<AdminOperation<T>>,
}

#[derive(Serialize)]
struct AdminOperation<T: Serialize> {
    operation: &'static str,
    resource: String,
    state: &'static str,
    operation_id: Option<String>,
    changed: bool,
    result: T,
}

fn admin_success_output<T: Serialize>(
    operation: &'static str,
    resource: String,
    changed: bool,
    result: T,
) -> AdminOperationsSuccess<T> {
    admin_operation_output(operation, resource, "succeeded", None, changed, result)
}

fn admin_operation_output<T: Serialize>(
    operation: &'static str,
    resource: String,
    state: &'static str,
    operation_id: Option<String>,
    changed: bool,
    result: T,
) -> AdminOperationsSuccess<T> {
    AdminOperationsSuccess {
        schema_version: 3,
        output_type: "admin_operations",
        status: "success",
        data: AdminOperationsData {
            operations: vec![AdminOperation {
                operation,
                resource,
                state,
                operation_id,
                changed,
                result,
            }],
        },
    }
}

#[derive(Serialize)]
struct SafeSiteReplicationInfo {
    enabled: bool,
    name: String,
    sites: Vec<SafeSiteReplicationPeer>,
    #[serde(rename = "apiVersion", skip_serializing_if = "String::is_empty")]
    api_version: String,
}

impl From<&SiteReplicationInfo> for SafeSiteReplicationInfo {
    fn from(info: &SiteReplicationInfo) -> Self {
        Self {
            enabled: info.enabled,
            name: info.name.clone(),
            sites: info
                .sites
                .iter()
                .map(SafeSiteReplicationPeer::from)
                .collect(),
            api_version: info.api_version.clone(),
        }
    }
}

#[derive(Serialize)]
struct SafeSiteReplicationPeer {
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(rename = "deploymentID", skip_serializing_if = "Option::is_none")]
    deployment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync: Option<String>,
    #[serde(rename = "defaultbandwidth", skip_serializing_if = "Option::is_none")]
    default_bandwidth: Option<SafeSiteReplicationBandwidth>,
    #[serde(
        rename = "replicate-ilm-expiry",
        skip_serializing_if = "Option::is_none"
    )]
    replicate_ilm_expiry: Option<bool>,
    #[serde(rename = "objectNamingMode", skip_serializing_if = "Option::is_none")]
    object_naming_mode: Option<String>,
    #[serde(rename = "skipTlsVerify", skip_serializing_if = "Option::is_none")]
    skip_tls_verify: Option<bool>,
    #[serde(rename = "hasCustomCA")]
    has_custom_ca: bool,
    #[serde(rename = "apiVersion", skip_serializing_if = "Option::is_none")]
    api_version: Option<String>,
}

impl From<&SiteReplicationPeer> for SafeSiteReplicationPeer {
    fn from(peer: &SiteReplicationPeer) -> Self {
        Self {
            endpoint: peer.endpoint().map(str::to_string),
            name: peer.name().map(str::to_string),
            deployment_id: peer.deployment_id().map(str::to_string),
            sync: peer.sync().map(str::to_string),
            default_bandwidth: peer
                .default_bandwidth()
                .and_then(SafeSiteReplicationBandwidth::from_value),
            replicate_ilm_expiry: peer.replicate_ilm_expiry(),
            object_naming_mode: peer.object_naming_mode().map(str::to_string),
            skip_tls_verify: peer.skip_tls_verify(),
            has_custom_ca: peer.has_custom_ca(),
            api_version: peer.api_version().map(str::to_string),
        }
    }
}

#[derive(Serialize)]
struct SafeSiteReplicationBandwidth {
    #[serde(
        rename = "bandwidthLimitPerBucket",
        skip_serializing_if = "Option::is_none"
    )]
    bandwidth_limit_per_bucket: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    set: Option<bool>,
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

impl SafeSiteReplicationBandwidth {
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            bandwidth_limit_per_bucket: object
                .get("bandwidthLimitPerBucket")
                .and_then(Value::as_u64),
            set: object.get("set").and_then(Value::as_bool),
            updated_at: object
                .get("updatedAt")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

#[derive(Serialize)]
struct SafeSiteReplicationEditResult {
    site: String,
    endpoint: String,
    verify_tls: bool,
    has_custom_ca: bool,
}

impl SafeSiteReplicationEditResult {
    fn new(peer: &SiteReplicationPeer, _status: &ReplicateEditStatus) -> Self {
        Self {
            site: peer.name().unwrap_or_default().to_string(),
            endpoint: peer.endpoint().unwrap_or_default().to_string(),
            verify_tls: !peer.skip_tls_verify().unwrap_or(false),
            has_custom_ca: peer.has_custom_ca(),
        }
    }
}

#[derive(Serialize)]
struct SafeSiteReplicationResyncResult {
    snapshot_kind: &'static str,
    lifecycle_state: &'static str,
    server_operation: String,
    server_status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    error_detail: String,
    buckets: Vec<SafeSiteReplicationResyncBucket>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    future: BTreeMap<String, Value>,
}

impl SafeSiteReplicationResyncResult {
    fn new(
        requested: SiteReplicationResyncOperation,
        status: &SiteReplicationResyncStatus,
    ) -> Self {
        Self {
            snapshot_kind: if requested == SiteReplicationResyncOperation::Status {
                "persisted_last_operation"
            } else {
                "mutation_response"
            },
            lifecycle_state: "unknown",
            server_operation: status.operation.clone(),
            server_status: status.status.clone(),
            error_detail: status.error_detail.clone(),
            buckets: status
                .buckets
                .iter()
                .map(SafeSiteReplicationResyncBucket::from)
                .collect(),
            future: safe_resync_future_fields(&status.extensions),
        }
    }
}

#[derive(Serialize)]
struct SafeSiteReplicationResyncBucket {
    bucket: String,
    status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    error_detail: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    future: BTreeMap<String, Value>,
}

impl From<&rc_core::admin::SiteReplicationResyncBucketStatus> for SafeSiteReplicationResyncBucket {
    fn from(bucket: &rc_core::admin::SiteReplicationResyncBucketStatus) -> Self {
        Self {
            bucket: bucket.bucket.clone(),
            status: bucket.status.clone(),
            error_detail: bucket.error_detail.clone(),
            future: safe_resync_future_fields(&bucket.extensions),
        }
    }
}

fn safe_resync_future_fields(fields: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    fields
        .iter()
        .filter(|(name, _)| safe_resync_future_field_name(name))
        .filter_map(|(name, value)| {
            safe_resync_future_value(value).map(|value| (name.clone(), value))
        })
        .collect()
}

fn safe_resync_future_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Some(value.clone()),
        Value::Array(values) => Some(Value::Array(
            values.iter().filter_map(safe_resync_future_value).collect(),
        )),
        Value::Object(fields) => Some(Value::Object(
            fields
                .iter()
                .filter(|(name, _)| safe_resync_future_field_name(name))
                .filter_map(|(name, value)| {
                    safe_resync_future_value(value).map(|value| (name.clone(), value))
                })
                .collect(),
        )),
    }
}

fn safe_resync_future_field_name(name: &str) -> bool {
    matches!(
        name,
        "createdAt"
            | "startedAt"
            | "updatedAt"
            | "completedAt"
            | "generation"
            | "progress"
            | "total"
            | "completed"
            | "failed"
            | "pending"
            | "state"
            | "phase"
    )
}

fn info_resource(info: &SiteReplicationInfo, alias: &str) -> String {
    if info.name.is_empty() {
        alias.to_string()
    } else {
        info.name.clone()
    }
}

#[derive(Serialize)]
struct AdminOperationsError {
    schema_version: u8,
    #[serde(rename = "type")]
    output_type: &'static str,
    status: &'static str,
    error: AdminErrorOutput,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AdminErrorOutput {
    Unsupported(UnsupportedAdminError),
    Standard(StandardAdminError),
}

#[derive(Serialize)]
struct UnsupportedAdminError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    capability: &'static str,
    server: Option<String>,
    suggestion: Option<&'static str>,
}

#[derive(Serialize)]
struct StandardAdminError {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
    retryable: bool,
    suggestion: Option<&'static str>,
}

fn emit_admin_error(
    formatter: &Formatter,
    error: &Error,
    operation: &'static str,
    mutation: bool,
) -> ExitCode {
    let code = ExitCode::from_i32(error.exit_code()).unwrap_or(ExitCode::GeneralError);
    emit_admin_message_error(formatter, code, operation, mutation, error.to_string())
}

fn emit_admin_message_error(
    formatter: &Formatter,
    code: ExitCode,
    operation: &'static str,
    mutation: bool,
    message: String,
) -> ExitCode {
    let message = formatter.sanitize_text(&message);
    if formatter.is_json() {
        formatter.json_error(&admin_error_output(code, operation, mutation, message));
    } else {
        formatter.error_with_code(code, &message);
    }
    code
}

fn admin_error_output(
    code: ExitCode,
    operation: &'static str,
    mutation: bool,
    message: String,
) -> AdminOperationsError {
    let error = if code == ExitCode::UnsupportedFeature {
        AdminErrorOutput::Unsupported(UnsupportedAdminError {
            error_type: "unsupported_feature",
            message,
            retryable: false,
            capability: operation,
            server: None,
            suggestion: Some("Upgrade RustFS or verify server support for this operation."),
        })
    } else {
        let (error_type, retryable, suggestion) = admin_error_metadata(code, operation, mutation);
        AdminErrorOutput::Standard(StandardAdminError {
            error_type,
            message,
            retryable,
            suggestion,
        })
    };
    AdminOperationsError {
        schema_version: 3,
        output_type: "admin_operations",
        status: "error",
        error,
    }
}

fn admin_error_metadata(
    code: ExitCode,
    operation: &str,
    mutation: bool,
) -> (&'static str, bool, Option<&'static str>) {
    let resync = operation.starts_with("site_replication_resync_");
    match code {
        ExitCode::UsageError if resync => (
            "usage_error",
            false,
            Some("Run the command with --help and verify its site selector and confirmation."),
        ),
        ExitCode::UsageError => (
            "usage_error",
            false,
            Some("Run the command with --help and verify its edit options."),
        ),
        ExitCode::NetworkError if mutation && resync => (
            "network_error",
            false,
            Some(
                "The resync mutation outcome may be unknown; inspect the persisted snapshot and storage state before retrying.",
            ),
        ),
        ExitCode::NetworkError if mutation => (
            "network_error",
            false,
            Some("The edit outcome may be unknown; inspect site replication info before retrying."),
        ),
        ExitCode::NetworkError => (
            "network_error",
            true,
            Some("Verify the endpoint and network connectivity, then retry."),
        ),
        ExitCode::AuthError => (
            "auth_error",
            false,
            Some("Verify the alias credentials and admin permissions."),
        ),
        ExitCode::NotFound => (
            "not_found",
            false,
            Some("Use an exact deployment ID or unique exact site name."),
        ),
        ExitCode::Conflict => (
            "conflict",
            false,
            Some("Resolve the ambiguous selector or pending server operation."),
        ),
        ExitCode::Interrupted => (
            "interrupted",
            false,
            Some("Inspect site replication state before retrying."),
        ),
        ExitCode::GeneralError if mutation && resync => (
            "general_error",
            false,
            Some(
                "The mutation response was rejected; inspect the persisted snapshot and storage state before retrying.",
            ),
        ),
        ExitCode::Success | ExitCode::GeneralError | ExitCode::UnsupportedFeature => {
            ("general_error", false, None)
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

#[cfg(test)]
mod tests {
    use super::*;
    use jsonschema::Validator;

    const INFO_SUCCESS_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/admin/site_replication_info_success.json");
    const EDIT_SUCCESS_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/admin/site_replication_edit_success.json");
    const EDIT_ERROR_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/admin/site_replication_edit_error.json");
    const INFO_ERROR_GOLDEN: &str =
        include_str!("../../../tests/fixtures/output_v3/admin/site_replication_info_error.json");
    const RESYNC_START_SUCCESS_GOLDEN: &str = include_str!(
        "../../../tests/fixtures/output_v3/admin/site_replication_resync_start_success.json"
    );
    const RESYNC_STATUS_PARTIAL_GOLDEN: &str = include_str!(
        "../../../tests/fixtures/output_v3/admin/site_replication_resync_status_partial.json"
    );

    #[test]
    fn edit_endpoint_must_be_a_safe_http_origin() {
        for invalid in [
            "ftp://site.example.test",
            "https://user@site.example.test",
            "https://site.example.test/path",
            "https://site.example.test?query=yes",
            "https://site.example.test#fragment",
            "https://",
        ] {
            assert!(
                validate_site_endpoint(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }

        assert!(validate_site_endpoint("http://site.example.test:9000").is_ok());
        assert!(validate_site_endpoint("https://site.example.test").is_ok());
    }

    #[test]
    fn edit_name_rejects_empty_and_control_characters() {
        assert!(validate_site_name("   ").is_err());
        assert!(validate_site_name("bad\nname").is_err());
        assert_eq!(
            validate_site_name(" renamed ").expect("valid name"),
            "renamed"
        );
    }

    #[test]
    fn peer_semantics_normalize_defaults_and_origin() {
        let original: SiteReplicationPeer = serde_json::from_str(
            r#"{"endpoint":"https://site.example.test","name":"site","deploymentID":"id"}"#,
        )
        .expect("valid peer");
        let mut explicit = original.clone();
        explicit.set_endpoint("https://site.example.test/".into());
        explicit.set_skip_tls_verify(false);
        explicit.set_ca_cert_pem(String::new());

        let original_semantics = peer_semantics(&original).expect("valid original semantics");
        let explicit_semantics = peer_semantics(&explicit).expect("valid explicit semantics");
        assert!(original_semantics == explicit_semantics);

        let certificate = include_str!("../../../../core/src/admin/test_ca.pem");
        let original_json = serde_json::json!({
            "endpoint": "https://site.example.test",
            "name": "site",
            "deploymentID": "id",
            "caCertPem": certificate,
        });
        let original: SiteReplicationPeer =
            serde_json::from_value(original_json).expect("valid peer");
        let mut whitespace_only = original.clone();
        whitespace_only.set_ca_cert_pem(format!("{certificate}\n   \n"));
        let original_semantics = peer_semantics(&original).expect("valid CA semantics");
        let whitespace_semantics =
            peer_semantics(&whitespace_only).expect("valid whitespace CA semantics");
        assert!(original_semantics == whitespace_semantics);
    }

    #[test]
    fn peer_semantics_require_edit_identity_fields() {
        for peer in [
            r#"{"name":"site","deploymentID":"id"}"#,
            r#"{"endpoint":"https://site.example.test","deploymentID":"id"}"#,
            r#"{"endpoint":"https://site.example.test","name":"site"}"#,
        ] {
            let peer: SiteReplicationPeer = serde_json::from_str(peer).expect("valid JSON object");
            assert!(peer_semantics(&peer).is_err());
        }
    }

    #[test]
    fn ca_bundle_rejects_empty_malformed_private_key_and_non_certificate_pem() {
        for invalid in [
            b"".as_slice(),
            b"not PEM".as_slice(),
            b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n".as_slice(),
            b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n".as_slice(),
            b"-----BEGIN CERTIFICATE-----\ninvalid-base64\n-----END CERTIFICATE-----\n".as_slice(),
        ] {
            assert!(
                validate_ca_certificate_bytes(invalid).is_err(),
                "invalid CA bundle must be rejected"
            );
        }
    }

    #[test]
    fn ca_bundle_rejects_256_kib_plus_one_byte() {
        let oversized = vec![b'x'; rc_core::admin::MAX_SITE_REPLICATION_CA_CERT_BYTES + 1];
        assert!(validate_ca_certificate_bytes(&oversized).is_err());
    }

    #[test]
    fn info_and_edit_outputs_match_v3_schema_and_goldens() {
        let info: SiteReplicationInfo = serde_json::from_str(
            r#"{
                "enabled":true,
                "name":"primary",
                "sites":[{
                    "endpoint":"https://secondary.example.test",
                    "name":"secondary",
                    "deploymentID":"deployment-2",
                    "sync":"future-mode",
                    "defaultbandwidth":{
                        "bandwidthLimitPerBucket":1024,
                        "set":true,
                        "updatedAt":"2026-07-22T00:00:00Z",
                        "futureSecret":"not projected"
                    },
                    "skipTlsVerify":false,
                    "caCertPem":"hidden",
                    "apiVersion":"v1",
                    "sessionToken":"not projected"
                }],
                "serviceAccountAccessKey":"not projected",
                "apiVersion":"v1"
            }"#,
        )
        .expect("valid info fixture");
        let info_success = serde_json::to_value(admin_success_output(
            "site_replication_info",
            "primary".into(),
            false,
            SafeSiteReplicationInfo::from(&info),
        ))
        .expect("info output serializes");
        let status: ReplicateEditStatus = serde_json::from_str(
            r#"{"success":true,"status":"ARBITRARY SECRET","apiVersion":"v1"}"#,
        )
        .expect("valid status fixture");
        let edit_success = serde_json::to_value(admin_success_output(
            "site_replication_edit",
            "deployment-2".into(),
            true,
            SafeSiteReplicationEditResult::new(&info.sites[0], &status),
        ))
        .expect("edit output serializes");
        let edit_error = serde_json::to_value(admin_error_output(
            ExitCode::UsageError,
            "site_replication_edit",
            true,
            "bad edit".into(),
        ))
        .expect("edit error serializes");
        let info_error = serde_json::to_value(admin_error_output(
            ExitCode::UnsupportedFeature,
            "site_replication_info",
            false,
            "unsupported".into(),
        ))
        .expect("info error serializes");

        for (actual, expected) in [
            (info_success, golden(INFO_SUCCESS_GOLDEN)),
            (edit_success, golden(EDIT_SUCCESS_GOLDEN)),
            (edit_error, golden(EDIT_ERROR_GOLDEN)),
            (info_error, golden(INFO_ERROR_GOLDEN)),
        ] {
            assert_eq!(actual, expected);
            assert_valid_v3(&actual);
        }
    }

    #[test]
    fn resync_outputs_match_v3_schema_and_drop_unsafe_future_fields() {
        let started: SiteReplicationResyncStatus = serde_json::from_value(serde_json::json!({
            "op": "start",
            "id": "resync-123",
            "status": "success",
            "buckets": [{"bucket": "photos", "status": "started"}],
            "generation": 7,
            "sessionToken": "MUST-NOT-PRINT"
        }))
        .expect("valid start snapshot");
        let start_output = serde_json::to_value(admin_operation_output(
            "site_replication_resync_start",
            "deployment-2".into(),
            "succeeded",
            Some(started.resync_id.clone()),
            true,
            SafeSiteReplicationResyncResult::new(SiteReplicationResyncOperation::Start, &started),
        ))
        .expect("start output serializes");

        let partial: SiteReplicationResyncStatus = serde_json::from_value(serde_json::json!({
            "op": "start",
            "id": "resync-partial",
            "status": "success",
            "buckets": [
                {"bucket": "photos", "status": "started", "updatedAt": "2026-07-22T00:00:00Z"},
                {"bucket": "archive", "status": "failed", "errorDetail": "target unavailable", "accessToken": "MUST-NOT-PRINT"}
            ],
            "errorDetail": "partial failure in starting site resync",
            "updatedAt": "2026-07-22T00:00:01Z",
            "secretKey": "MUST-NOT-PRINT"
        }))
        .expect("valid partial snapshot");
        let partial_output = serde_json::to_value(admin_operation_output(
            "site_replication_resync_status",
            "deployment-2".into(),
            "unknown",
            Some(partial.resync_id.clone()),
            false,
            SafeSiteReplicationResyncResult::new(SiteReplicationResyncOperation::Status, &partial),
        ))
        .expect("status output serializes");

        for (actual, expected) in [
            (start_output, golden(RESYNC_START_SUCCESS_GOLDEN)),
            (partial_output, golden(RESYNC_STATUS_PARTIAL_GOLDEN)),
        ] {
            assert_eq!(actual, expected);
            assert_valid_v3(&actual);
            assert!(!actual.to_string().contains("MUST-NOT-PRINT"));
        }
    }

    #[test]
    fn resync_local_request_rejection_is_not_reported_as_an_attempted_mutation() {
        assert!(!mutation_was_attempted(
            true,
            &Error::RequestRejected("request too large".to_string())
        ));
        assert!(mutation_was_attempted(
            true,
            &Error::Network("connection lost".to_string())
        ));
        assert!(!mutation_was_attempted(
            false,
            &Error::Network("connection lost".to_string())
        ));
    }

    fn output_v3_validator() -> Validator {
        let schema_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("schemas/output_v3.json");
        let schema = std::fs::read_to_string(&schema_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", schema_path.display()));
        let schema: Value = serde_json::from_str(&schema).expect("output v3 schema should parse");
        jsonschema::validator_for(&schema).expect("output v3 schema should compile")
    }

    fn assert_valid_v3(value: &Value) {
        let errors = output_v3_validator()
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "site replication output must satisfy output v3:\n{}",
            errors.join("\n")
        );
    }

    fn golden(contents: &str) -> Value {
        serde_json::from_str(contents).expect("site replication golden fixture should parse")
    }
}
